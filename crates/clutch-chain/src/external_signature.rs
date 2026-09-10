//! Turning an external signer's answer into the signature this stack expects.
//!
//! Readiness items A1 and A2. An external signing service — AWS KMS with an `ECC_SECG_P256K1`
//! key is the one named in `docs/keys.md` — returns a DER-encoded `(r, s)` and nothing else.
//! `ChainSigner` has to produce `(r, s, v)` with `v` in `{27, 28}`, in the low-s form, because
//! the node identifies the signer by *recovering* the public key from the signature.
//!
//! Neither the recovery id nor the low-s normalisation comes back from such a service, so both
//! have to be derived here. Getting either wrong is the dangerous kind of wrong: the result is a
//! signature that is valid ECDSA and recovers to the WRONG address, which the node rejects as an
//! unauthorised mint. It fails at the worst moment rather than the obvious one.
//!
//! This module has no AWS dependency, on purpose. Everything difficult is here and tested against
//! the in-process signer, so the remaining `KmsSigner` is the API call and nothing else — see
//! `KMS_SIGNER_SHAPE` at the bottom for exactly what that call must ask for.

use secp256k1::ecdsa::{RecoverableSignature, RecoveryId, Signature};
use secp256k1::{Message, PublicKey, Secp256k1};
use sha3::{Digest, Keccak256};

/// The digest an external signer must be asked to sign, for a given transaction hash.
///
/// This is the stack's convention and it is not the obvious one: the digest is Keccak-256 of the
/// hex STRING's UTF-8 bytes, not of the 32 bytes that hex represents. Signing the bytes produces a
/// signature the node will not accept. Kept as its own function so a signer implementation cannot
/// quietly disagree with `EnvKeySigner` about what is being signed.
pub fn digest_for_hash_hex(hash_hex: &str) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(hash_hex.as_bytes());
    hasher.finalize().into()
}

/// The 0x address for an uncompressed (65-byte, `0x04`-prefixed) public key.
///
/// An external service returns its public key in DER SPKI form; the caller unwraps that to the
/// 65-byte point. Same derivation as `EnvKeySigner`, with a test asserting the two agree — a
/// signer whose `address()` disagrees with its own signatures is a mint that fails verification
/// for a reason no log line will name.
pub fn address_from_uncompressed(pubkey: &[u8]) -> Result<String, String> {
    if pubkey.len() != 65 || pubkey[0] != 0x04 {
        return Err(format!(
            "expected a 65-byte uncompressed public key starting 0x04, got {} bytes starting 0x{:02x}",
            pubkey.len(),
            pubkey.first().copied().unwrap_or(0)
        ));
    }
    let mut hasher = Keccak256::new();
    hasher.update(&pubkey[1..]);
    let h = hasher.finalize();
    Ok(format!("0x{}", hex::encode(&h[12..32])))
}

/// Convert a DER `(r, s)` from an external signer into `(r_hex, s_hex, v)`.
///
/// `expected_pubkey` is the signer's own 65-byte uncompressed key, and it is required rather than
/// optional: the recovery id is found by TRYING both candidates and keeping the one that recovers
/// to this key. That is deliberately a search rather than a calculation — it cannot be off by one,
/// and it doubles as a check that the signature really came from the key we believe we are signing
/// with. A service returning a signature from a different key, and a caller passing a digest other
/// than the one signed, both fail here instead of producing a valid signature for a wrong address.
///
/// `s` is normalised to the low half first, which is what the in-process signer always produces.
/// Normalising flips which recovery id is correct, which is another reason to search after
/// normalising rather than to reason about it.
pub fn recoverable_from_der(
    der: &[u8],
    digest: &[u8; 32],
    expected_pubkey: &[u8],
) -> Result<(String, String, u64), String> {
    let expected = PublicKey::from_slice(expected_pubkey)
        .map_err(|e| format!("expected_pubkey is not a valid public key: {e}"))?;

    let mut sig =
        Signature::from_der(der).map_err(|e| format!("signer returned invalid DER: {e}"))?;
    sig.normalize_s();
    let compact = sig.serialize_compact();

    let msg = Message::from_digest_slice(digest).map_err(|e| e.to_string())?;
    let secp = Secp256k1::new();

    for candidate in 0..=1i32 {
        let rec_id = RecoveryId::from_i32(candidate).map_err(|e| e.to_string())?;
        let recoverable = RecoverableSignature::from_compact(&compact, rec_id)
            .map_err(|e| format!("could not build a recoverable signature: {e}"))?;
        if let Ok(recovered) = secp.recover_ecdsa(&msg, &recoverable) {
            if recovered == expected {
                return Ok((
                    hex::encode(&compact[..32]),
                    hex::encode(&compact[32..]),
                    candidate as u64 + 27,
                ));
            }
        }
    }

    // Reaching here means no recovery id maps this signature to this key. Never guess a `v` to
    // get past it: a wrong one recovers to a different, valid-looking address.
    Err("no recovery id recovers this signature to the expected public key — the signature is \
         from a different key, or the digest signed was not the one supplied"
        .to_string())
}

/// What a `KmsSigner` still has to do, kept next to the code it depends on so the two cannot
/// drift. Not implemented here because it cannot be tested here: there is no KMS to call.
///
/// ```text
/// address():
///   1. kms.GetPublicKey(key_id) -> DER SPKI
///   2. unwrap the SubjectPublicKeyInfo to the 65-byte uncompressed point (locate the
///      0x04 byte and take 65 bytes; do not assume a fixed offset)
///   3. address_from_uncompressed(point)                      <- tested here
///   Fetch ONCE at construction and cache it. Every signature needs it for the
///   recovery-id search, and a per-signature GetPublicKey turns one mint into two API
///   round trips plus a rate-limit surface.
///
/// sign_hash_hex(hash_hex):
///   1. digest = digest_for_hash_hex(hash_hex)                <- tested here
///   2. kms.Sign(key_id, message = digest,
///               MessageType = DIGEST,     <- NOT RAW. RAW makes KMS hash again, signing
///                                            the wrong preimage. With DIGEST it does not
///                                            re-hash, which is what lets a Keccak-256
///                                            digest be signed by a service that has no
///                                            Keccak at all.
///               SigningAlgorithm = ECDSA_SHA_256)
///   3. recoverable_from_der(der, &digest, &cached_pubkey)    <- tested here
///
/// The key must be created with KeySpec = ECC_SECG_P256K1 and KeyUsage = SIGN_VERIFY, and
/// its policy must NOT grant kms:ScheduleKeyDeletion to the signing principal.
/// ```
pub const KMS_SIGNER_SHAPE: () = ();

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{ChainSigner, EnvKeySigner};
    use secp256k1::SecretKey;

    const SECRET: &str = "0883ddd3d07303b87c954b0c9383f7b78f45e002520fc03a8adc80595dbf6509";
    const HASH_HEX: &str = "ae174ce0588b221ed24d685518d486513187ab5ee649347062e75066a2aa9a37";

    fn keypair() -> (SecretKey, [u8; 65]) {
        let secret = SecretKey::from_slice(&hex::decode(SECRET).unwrap()).unwrap();
        let secp = Secp256k1::new();
        let pk = PublicKey::from_secret_key(&secp, &secret);
        (secret, pk.serialize_uncompressed())
    }

    fn der_for(secret: &SecretKey, digest: &[u8; 32]) -> Vec<u8> {
        let secp = Secp256k1::new();
        let msg = Message::from_digest_slice(digest).unwrap();
        secp.sign_ecdsa(&msg, secret).serialize_der().to_vec()
    }

    /// The equivalence that matters: given the same key and the same transaction hash, the DER
    /// path must produce the byte-identical signature the in-process signer produces. If these
    /// ever diverge, a mint signed through KMS recovers to a different address than one signed
    /// with the env key, and nothing else in the stack notices until the node refuses it.
    #[test]
    fn the_der_path_agrees_with_the_in_process_signer() {
        let (secret, pubkey) = keypair();
        let digest = digest_for_hash_hex(HASH_HEX);
        let der = der_for(&secret, &digest);

        let (r, s, v) = recoverable_from_der(&der, &digest, &pubkey).unwrap();

        let env = EnvKeySigner::from_secret_hex(SECRET).unwrap();
        let (r_env, s_env, v_env) = env.sign_hash_hex(HASH_HEX).unwrap();

        assert_eq!(r, r_env, "r must match the in-process signer");
        assert_eq!(s, s_env, "s must match the in-process signer");
        assert_eq!(v, v_env, "v must match the in-process signer");
        assert!(v == 27 || v == 28, "v must be 27 or 28, got {v}");
    }

    /// The address derivation has to agree too, or `address()` names one account while the
    /// signatures authorise another.
    #[test]
    fn the_address_agrees_with_the_in_process_signer() {
        let (_, pubkey) = keypair();
        let env = EnvKeySigner::from_secret_hex(SECRET).unwrap();
        assert_eq!(address_from_uncompressed(&pubkey).unwrap(), env.address());
    }

    /// KMS makes no low-s promise, so a high-s signature has to be normalised — and normalising
    /// flips which recovery id is correct, which is the trap this test exists for. Built by
    /// replacing `s` with `n - s`, the other valid representation of the same signature.
    #[test]
    fn a_high_s_signature_is_normalised_and_still_recovers() {
        let (secret, pubkey) = keypair();
        let digest = digest_for_hash_hex(HASH_HEX);
        let secp = Secp256k1::new();
        let msg = Message::from_digest_slice(&digest).unwrap();
        let canonical = secp.sign_ecdsa(&msg, &secret).serialize_compact();

        // n, the curve order.
        const N: [u8; 32] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C,
            0xD0, 0x36, 0x41, 0x41,
        ];
        // n - s, big-endian, byte-wise.
        let mut high_s = [0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let mut d = N[i] as i16 - canonical[32 + i] as i16 - borrow;
            if d < 0 {
                d += 256;
                borrow = 1;
            } else {
                borrow = 0;
            }
            high_s[i] = d as u8;
        }
        let mut flipped = [0u8; 64];
        flipped[..32].copy_from_slice(&canonical[..32]);
        flipped[32..].copy_from_slice(&high_s);

        let high_der = Signature::from_compact(&flipped)
            .expect("n - s is a valid s")
            .serialize_der()
            .to_vec();

        let (r, s, v) = recoverable_from_der(&high_der, &digest, &pubkey)
            .expect("a high-s signature must be accepted, not rejected");

        let env = EnvKeySigner::from_secret_hex(SECRET).unwrap();
        let (r_env, s_env, v_env) = env.sign_hash_hex(HASH_HEX).unwrap();
        assert_eq!(r, r_env);
        assert_eq!(s, s_env, "s must come back normalised to the low half");
        assert_eq!(
            v, v_env,
            "the recovery id must follow the normalisation, not precede it"
        );
    }

    /// A signature from another key must fail rather than resolve to that other key's address.
    /// This is what makes the recovery-id search a safety property and not just a convenience.
    #[test]
    fn a_signature_from_a_different_key_is_refused() {
        let (_, pubkey) = keypair();
        let digest = digest_for_hash_hex(HASH_HEX);
        let other = SecretKey::from_slice(
            &hex::decode("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap(),
        )
        .unwrap();
        let der = der_for(&other, &digest);

        let err = recoverable_from_der(&der, &digest, &pubkey)
            .expect_err("a signature from another key must not be accepted");
        assert!(err.contains("no recovery id"), "got: {err}");
    }

    /// A digest that is not the one signed must fail too. The same guard catches a caller that
    /// hashed the wrong thing, which is the easiest mistake to make given the hex-string
    /// convention.
    #[test]
    fn a_mismatched_digest_is_refused() {
        let (secret, pubkey) = keypair();
        let signed = digest_for_hash_hex(HASH_HEX);
        let other =
            digest_for_hash_hex("00000000000000000000000000000000000000000000000000000000deadbeef");
        assert_ne!(signed, other);

        let der = der_for(&secret, &signed);
        assert!(recoverable_from_der(&der, &other, &pubkey).is_err());
    }

    #[test]
    fn malformed_input_is_refused_with_a_reason() {
        let (secret, pubkey) = keypair();
        let digest = digest_for_hash_hex(HASH_HEX);

        let e = recoverable_from_der(&[0x30, 0x00], &digest, &pubkey).unwrap_err();
        assert!(e.contains("invalid DER"), "got: {e}");

        let der_ok = der_for(&secret, &digest);
        let e = recoverable_from_der(&der_ok, &digest, &[0x04; 10]).unwrap_err();
        assert!(e.contains("not a valid public key"), "got: {e}");
    }

    #[test]
    fn the_address_helper_rejects_a_wrong_shaped_key() {
        assert!(
            address_from_uncompressed(&[0x04; 64]).is_err(),
            "wrong length"
        );
        assert!(
            address_from_uncompressed(&[0x02; 65]).is_err(),
            "compressed prefix"
        );
        assert!(address_from_uncompressed(&[]).is_err(), "empty");
    }

    /// The digest convention itself: Keccak-256 of the hex STRING, not of the bytes it encodes.
    /// Signing the bytes is the mistake this pins, and it would produce a signature the node
    /// silently refuses.
    #[test]
    fn the_digest_is_over_the_hex_string_not_the_bytes() {
        let over_string = digest_for_hash_hex(HASH_HEX);
        let over_bytes: [u8; 32] = {
            let mut h = Keccak256::new();
            h.update(hex::decode(HASH_HEX).unwrap());
            h.finalize().into()
        };
        assert_ne!(
            over_string, over_bytes,
            "the two conventions must not be confused"
        );
    }
}
