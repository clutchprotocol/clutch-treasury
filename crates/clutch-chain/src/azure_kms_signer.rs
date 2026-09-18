//! `ChainSigner` backed by Azure Key Vault. Readiness items A1 (mint authority) and A2 (payout
//! signer) — see `docs/mainnet-readiness.md` and `docs/KEY-CEREMONY.md`.
//!
//! All the hard cryptography lives in `external_signature.rs` and is tested there without any
//! network. This file is deliberately thin: three HTTP calls (get a token, get the public key
//! once, sign a digest), each doing exactly what its doc comment says and nothing else.
//!
//! **Not wired into `main.rs` yet, on purpose.** Wiring it up decides how `treasury-service`
//! chooses between this and `EnvKeySigner`, and that decision belongs with the actual key
//! ceremony (`docs/KEY-CEREMONY.md` step 5), not before a real key exists to test it against. Once
//! the vault and key exist, the ceremony's own step 4 — "sign a known value and verify it end to
//! end" — is what proves this file actually works, on the real key, before anything depends on it.
//!
//! ## What the signing principal needs, and nothing more
//!
//! An Azure AD App Registration (Service Principal) with the **Key Vault Crypto User** RBAC role
//! on the vault, scoped to the vault only. Not **Crypto Officer** — that role can also delete and
//! rotate keys, which defeats the entire point of A1: the account that signs must not be able to
//! delete the key it signs with.
//!
//! ## Where Azure's shape differs from AWS KMS
//!
//! `KMS_SIGNER_SHAPE` in `external_signature.rs` documents the AWS shape this stack was designed
//! against first. Azure differs in exactly three places, each called out at its call site below:
//! the public key comes back as separate `x`/`y` fields, not a DER blob; the signed digest needs
//! no "don't re-hash this" flag because Key Vault never re-hashes; and the signature comes back as
//! raw `(r, s)`, not DER — hence `recoverable_from_compact` rather than `recoverable_from_der`.

use crate::external_signature::{
    address_from_uncompressed, digest_for_hash_hex, recoverable_from_compact,
};
use crate::signer::ChainSigner;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;

/// The Key Vault REST API version this file was written against. A query parameter, not a
/// dependency version — bumping it does not otherwise change this file.
const API_VERSION: &str = "7.4";

pub struct AzureKmsSigner {
    http: reqwest::Client,
    /// `https://login.microsoftonline.com` in `new()`. A field rather than a hardcoded literal
    /// only so tests can point it at a mock server; nothing that calls `new()` sets this itself.
    auth_base_url: String,
    tenant_id: String,
    client_id: String,
    client_secret: String,
    /// e.g. `https://my-vault.vault.azure.net` — no trailing slash.
    vault_url: String,
    key_name: String,
    /// Pinned deliberately, never "current version": Key Vault lets a key name grow new
    /// versions, and floating to whichever is current would let the mint authority's address
    /// change under this service without anyone deciding that on purpose.
    key_version: String,
    /// Fetched once at construction. A per-signature GetKey call would turn one mint into two
    /// round trips and a second rate-limit surface for no benefit — the key does not rotate
    /// under a pinned version.
    cached_pubkey: [u8; 65],
    address: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct GetKeyResponse {
    key: JsonWebKey,
}

#[derive(Deserialize)]
struct JsonWebKey {
    kty: String,
    crv: Option<String>,
    /// Base64url, unpadded. 32 bytes for a P-256K point.
    x: String,
    y: String,
}

#[derive(Deserialize)]
struct SignResponse {
    /// Base64url, unpadded. Raw `r || s`, NOT DER — see the module doc.
    value: String,
}

impl AzureKmsSigner {
    /// Fetches the public key immediately, so a misconfigured vault, key name, key version, or
    /// permission fails at startup with a clear error rather than on the first real mint.
    pub async fn new(
        tenant_id: String,
        client_id: String,
        client_secret: String,
        vault_url: String,
        key_name: String,
        key_version: String,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("could not build the Key Vault HTTP client: {e}"))?;

        let mut signer = Self {
            http,
            auth_base_url: "https://login.microsoftonline.com".to_string(),
            tenant_id,
            client_id,
            client_secret,
            vault_url,
            key_name,
            key_version,
            cached_pubkey: [0u8; 65],
            address: String::new(),
        };

        let token = signer.fetch_token().await?;
        let pubkey = signer.fetch_public_key(&token).await?;
        let address = address_from_uncompressed(&pubkey)?;

        signer.cached_pubkey = pubkey;
        signer.address = address;
        Ok(signer)
    }

    /// Azure AD's client-credentials grant. The scope is fixed — Key Vault's own resource
    /// identifier, not the vault's URL — because a token is valid for every vault the principal
    /// has a role on, not only this one.
    async fn fetch_token(&self) -> Result<String, String> {
        let url = format!(
            "{}/{}/oauth2/v2.0/token",
            self.auth_base_url, self.tenant_id
        );
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("scope", "https://vault.azure.net/.default"),
            ])
            .send()
            .await
            .map_err(|e| format!("could not reach Azure AD for a token: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Azure AD refused to issue a token ({status}): {body}"
            ));
        }

        let parsed: TokenResponse = resp
            .json()
            .await
            .map_err(|e| format!("Azure AD's token response did not parse: {e}"))?;
        Ok(parsed.access_token)
    }

    /// Difference from AWS: the public key is two plain fields, `x` and `y`, not a DER
    /// SubjectPublicKeyInfo to unwrap. The uncompressed point is `0x04 ++ x ++ y` directly.
    async fn fetch_public_key(&self, token: &str) -> Result<[u8; 65], String> {
        let url = format!(
            "{}/keys/{}/{}?api-version={API_VERSION}",
            self.vault_url, self.key_name, self.key_version
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("could not reach Key Vault for the public key: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Key Vault refused GetKey ({status}): {body}"));
        }

        let parsed: GetKeyResponse = resp
            .json()
            .await
            .map_err(|e| format!("Key Vault's GetKey response did not parse: {e}"))?;

        let crv = parsed.key.crv.unwrap_or_default();
        // A loose check on purpose: the exact string Key Vault reports for this curve was not
        // verified against a real vault while writing this, so this warns rather than hard-fails
        // on a casing or naming difference. What must not pass silently is a DIFFERENT curve
        // entirely — that produces a key of the wrong length below, which does fail hard.
        if !crv.to_ascii_lowercase().contains("256k") {
            tracing::warn!(
                "Key Vault reports this key's curve as '{crv}', which does not look like \
                 P-256K/secp256k1 — double-check the key was created with the right curve \
                 before trusting anything this signer produces."
            );
        }
        if !parsed.key.kty.starts_with("EC") {
            return Err(format!(
                "expected an EC key, Key Vault reports kty '{}' for {}/{}",
                parsed.key.kty, self.key_name, self.key_version
            ));
        }

        let x = URL_SAFE_NO_PAD
            .decode(&parsed.key.x)
            .map_err(|e| format!("key's x coordinate did not decode as base64url: {e}"))?;
        let y = URL_SAFE_NO_PAD
            .decode(&parsed.key.y)
            .map_err(|e| format!("key's y coordinate did not decode as base64url: {e}"))?;
        if x.len() != 32 || y.len() != 32 {
            return Err(format!(
                "expected 32-byte x and y for a P-256K key, got {} and {} bytes — wrong curve?",
                x.len(),
                y.len()
            ));
        }

        let mut point = [0u8; 65];
        point[0] = 0x04;
        point[1..33].copy_from_slice(&x);
        point[33..65].copy_from_slice(&y);
        Ok(point)
    }

    /// Difference from AWS: no `MessageType = DIGEST` equivalent to set. Key Vault's `sign`
    /// operation never re-hashes what it is given — there is no "raw message" mode to avoid here.
    async fn sign_digest(&self, token: &str, digest: &[u8; 32]) -> Result<[u8; 64], String> {
        let url = format!(
            "{}/keys/{}/{}/sign?api-version={API_VERSION}",
            self.vault_url, self.key_name, self.key_version
        );
        let body = serde_json::json!({
            "alg": "ES256K",
            "value": URL_SAFE_NO_PAD.encode(digest),
        });
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("could not reach Key Vault to sign: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Key Vault refused Sign ({status}): {text}"));
        }

        let parsed: SignResponse = resp
            .json()
            .await
            .map_err(|e| format!("Key Vault's Sign response did not parse: {e}"))?;

        let raw = URL_SAFE_NO_PAD
            .decode(&parsed.value)
            .map_err(|e| format!("signature did not decode as base64url: {e}"))?;
        let compact: [u8; 64] = raw.try_into().map_err(|v: Vec<u8>| {
            format!(
                "expected a 64-byte raw (r, s) signature from Key Vault, got {} bytes — if this \
                 ever fires, Key Vault is not returning the JOSE/JWS shape this file assumes",
                v.len()
            )
        })?;
        Ok(compact)
    }
}

impl ChainSigner for AzureKmsSigner {
    fn address(&self) -> String {
        self.address.clone()
    }

    /// `ChainSigner` is synchronous — `EnvKeySigner` has no reason to be async, and changing the
    /// trait ripples into every caller. `block_in_place` is tokio's documented way to run async
    /// work from inside a sync trait method without it: it hands this call's OS thread over to
    /// other tasks for the duration, rather than blocking the runtime outright. It needs the
    /// multi-threaded runtime, which this workspace already requires (`tokio` feature "full").
    ///
    /// Blocking a thread for one network round trip is deliberately acceptable here: the node
    /// accepts at most one mint per block (~20s at the standard three-authority slot time), so
    /// this is called at most once per block, never in a hot loop.
    fn sign_hash_hex(&self, hash_hex: &str) -> Result<(String, String, u64), String> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let digest = digest_for_hash_hex(hash_hex);
                let token = self.fetch_token().await?;
                let compact = self.sign_digest(&token, &digest).await?;
                recoverable_from_compact(&compact, &digest, &self.cached_pubkey)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::EnvKeySigner;
    use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "0883ddd3d07303b87c954b0c9383f7b78f45e002520fc03a8adc80595dbf6509";
    const HASH_HEX: &str = "ae174ce0588b221ed24d685518d486513187ab5ee649347062e75066a2aa9a37";

    /// `auth_base_url` and `vault_url` both point at the same mock server in every test here —
    /// wiremock tells them apart by path, exactly as two real hosts would be told apart by DNS.
    /// `tenant_id`/`client_id`/`client_secret` are sent as form fields but never checked by the
    /// mocks below, so any value proves nothing either way.
    fn test_signer(server: &MockServer, pubkey: [u8; 65], address: String) -> AzureKmsSigner {
        AzureKmsSigner {
            http: reqwest::Client::new(),
            auth_base_url: server.uri(),
            tenant_id: "test-tenant".into(),
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            vault_url: server.uri(),
            key_name: "mint-key-a".into(),
            key_version: "abc123".into(),
            cached_pubkey: pubkey,
            address,
        }
    }

    /// GetKey against a mocked Key Vault, using a real key's actual x/y — proves the JWK
    /// deserialisation, the base64url decode, and the uncompressed-point reconstruction against
    /// the same secp256k1 point `EnvKeySigner` would derive for the same secret.
    #[tokio::test]
    async fn fetch_public_key_matches_the_in_process_signer() {
        let secp = Secp256k1::new();
        let secret = SecretKey::from_slice(&hex::decode(SECRET).unwrap()).unwrap();
        let pubkey = PublicKey::from_secret_key(&secp, &secret).serialize_uncompressed();
        let (x, y) = (&pubkey[1..33], &pubkey[33..65]);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/keys/mint-key-a/abc123"))
            .and(query_param("api-version", API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "key": {
                    "kty": "EC",
                    "crv": "P-256K",
                    "x": URL_SAFE_NO_PAD.encode(x),
                    "y": URL_SAFE_NO_PAD.encode(y),
                }
            })))
            .mount(&server)
            .await;

        let signer = test_signer(&server, [0u8; 65], String::new());
        let fetched = signer.fetch_public_key("fake-token").await.unwrap();

        assert_eq!(fetched, pubkey, "reconstructed point must match the real key");
        let env = EnvKeySigner::from_secret_hex(SECRET).unwrap();
        assert_eq!(
            address_from_uncompressed(&fetched).unwrap(),
            env.address(),
            "the address this signer would report must match the in-process signer for the same key"
        );
    }

    /// Sign against a mocked Key Vault, returning a real signature over the digest this file
    /// actually asks Key Vault to sign — proves the full path: outgoing base64url-encoded digest,
    /// incoming raw-(r,s) response, and `recoverable_from_compact` recovering to the right key.
    #[tokio::test]
    async fn sign_digest_matches_the_in_process_signer() {
        let secp = Secp256k1::new();
        let secret = SecretKey::from_slice(&hex::decode(SECRET).unwrap()).unwrap();
        let pubkey = PublicKey::from_secret_key(&secp, &secret).serialize_uncompressed();
        let digest = digest_for_hash_hex(HASH_HEX);
        let msg = Message::from_digest_slice(&digest).unwrap();
        let compact = secp.sign_ecdsa(&msg, &secret).serialize_compact();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/keys/mint-key-a/abc123/sign"))
            .and(query_param("api-version", API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "kid": format!("{}/keys/mint-key-a/abc123", server.uri()),
                "value": URL_SAFE_NO_PAD.encode(compact),
            })))
            .mount(&server)
            .await;

        let signer = test_signer(&server, pubkey, String::new());
        let returned = signer.sign_digest("fake-token", &digest).await.unwrap();
        let (r, s, v) = recoverable_from_compact(&returned, &digest, &pubkey).unwrap();

        let env = EnvKeySigner::from_secret_hex(SECRET).unwrap();
        let (r_env, s_env, v_env) = env.sign_hash_hex(HASH_HEX).unwrap();
        assert_eq!(r, r_env);
        assert_eq!(s, s_env);
        assert_eq!(v, v_env);
    }

    /// The one test that calls the actual `ChainSigner` method — token fetch, sign, and the
    /// `block_in_place` bridge between them, all through the same public entry point production
    /// code will call. Needs the multi-threaded runtime: `block_in_place` panics on a
    /// current-thread one, so this is the test that would fail if `sign_hash_hex` were ever
    /// called from the wrong runtime flavour, or if the bridge were built wrong in the first place.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chain_signer_sign_hash_hex_works_end_to_end() {
        let secp = Secp256k1::new();
        let secret = SecretKey::from_slice(&hex::decode(SECRET).unwrap()).unwrap();
        let pubkey = PublicKey::from_secret_key(&secp, &secret).serialize_uncompressed();
        let digest = digest_for_hash_hex(HASH_HEX);
        let msg = Message::from_digest_slice(&digest).unwrap();
        let compact = secp.sign_ecdsa(&msg, &secret).serialize_compact();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/test-tenant/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token_type": "Bearer",
                "expires_in": 3599,
                "access_token": "fake-token",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/keys/mint-key-a/abc123/sign"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "kid": "unused",
                "value": URL_SAFE_NO_PAD.encode(compact),
            })))
            .mount(&server)
            .await;

        let signer = test_signer(&server, pubkey, "unused-address-not-checked-here".into());
        // The call this whole file exists for: the ordinary, synchronous `ChainSigner` method.
        let (r, s, v) = signer.sign_hash_hex(HASH_HEX).expect("sign_hash_hex must succeed");

        let env = EnvKeySigner::from_secret_hex(SECRET).unwrap();
        let (r_env, s_env, v_env) = env.sign_hash_hex(HASH_HEX).unwrap();
        assert_eq!((r, s, v), (r_env, s_env, v_env));
    }
}
