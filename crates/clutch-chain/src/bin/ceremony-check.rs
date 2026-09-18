//! Steps 3 and 4 of `docs/KEY-CEREMONY.md`, through the code production actually uses.
//!
//! The ceremony says to derive the mint authority's address and then prove, by signing, that the
//! address really belongs to the key in the vault. Doing either by hand defeats the point: a
//! hand-derived address checked by a second hand-derivation is one method checked against itself,
//! and this address goes into the genesis hash where it cannot be corrected.
//!
//! Two modes, deliberately split by what they need:
//!
//!   jwk <x> <y>   Derives the address from the PUBLIC coordinates and nothing else. No secret,
//!                 no network. Safe to run anywhere, including a public CI log, because the
//!                 public key is public. This is the value that becomes `mint_authority`.
//!
//!   azure         Builds the real `AzureKmsSigner`, which fetches the key itself, then signs and
//!                 confirms the signature recovers to that same address. Needs the client secret,
//!                 so it must run where that secret is allowed to exist.
//!
//! Run `jwk` first and `azure` second, then check that the two addresses match. They come from
//! different inputs -- one from coordinates you read out of the portal, one from what the vault
//! serves the service -- so agreement means the thing you wrote down is the thing that will sign.

use base64::Engine;
use clutch_chain::external_signature::address_from_uncompressed;
use clutch_chain::signer::ChainSigner;

/// A fixed, obviously-synthetic digest. Fixed so the ceremony register records a value someone can
/// re-run later and compare; synthetic so it is not a real transaction hash.
const CEREMONY_HASH: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

/// Accepts either base64 alphabet, because the two places an operator can get these from disagree:
/// `az keyvault key show` prints STANDARD base64 (`+`, `/`, `=`), while the Key Vault REST API
/// returns base64url (`-`, `_`) per the JWK spec — which is what `AzureKmsSigner` decodes.
///
/// Being lenient here is deliberate and is confined to this helper. The alternative is an operator
/// hand-editing `+` to `-` in a value that becomes the genesis hash, and a transcription slip there
/// cannot be corrected without a new chain. `AzureKmsSigner` stays strict: it reads one source with
/// one encoding, so it has no such ambiguity to tolerate.
fn decode_coord(name: &str, s: &str) -> Result<Vec<u8>, String> {
    let normalised: String = s
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(normalised)
        .map_err(|e| format!("{name} did not decode as base64: {e}"))
}

/// Builds the point exactly as `AzureKmsSigner::fetch_public_key` does, including the 32-byte
/// length check. A shorter coordinate means the key is not on a 256-bit curve, which is the
/// P-256 vs P-256K mistake the ceremony is most concerned with.
fn address_from_jwk(x_b64: &str, y_b64: &str) -> Result<String, String> {
    let x = decode_coord("x", x_b64)?;
    let y = decode_coord("y", y_b64)?;
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
    address_from_uncompressed(&point)
}

// block_in_place, which ChainSigner::sign_hash_hex uses, requires the multi-threaded runtime.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");

    let result = match mode {
        "jwk" => match (args.get(2), args.get(3)) {
            (Some(x), Some(y)) => address_from_jwk(x, y).map(|addr| {
                println!("=== KEY-CEREMONY.md step 3, from the public coordinates ===");
                println!("address: {addr}");
                println!();
                println!("This is the mainnet `mint_authority`. It is committed into the genesis");
                println!("hash, so record it now and check it against the `azure` mode's address");
                println!("before any chain is started with it.");
            }),
            _ => Err("usage: ceremony-check jwk <x-base64url> <y-base64url>".to_string()),
        },

        "azure" => run_azure().await,

        _ => Err(concat!(
            "usage:\n",
            "  ceremony-check jwk <x> <y>   address from the public coordinates (no secret)\n",
            "  ceremony-check azure         address + test signature via the real signer\n",
            "\n",
            "azure mode reads AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_CLIENT_SECRET,\n",
            "AZURE_VAULT_URL, AZURE_KEY_NAME, AZURE_KEY_VERSION."
        )
        .to_string()),
    };

    if let Err(e) = result {
        eprintln!("FAILED: {e}");
        std::process::exit(1);
    }
}

async fn run_azure() -> Result<(), String> {
    let signer = clutch_chain::azure_kms_signer::AzureKmsSigner::new(
        env("AZURE_TENANT_ID")?,
        env("AZURE_CLIENT_ID")?,
        env("AZURE_CLIENT_SECRET")?,
        env("AZURE_VAULT_URL")?,
        env("AZURE_KEY_NAME")?,
        env("AZURE_KEY_VERSION")?,
    )
    .await?;

    let address = signer.address();
    println!("=== KEY-CEREMONY.md step 3, from the vault ===");
    println!("address: {address}");

    println!();
    println!("=== KEY-CEREMONY.md step 4, a test signature ===");
    println!("hash:    {CEREMONY_HASH}");

    // sign_hash_hex returns Ok only when one of the candidate recovery ids reproduces the public
    // key the signer fetched -- so reaching this line at all IS the recovery check. There is no
    // separate assertion to make, and adding a hand-rolled one here would check a different
    // implementation rather than this one.
    let (r, s, v) = signer.sign_hash_hex(CEREMONY_HASH)?;
    println!("r:       {r}");
    println!("s:       {s}");
    println!("v:       {v}");
    println!();
    println!("Recovery matched: the signature recovers to {address}, which is why this printed");
    println!("instead of failing. Record r, s, v and this address in the ceremony register.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A valid secp256k1 point, in the two encodings the two sources produce. The x here contains
    // a '/' in standard base64 and a '_' in base64url, which is precisely the pair that made the
    // first version of this tool reject what `az keyvault key show` prints.
    const X_STD: &str = "7HlxUjXo/ZiIUv+sYMwa9ILIbGrgXxFS9IiDDGNt9zM=";
    const Y_STD: &str = "hHLMlFOx0dfEYkR/zA7Yblt1IbrjVydJDXpTikzCOjk=";
    const X_URL: &str = "7HlxUjXo_ZiIUv-sYMwa9ILIbGrgXxFS9IiDDGNt9zM";
    const Y_URL: &str = "hHLMlFOx0dfEYkR_zA7Yblt1IbrjVydJDXpTikzCOjk";

    #[test]
    fn both_base64_alphabets_give_the_same_address() {
        let from_std = address_from_jwk(X_STD, Y_STD).expect("standard base64 must decode");
        let from_url = address_from_jwk(X_URL, Y_URL).expect("base64url must decode");
        assert_eq!(
            from_std, from_url,
            "the CLI's encoding and the REST API's encoding must agree — if they ever do not, an \
             operator reading coordinates from one source and a service reading them from the \
             other would commit different addresses to the genesis hash"
        );
    }

    #[test]
    fn a_wrong_length_coordinate_is_refused() {
        // 31 bytes: what a non-256-bit curve would give, which is the P-256K mistake arriving as
        // a length rather than as an obviously wrong address.
        let short = base64::engine::general_purpose::STANDARD_NO_PAD.encode([0u8; 31]);
        let err = address_from_jwk(&short, Y_STD).expect_err("a short coordinate must be refused");
        assert!(err.contains("wrong curve"), "{err}");
    }
}
