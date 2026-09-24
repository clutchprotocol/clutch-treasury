# GasFree Crate Implementation Plan (Plan 1 of 2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a pure `gasfree` crate that computes a wallet's GasFree address and the TIP-712 hash of a GasFree permit, proven equal to the official SDK and to the live GasFree contracts.

**Architecture:** One small library crate with no key, no network and no async: public constants in, bytes out. In Plan 2 the key-free orchestrator (to show a user their address) and the signer (to sign permits) will both depend on it, so the derivation exists in one copy. Nothing depends on it after this plan, so merging it changes nothing in production.

**Tech Stack:** Rust 2021, `sha3` 0.10 (Keccak-256), `bs58` 0.5 with the `check` feature (TRON base58check), the workspace's `hex`.

**Spec:** `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md`. This plan implements the derivation in §1 and the permit hash that §3 and §4 sign. §8 lists the tests.

## Global Constraints

- **No local builds.** "Do not run `cargo`, `npm`, `docker`, or any build/test/lint command on this Windows host, and forbid it in every subagent prompt. Verify code by dispatching CI and reading the run log." This machine cannot link Rust and has no Docker daemon.
- A test counts only when the CI log shows it **by name** (`test address::tests::nile_salt_matches_the_sdk ... ok`). A green badge is not evidence. Pick the CI run by head SHA, never "the latest run".
- One implementer per checkout at a time. Commit with `git commit -F <file>`. Never put backticks in `-m`.
- The crate stays pure: no private key, no network, no I/O, no async. Dependencies are `sha3`, `bs58` and the workspace `hex`, nothing else. The orchestrator will link this crate, and the orchestrator must stay key-free.
- Every constant and every vector comes from `gasfreeio/gasfree-sdk-js` at commit `70f4d5aa12da785735903eda8efd568406ba6ffc`, except the two mainnet permit hashes, which come from the mainnet controller itself.
- The CREATE2 prefix is `0x41` on TRON, not Ethereum's `0xff` (spec §1).
- The TIP-712 domain is name `GasFreeController`, version `V1.0.0`, the network's chain id, and the controller as `verifyingContract`.
- Chain ids: Nile `3448148188` (`0xcd8690dc`), mainnet `728126428` (`0x2b6653dc`).
- A fenced code block in a doc comment must be marked `text`. `cargo test` compiles every other fenced block as a doctest, and pseudo-code fails it.
- Nothing in this plan depends on the spec's open question 1 (what the permit's `value` means). The crate hashes the number it is given. Plan 2 depends on the answer.

## Two deviations from the spec, on purpose

1. **§8 asks for a CI job that runs the JavaScript SDK** to cross-check the permit hash. This plan uses that SDK's own published vectors instead. The SDK's test suite checks each one against the live Nile controller's `getMessageHashForTransfer`, and on 2026-09-24 five vectors were read again from the live controllers through TronGrid and still matched: the mainnet addresses of `TMVQ…` and `TWYS…`, the Nile address of `TMVQ…`, and the Nile permit hashes of messages 1 and 5. A pinned JavaScript job would print the same fixed numbers, and it would add Node and `tronweb` to this repo's CI. The SDK publishes no mainnet permit hashes, so two were read from the mainnet controller the same way.
2. **§1 says the crate depends "only on `sha3` and `bs58`".** It also uses the workspace's `hex`, to decode the creation code. Every other crate here already uses it.

---

## File Structure

- `Cargo.toml` (workspace root): add `crates/gasfree` to `members` (Task 1)
- `Cargo.lock`: add the `gasfree` package entry (Task 1)
- `crates/gasfree/Cargo.toml`: the new crate (Task 1)
- `crates/gasfree/src/lib.rs`: `Chain`, `NILE`, `MAINNET`, and the private helpers that both modules use: TRON address decode and encode, ABI words, Keccak-256 (Task 1; Task 2 adds two lines)
- `crates/gasfree/src/creation_code_nile.hex`, `crates/gasfree/src/creation_code_mainnet.hex`: the beacon proxy's creation code, 997 bytes each, as hex text, from the pinned SDK commit (Task 1)
- `crates/gasfree/src/address.rs`: `gasfree_address` and its tests (Task 1)
- `crates/gasfree/src/permit.rs`: `Permit`, `permit_hash` and their tests (Task 2)
- `README.md`: a row for the crate in the Crates table (Task 1)

Tests live inline in `#[cfg(test)] mod tests`, as they do in `crates/tron-signer/src/keys.rs`.

Commit message files go in `.superpowers/sdd/2026-09-24-gasfree-crate/`, which git ignores.

---

### Task 1: The crate, and a wallet's GasFree address

**Files:**
- Create: `crates/gasfree/Cargo.toml`
- Create: `crates/gasfree/src/lib.rs`
- Create: `crates/gasfree/src/address.rs`
- Create: `crates/gasfree/src/creation_code_nile.hex`, `crates/gasfree/src/creation_code_mainnet.hex`
- Modify: `Cargo.toml` (the `members` line)
- Modify: `Cargo.lock` (one new `[[package]]` block)
- Modify: `README.md` (the Crates table)
- Test: `crates/gasfree/src/address.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces, public:
  - `gasfree::Chain` with public fields `chain_id: u64`, `controller: &'static str`, `beacon: &'static str`, and one private field
  - `gasfree::NILE: Chain` and `gasfree::MAINNET: Chain`
  - `gasfree::gasfree_address(chain: &Chain, user: &str) -> Result<String, String>`
- Produces, crate-private (Task 2 uses them): `decode(address: &str) -> Result<[u8; 20], String>`, `encode(body: &[u8; 20]) -> String`, `address_word(body: &[u8; 20]) -> [u8; 32]`, `uint_word(n: u64) -> [u8; 32]`, `keccak(data: &[u8]) -> [u8; 32]`, `Chain::creation_code(&self) -> Vec<u8>`

- [ ] **Step 1: Create the branch**

```bash
cd /d/source/clutch/clutch-treasury
git checkout main
git pull --ff-only origin main
git checkout -b feat/gasfree-crate
```

- [ ] **Step 2: Fetch the two creation codes from the pinned SDK commit**

They are copied from the SDK, not typed. Run this from the repo root in Git Bash:

```bash
mkdir -p crates/gasfree/src
R=70f4d5aa12da785735903eda8efd568406ba6ffc
COMMON=$(gh api "repos/gasfreeio/gasfree-sdk-js/contents/src/constant/common.ts?ref=$R" --jq .content | base64 -d)
for pair in TRON_NILE_CREATION_CODE:nile TRON_CREATION_CODE:mainnet; do
  printf '%s\n' "$COMMON" | sed -n "/^export const ${pair%%:*} =\$/{n;s/^ *'0x\([0-9a-f]*\)';\$/\1/p;}" | tr -d '\n' > "crates/gasfree/src/creation_code_${pair##*:}.hex"
done
sha256sum crates/gasfree/src/creation_code_*.hex
```

Expected, exactly (Git Bash prints a `*` before each path):

```text
b5ed723d40adf358e9e53bf370d0752aa08f0a78ddccd1a12ffb44edbc34d2b5 *crates/gasfree/src/creation_code_mainnet.hex
32e5cc0e46ade676613ede0c315d63df800b534e5eb1d18e2fa2b8fc26e2a80f *crates/gasfree/src/creation_code_nile.hex
```

Each file is one line of 1994 lowercase hex characters with no newline. If either hash differs, stop and report it. Do not edit these files by hand, and do not open them in an editor that adds a final newline. (`lib.rs` trims whitespace anyway, but the hashes above would no longer match.)

- [ ] **Step 3: Create the crate manifest and add it to the workspace**

Create `crates/gasfree/Cargo.toml`:

```toml
[package]
name = "gasfree"
version = "0.1.0"
edition = "2021"

# Pure functions over public constants: no key, no network, no async. Keep it that way. The
# key-free payment-orchestrator links this crate, so nothing that can sign or spend belongs here.
[dependencies]
bs58 = { version = "0.5", features = ["check"] }
sha3 = "0.10.1"
hex.workspace = true
```

In the root `Cargo.toml`, replace the `members` line with:

```toml
members = ["crates/clutch-chain", "crates/treasury-service", "crates/payment-orchestrator", "crates/tron-signer", "crates/gasfree"]
```

In `Cargo.lock`, insert this block between the `futures-util` block and the `generic-array` block (packages are sorted by name, and each block is separated by one empty line):

```toml
[[package]]
name = "gasfree"
version = "0.1.0"
dependencies = [
 "bs58",
 "hex",
 "sha3",
]
```

Each of the three dependencies has exactly one version in the lockfile, so cargo writes them by name only. Nothing here builds with `--locked`, so cargo would add this block by itself; it is written by hand so the committed lockfile matches the workspace.

- [ ] **Step 4: Write `lib.rs`**

Create `crates/gasfree/src/lib.rs`:

```rust
//! GasFree on TRON: the address where a wallet's GasFree account lives, and the hash its owner
//! signs to move a token out of it.
//!
//! Pure functions over public constants, with no key and no network. The key-free orchestrator
//! and the signer both compute from this one copy, because two copies that drifted apart would
//! show a user one address while the signer swept another.
//!
//! Ported from the official SDK, `gasfreeio/gasfree-sdk-js` at commit
//! `70f4d5aa12da785735903eda8efd568406ba6ffc`, and tested against that SDK's own vectors.
//! Design: `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md`.

mod address;

pub use address::gasfree_address;

use sha3::{Digest, Keccak256};

/// One GasFree deployment: the public constants that an address and a permit are computed from.
///
/// Only `NILE` and `MAINNET` exist. The creation code field is private, so a caller cannot build a
/// third `Chain` that mixes one network's controller with another network's creation code.
pub struct Chain {
    /// The TIP-712 domain's `chainId`.
    pub chain_id: u64,
    /// The GasFreeController. It deploys every GasFree address with CREATE2, and it is the permit's
    /// `verifyingContract`.
    pub controller: &'static str,
    /// The beacon that every GasFree address proxies to. Whoever controls it can change the code
    /// at every GasFree address at once. The design's tripwire watches it.
    pub beacon: &'static str,
    creation_code_hex: &'static str,
}

/// TRON's Nile testnet, from the SDK's `DefaultChainInfoMap`.
pub const NILE: Chain = Chain {
    chain_id: 3_448_148_188, // 0xcd8690dc
    controller: "THQGuFzL87ZqhxkgqYEryRAd7gqFqL5rdc",
    beacon: "TLtCGmaxH3PbuaF6kbybwteZcHptEdgQGC",
    creation_code_hex: include_str!("creation_code_nile.hex"),
};

/// TRON mainnet, from the SDK's `DefaultChainInfoMap`.
pub const MAINNET: Chain = Chain {
    chain_id: 728_126_428, // 0x2b6653dc
    controller: "TFFAMQLZybALaLb4uxHA9RBE7pxhUAjF3U",
    beacon: "TSP9UW6FQhT76XD2jWA6ipGMx3yGbjDffP",
    creation_code_hex: include_str!("creation_code_mainnet.hex"),
};

impl Chain {
    /// The beacon proxy's creation code, 997 bytes, copied from the SDK's
    /// `src/constant/common.ts`. The address tests fail if a single byte of it is wrong.
    fn creation_code(&self) -> Vec<u8> {
        hex::decode(self.creation_code_hex.trim())
            .expect("the creation code is hex; the address tests fail if it is not")
    }
}

const TRON_ADDRESS_VERSION: u8 = 0x41;

/// A TRON address's 20-byte body, after its base58check checksum and version byte are checked.
fn decode(address: &str) -> Result<[u8; 20], String> {
    let bytes = bs58::decode(address)
        .with_check(Some(TRON_ADDRESS_VERSION))
        .into_vec()
        .map_err(|e| format!("address {address} failed base58check: {e}"))?;
    if bytes.len() != 21 {
        return Err(format!("address {address} decoded to {} bytes, want 21", bytes.len()));
    }
    let mut body = [0u8; 20];
    body.copy_from_slice(&bytes[1..]);
    Ok(body)
}

/// The base58check TRON address of a 20-byte body.
fn encode(body: &[u8; 20]) -> String {
    let mut payload = Vec::with_capacity(21);
    payload.push(TRON_ADDRESS_VERSION);
    payload.extend_from_slice(body);
    // bs58's `with_check` appends the 4-byte double-SHA256 checksum, which is exactly TRON's.
    bs58::encode(payload).with_check().into_string()
}

/// An address as one 32-byte ABI word: twelve zero bytes, then the 20-byte body.
fn address_word(body: &[u8; 20]) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(body);
    word
}

/// A `uint256` as one 32-byte ABI word, big-endian.
fn uint_word(n: u64) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&n.to_be_bytes());
    word
}

fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}
```

- [ ] **Step 5: Write `address.rs` with its tests, and stub bodies**

The three function bodies are stubs in this step, so that CI shows every test failing by name before the real code exists. Step 9 replaces them.

Create `crates/gasfree/src/address.rs`:

```rust
//! Where a wallet's GasFree account lives.
//!
//! A GasFree address is a beacon proxy that the GasFreeController deploys with CREATE2 the first
//! time the address is used. So the address is known before anything exists there:
//!
//! ```text
//! salt          = user as 20 bytes, left-padded to 32
//! initData      = selector("initialize(address)") ++ salt
//! bytecodeHash  = keccak256(creationCode ++ abi.encode(address beacon, bytes initData))
//! address       = keccak256(0x41 ++ controller ++ salt ++ bytecodeHash)[12..32]
//! ```
//!
//! The CREATE2 prefix is `0x41` on TRON, not Ethereum's `0xff`. The wrong prefix gives a
//! valid-looking address that nobody controls.

use crate::{address_word, decode, encode, keccak, uint_word, Chain};

const CREATE2_PREFIX: u8 = 0x41;

/// The GasFree address of the TRON wallet `user`.
///
/// USDT sent here can be moved later by a permit signed with `user`'s key. The beacon's upgrade
/// authority can also move it; that is the risk the design accepted.
pub fn gasfree_address(_chain: &Chain, _user: &str) -> Result<String, String> {
    Ok(String::new())
}

/// `user`'s 20 bytes, left-padded to 32. It is the CREATE2 salt, and also the argument of the
/// `initialize` call.
fn salt(_user: &str) -> Result<[u8; 32], String> {
    Ok([0; 32])
}

/// Keccak-256 of the proxy's creation code followed by its ABI-encoded constructor arguments,
/// `(address beacon, bytes initData)`, where `initData` is the call `initialize(user)`.
fn bytecode_hash(_chain: &Chain, _user: &str) -> Result<[u8; 32], String> {
    Ok([0; 32])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAINNET, NILE};

    // gasfree-sdk-js @ 70f4d5a, src/tests/constant.ts, `tronAddressList`, as
    // (user, salt, bytecodeHash, gasFreeAddress). The SDK's own suite checks each address against
    // the Nile controller's getGasFreeAddress(user). The first one was read again from the live
    // controller on 2026-09-24 and still matched. The SDK prints the salt in EIP-55 mixed case;
    // it is lowercased here because `hex::encode` prints lowercase.
    const NILE_VECTORS: [(&str, &str, &str, &str); 5] = [
        (
            "TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC",
            "0000000000000000000000007e5f4552091a69125d5dfcb7b8c2659029395bdf",
            "05fc7930389726757627ebd6eccfa44c1ed3d61b7bc3eb2a52e33a10c72feb23",
            "TUGC4eNuEgbaLotwxzzXEck1fRWru6n8ye",
        ),
        (
            "TDvSsdrNM5eeXNL3czpa6AxLDHZA9nwe9K",
            "0000000000000000000000002b5ad5c4795c026514f8317c7a215e218dccd6cf",
            "4137db2fd612a7ece04620ebee511bc49ff1dd616f2fa754c337fc4cd3eca2f7",
            "TLvVuqx74fMy8QMjEsMT4dWwmVbuNwYt8X",
        ),
        (
            "TKTX96CBxr5kvhjsDHcqoiPWZageGxoTW3",
            "0000000000000000000000006813eb9362372eef6200f3b1dbc3f819671cba69",
            "ab0d0d89b6d2921967b3ff3b13b0a9ece958a3a2b3e2c56a60f25e9fba01b722",
            "TTjqEjsitExzYsoDaR65nd3d2avhsXayfL",
        ),
        (
            "TCo75zcxTuWn5nnFqZUeK5socdVnG11f2T",
            "0000000000000000000000001eff47bc3a10a45d4b230b5d10e37751fe6aa718",
            "3b993129779481dcf8ca137aae0b3ad76e606333e8d1789538cdeafbfdf1dcf3",
            "TDd2QwVKX5ujRtCkWGNJUXRD16GyvYLZm3",
        ),
        (
            "TWYSVbUy6eTu6ZrFWRUimgDy9SinkggVKL",
            "000000000000000000000000e1ab8145f7e55dc933d51a18c793f901a3a0b276",
            "f69dc6968343dcfb721a1275a476eb0e1c5e8e818865d58bcaecff55dda577f5",
            "TUgxrm9ynUaPMSbDt65nXbWVD9fYcZyFKk",
        ),
    ];

    // Same file, `tronMainnetAddressList`, as (user, gasFreeAddress). The SDK checks these against
    // the mainnet controller; the first and the last were read again on 2026-09-24 and still
    // matched. It publishes no salt or bytecode hash for mainnet. The salt does not depend on the
    // network, so if the Nile tests pass and only this one fails, suspect the mainnet constants in
    // lib.rs, not the algorithm.
    const MAINNET_VECTORS: [(&str, &str); 5] = [
        ("TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC", "TBwmA2PtMXC4HiGvfi8xg2jZd5i3y89DjK"),
        ("TDvSsdrNM5eeXNL3czpa6AxLDHZA9nwe9K", "TTA7pGKZdpkJwiwuookcfbkdq6kZxysn86"),
        ("TKTX96CBxr5kvhjsDHcqoiPWZageGxoTW3", "TJyjjMa8AKvARKyMRNm2JrZ6ftQh3cC6Zg"),
        ("TCo75zcxTuWn5nnFqZUeK5socdVnG11f2T", "TLCZMcXQuv7A9n1j3L8dTn2rr9oNe2Xqz3"),
        ("TWYSVbUy6eTu6ZrFWRUimgDy9SinkggVKL", "TVKvPadRp1B5uagKTWi93YrwxfwPGEbbUQ"),
    ];

    #[test]
    fn nile_salt_matches_the_sdk() {
        for (user, want, _, _) in NILE_VECTORS {
            assert_eq!(hex::encode(salt(user).unwrap()), want, "salt of {user}");
        }
    }

    #[test]
    fn nile_bytecode_hash_matches_the_sdk() {
        for (user, _, want, _) in NILE_VECTORS {
            assert_eq!(hex::encode(bytecode_hash(&NILE, user).unwrap()), want, "bytecode hash for {user}");
        }
    }

    #[test]
    fn nile_gasfree_address_matches_the_sdk() {
        for (user, _, _, want) in NILE_VECTORS {
            assert_eq!(gasfree_address(&NILE, user).unwrap(), want, "Nile GasFree address of {user}");
        }
    }

    #[test]
    fn mainnet_gasfree_address_matches_the_sdk() {
        for (user, want) in MAINNET_VECTORS {
            assert_eq!(gasfree_address(&MAINNET, user).unwrap(), want, "mainnet GasFree address of {user}");
        }
    }

    #[test]
    fn a_mistyped_address_is_refused() {
        // The last character changed: still valid base58, but the checksum no longer matches.
        assert!(gasfree_address(&NILE, "TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HD").is_err());
        // The same wallet in the hex form that Ethereum tools print. It is not base58 at all.
        assert!(gasfree_address(&NILE, "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf").is_err());
    }
}
```

The vector tables are fixed-size arrays, so a table with a missing row does not compile. A test cannot pass by looping over nothing.

- [ ] **Step 6: Add the crate to the README**

In `README.md`, in the table under `## Crates`, add this row after the `clutch-chain` row:

```markdown
| `gasfree` | GasFree address derivation and permit hashing, one copy shared by the orchestrator and the signer | — |
```

- [ ] **Step 7: Commit the failing tests**

Create `.superpowers/sdd/2026-09-24-gasfree-crate/commit-msg.txt` with:

```text
test(gasfree): address vectors from the official SDK, against stubs

A new crate for GasFree address derivation, with the five Nile and five
mainnet vectors from gasfree-sdk-js at 70f4d5a. The three functions are
stubs in this commit, so CI shows each test failing by name before the
real code lands.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

Then:

```bash
cd /d/source/clutch/clutch-treasury
git add Cargo.toml Cargo.lock README.md crates/gasfree
git status --short
git commit -F .superpowers/sdd/2026-09-24-gasfree-crate/commit-msg.txt
```

`git status --short` must list exactly these, all staged: `Cargo.lock`, `Cargo.toml`, `README.md`, and the five files under `crates/gasfree/` (`Cargo.toml`, `src/lib.rs`, `src/address.rs`, and the two `.hex` files).

- [ ] **Step 8 (controller): Run CI and confirm the five tests fail by name**

```bash
cd /d/source/clutch/clutch-treasury
git push -u origin feat/gasfree-crate
gh workflow run test.yml --repo clutchprotocol/clutch-treasury --ref feat/gasfree-crate
SHA=$(git rev-parse HEAD)
gh run list --repo clutchprotocol/clutch-treasury --workflow test.yml --branch feat/gasfree-crate --json databaseId,headSha --jq ".[] | select(.headSha==\"$SHA\") | .databaseId"
```

The run takes a few seconds to appear. Repeat the last command until it prints an id, then:

```bash
RUN=<the id>
gh run watch "$RUN" --repo clutchprotocol/clutch-treasury --exit-status
gh run view "$RUN" --repo clutchprotocol/clutch-treasury --log | grep -E "test address::tests::|test result:"
```

`gh run watch` blocks until the run ends; give it a 10-minute timeout, or run it in the background.

Expected: the run fails, and the log shows all five tests by name:

```text
test address::tests::a_mistyped_address_is_refused ... FAILED
test address::tests::mainnet_gasfree_address_matches_the_sdk ... FAILED
test address::tests::nile_bytecode_hash_matches_the_sdk ... FAILED
test address::tests::nile_gasfree_address_matches_the_sdk ... FAILED
test address::tests::nile_salt_matches_the_sdk ... FAILED
```

Also expected: compile warnings about unused imports, functions and constants, because the stubs use nothing yet. Cargo stops at the first test binary that fails, so the other crates' tests may not run in this run. Both are fine here.

If the run fails for any other reason, such as a compile error, fix that first. A compile error is not the red this step is looking for.

- [ ] **Step 9: Replace the three stub bodies**

In `crates/gasfree/src/address.rs`, replace the three stub functions (keep their doc comments) with:

```rust
pub fn gasfree_address(chain: &Chain, user: &str) -> Result<String, String> {
    let hash = keccak(
        &[
            &[CREATE2_PREFIX][..],
            &decode(chain.controller)?[..],
            &salt(user)?[..],
            &bytecode_hash(chain, user)?[..],
        ]
        .concat(),
    );
    let mut body = [0u8; 20];
    body.copy_from_slice(&hash[12..]);
    Ok(encode(&body))
}

fn salt(user: &str) -> Result<[u8; 32], String> {
    Ok(address_word(&decode(user)?))
}

fn bytecode_hash(chain: &Chain, user: &str) -> Result<[u8; 32], String> {
    let init_data = [&keccak(b"initialize(address)")[..4], &salt(user)?[..]].concat();
    let mut args = [
        &address_word(&decode(chain.beacon)?)[..],
        // `bytes` is a dynamic type, so its head word holds the offset of its tail. The tail
        // starts after the two head words.
        &uint_word(64)[..],
        &uint_word(init_data.len() as u64)[..],
        &init_data[..],
    ]
    .concat();
    args.resize(160, 0); // pad the 36-byte tail up to a whole number of 32-byte words
    Ok(keccak(&[&chain.creation_code()[..], &args[..]].concat()))
}
```

Do not change the tests.

- [ ] **Step 10: Commit the implementation**

Overwrite `.superpowers/sdd/2026-09-24-gasfree-crate/commit-msg.txt` with:

```text
feat(gasfree): derive a wallet's GasFree address

CREATE2 with TRON's 0x41 prefix, over the controller, the wallet as the
salt, and the hash of the beacon proxy's creation code plus its
constructor arguments. Matches all ten SDK vectors. Nothing uses the
crate yet.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

Then:

```bash
cd /d/source/clutch/clutch-treasury
git add crates/gasfree/src/address.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-crate/commit-msg.txt
```

- [ ] **Step 11 (controller): Run CI and confirm the five tests pass by name**

```bash
cd /d/source/clutch/clutch-treasury
git push
gh workflow run test.yml --repo clutchprotocol/clutch-treasury --ref feat/gasfree-crate
SHA=$(git rev-parse HEAD)
gh run list --repo clutchprotocol/clutch-treasury --workflow test.yml --branch feat/gasfree-crate --json databaseId,headSha --jq ".[] | select(.headSha==\"$SHA\") | .databaseId"
```

Repeat the last command until it prints an id, then:

```bash
RUN=<the id>
gh run watch "$RUN" --repo clutchprotocol/clutch-treasury --exit-status
gh run view "$RUN" --repo clutchprotocol/clutch-treasury --log | grep -E "test address::tests::|test result:"
```

Expected: the run succeeds, and the log shows:

```text
test address::tests::a_mistyped_address_is_refused ... ok
test address::tests::mainnet_gasfree_address_matches_the_sdk ... ok
test address::tests::nile_bytecode_hash_matches_the_sdk ... ok
test address::tests::nile_gasfree_address_matches_the_sdk ... ok
test address::tests::nile_salt_matches_the_sdk ... ok
```

Every `test result:` line says `ok`, which means every other crate's tests still pass too. There should be no compile warning from `crates/gasfree`.

If a test fails, the assertion names the wallet and the step. The salt is the first step, the bytecode hash the second, and the address the last. Fix the earliest failing step first.

---

### Task 2: The TIP-712 hash of a GasFree permit

**Files:**
- Create: `crates/gasfree/src/permit.rs`
- Modify: `crates/gasfree/src/lib.rs` (two lines)
- Test: `crates/gasfree/src/permit.rs` (inline `mod tests`)

**Interfaces:**
- Consumes, from Task 1 (crate-private, in `lib.rs`): `decode(address: &str) -> Result<[u8; 20], String>`, `address_word(body: &[u8; 20]) -> [u8; 32]`, `uint_word(n: u64) -> [u8; 32]`, `keccak(data: &[u8]) -> [u8; 32]`, and `gasfree::Chain` with its public `chain_id: u64` and `controller: &'static str`. Also `gasfree::NILE` and `gasfree::MAINNET` in the tests.
- Produces, public:
  - `gasfree::Permit<'a>` with public fields `token`, `service_provider`, `user`, `receiver` (all `&'a str`), and `value`, `max_fee`, `deadline`, `version`, `nonce` (all `u64`)
  - `gasfree::permit_hash(chain: &Chain, permit: &Permit) -> Result<[u8; 32], String>`: the 32 bytes that the owner of `permit.user` signs

- [ ] **Step 1: Write `permit.rs` with its tests, and stub bodies**

As in Task 1, the three function bodies are stubs in this step. Step 5 replaces them.

Create `crates/gasfree/src/permit.rs`:

```rust
//! The hash that a wallet's owner signs to move a token out of its GasFree address.
//!
//! TIP-712 is EIP-712 with every TRON address reduced to its 20-byte body. So the hash is the
//! standard one:
//!
//! ```text
//! permitHash = keccak256(0x1901 ++ domainSeparator ++ hashStruct(PermitTransfer))
//! ```
//!
//! over the domain `GasFreeController`, `V1.0.0`, the network's chain id, and the controller.

use crate::{address_word, decode, keccak, uint_word, Chain};

const DOMAIN_TYPE: &[u8] =
    b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
const PERMIT_TYPE: &[u8] = b"PermitTransfer(address token,address serviceProvider,address user,address receiver,uint256 value,uint256 maxFee,uint256 deadline,uint256 version,uint256 nonce)";

/// One GasFree transfer, field for field the controller's `PermitTransfer`.
pub struct Permit<'a> {
    /// The TRC-20 token being moved.
    pub token: &'a str,
    /// The relay that may submit this permit and collect its fee.
    pub service_provider: &'a str,
    /// The wallet that signs: `D`, not its GasFree address. The tokens leave
    /// `gasfree_address(user)`.
    pub user: &'a str,
    pub receiver: &'a str,
    /// In the token's smallest unit. Whether the fee is charged on top of it or taken out of it
    /// is the design's open question 1. This crate only hashes the number.
    pub value: u64,
    /// The most the relay may take as its fee, in the token's smallest unit.
    pub max_fee: u64,
    /// Unix seconds. The controller refuses the permit after this.
    pub deadline: u64,
    /// The permit format's version.
    pub version: u64,
    /// The GasFree account's nonce. Each permit uses the next one.
    pub nonce: u64,
}

/// The 32 bytes the owner of `permit.user` signs.
pub fn permit_hash(_chain: &Chain, _permit: &Permit) -> Result<[u8; 32], String> {
    Ok([0; 32])
}

fn domain_separator(_chain: &Chain) -> Result<[u8; 32], String> {
    Ok([0; 32])
}

fn struct_hash(_permit: &Permit) -> Result<[u8; 32], String> {
    Ok([0; 32])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAINNET, NILE};

    // gasfree-sdk-js @ 70f4d5a, src/tests/constant.ts, `tronMessageList`. The SDK's suite checks
    // each permit hash against the Nile controller's getMessageHashForTransfer; messages 1 and 5
    // were read again from the live controller on 2026-09-24 and still matched.
    const SERVICE_PROVIDER: &str = "TDbJyQ6g1Lx9BAfEEeN5S5TMjjDRAVFCaA";

    const PERMITS: [Permit<'static>; 5] = [
        Permit {
            token: "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf",
            service_provider: SERVICE_PROVIDER,
            user: "TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC",
            receiver: "TJM1BE5wq1VdHh3gwjUeyaVkvZp9DVYCfC",
            value: 10_000,
            max_fee: 2_000,
            deadline: 1_726_207_632,
            version: 1,
            nonce: 2,
        },
        Permit {
            token: "TLBaRhANQoJFTqre9Nf1mjuwNWjCJeYqUL",
            service_provider: SERVICE_PROVIDER,
            user: "TDvSsdrNM5eeXNL3czpa6AxLDHZA9nwe9K",
            receiver: "TLFXfejEMgivFDR2x8qBpukMXd56spmFhz",
            value: 20_000,
            max_fee: 2_000,
            deadline: 1_726_507_632,
            version: 1,
            nonce: 3,
        },
        Permit {
            token: "TVSvjZdyDSNocHm7dP3jvCmMNsCnMTPa5W",
            service_provider: SERVICE_PROVIDER,
            user: "TKTX96CBxr5kvhjsDHcqoiPWZageGxoTW3",
            receiver: "TX7WF4tRGQehC9W88XEEKBhQRkLmAtZqKo",
            value: 100_000,
            max_fee: 2_000,
            deadline: 1_729_507_632,
            version: 1,
            nonce: 5,
        },
        Permit {
            token: "TWrZRHY9aKQZcyjpovdH6qeCEyYZrRQDZt",
            service_provider: SERVICE_PROVIDER,
            user: "TCo75zcxTuWn5nnFqZUeK5socdVnG11f2T",
            receiver: "TCN4biEVzzfyUgN1NM8iysp4bYx6mx2gPv",
            value: 100_000,
            max_fee: 2_000,
            deadline: 1_729_517_632,
            version: 1,
            nonce: 15,
        },
        Permit {
            token: "TDnDyfMigx5nch7cCrtzGSwTXkUBnQJ9Pg",
            service_provider: SERVICE_PROVIDER,
            user: "TWYSVbUy6eTu6ZrFWRUimgDy9SinkggVKL",
            receiver: "TVkoisqxn1SbET8ztcnjqRGAY4npxqDcmv",
            value: 100_000,
            max_fee: 2_000,
            deadline: 1_729_907_632,
            version: 1,
            nonce: 50,
        },
    ];

    // `encodeMessage` in the SDK file. It does not depend on the network.
    const STRUCT_HASHES: [&str; 5] = [
        "66ddee4970f99745397d1da5037b1af25380b406e58d613e9a4e44c88f53f656",
        "810698dcc75464432adbb9ee4f3cabeb8340e59b278df4a67799cedcbd4ff2eb",
        "2904790f034a8932b4098421c9a471b340a3801aaaab2e59a70498bb87d680d3",
        "3bf2b313fb43ef51fd25ef50e2ea48471879d1f9db00fcac9b1e7b054e27779d",
        "a2d2ed284f8300560812505b7699d3f9fde3bafacd5444ca3de812705ff2ebbf",
    ];

    // `permitHash` in the SDK file.
    const NILE_PERMIT_HASHES: [&str; 5] = [
        "b1226f3a0b690b04e2c39fac3b58352ed68943a12a54b58035045215aaf0b9b1",
        "25c20423c18719438f4d40e6b8fec40ede6b73fb3fa702453ea9bd17dd154fb5",
        "3d103a6a3407dfe7540696131d7cafc3d41d7d8649b93a95daeee041e66238ce",
        "a1a612e946ad2fecc8bcd2f93f987c38a06ca4807db7af30442e9308a20234ea",
        "c78d11f0afc5397f9329861888a6724b66fe370f0189e67c39bfad4eeb7ec2a9",
    ];

    // `TRON_DOMAIN_SEPARATOR` in the SDK file.
    const NILE_DOMAIN_SEPARATOR: &str = "31a0a46f427dd040c91835228e4555951bde0a894cae6239869bb680ebc6ebea";

    // Not from the SDK, which publishes no mainnet permit hashes. These are what the mainnet
    // controller TFFAMQLZybALaLb4uxHA9RBE7pxhUAjF3U itself returned from
    // getMessageHashForTransfer for PERMITS[0] and PERMITS[4], read through TronGrid's
    // triggerconstantcontract on 2026-09-24. They pin the mainnet chain id and controller.
    const MAINNET_PERMIT_HASHES: [(usize, &str); 2] = [
        (0, "f8e96d82742565a5e4a661cda2b87344dbc69d0b766ecf96785f6d8c933c376e"),
        (4, "b83af7d9fa79dd84011cef0a902b0035101fd33b00b006cc2f95cb1ab698e43c"),
    ];

    #[test]
    fn nile_domain_separator_matches_the_sdk() {
        assert_eq!(hex::encode(domain_separator(&NILE).unwrap()), NILE_DOMAIN_SEPARATOR);
    }

    #[test]
    fn struct_hash_matches_the_sdk() {
        for (i, permit) in PERMITS.iter().enumerate() {
            assert_eq!(hex::encode(struct_hash(permit).unwrap()), STRUCT_HASHES[i], "SDK message {}", i + 1);
        }
    }

    #[test]
    fn nile_permit_hash_matches_the_sdk() {
        for (i, permit) in PERMITS.iter().enumerate() {
            assert_eq!(
                hex::encode(permit_hash(&NILE, permit).unwrap()),
                NILE_PERMIT_HASHES[i],
                "SDK message {}",
                i + 1
            );
        }
    }

    #[test]
    fn mainnet_permit_hash_matches_the_mainnet_controller() {
        for (i, want) in MAINNET_PERMIT_HASHES {
            assert_eq!(hex::encode(permit_hash(&MAINNET, &PERMITS[i]).unwrap()), want, "SDK message {}", i + 1);
        }
    }
}
```

- [ ] **Step 2: Register the module in `lib.rs`**

In `crates/gasfree/src/lib.rs`, replace:

```rust
mod address;

pub use address::gasfree_address;
```

with:

```rust
mod address;
mod permit;

pub use address::gasfree_address;
pub use permit::{permit_hash, Permit};
```

- [ ] **Step 3: Commit the failing tests**

Overwrite `.superpowers/sdd/2026-09-24-gasfree-crate/commit-msg.txt` with:

```text
test(gasfree): permit hash vectors, against stubs

The five Nile messages from gasfree-sdk-js at 70f4d5a, checked at the
domain separator, the struct hash and the permit hash, plus two mainnet
permit hashes read from the mainnet controller. The functions are stubs
in this commit, so CI shows each test failing by name.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

Then:

```bash
cd /d/source/clutch/clutch-treasury
git add crates/gasfree/src/permit.rs crates/gasfree/src/lib.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-crate/commit-msg.txt
```

- [ ] **Step 4 (controller): Run CI and confirm the four new tests fail by name**

```bash
cd /d/source/clutch/clutch-treasury
git push
gh workflow run test.yml --repo clutchprotocol/clutch-treasury --ref feat/gasfree-crate
SHA=$(git rev-parse HEAD)
gh run list --repo clutchprotocol/clutch-treasury --workflow test.yml --branch feat/gasfree-crate --json databaseId,headSha --jq ".[] | select(.headSha==\"$SHA\") | .databaseId"
```

Repeat the last command until it prints an id, then:

```bash
RUN=<the id>
gh run watch "$RUN" --repo clutchprotocol/clutch-treasury --exit-status
gh run view "$RUN" --repo clutchprotocol/clutch-treasury --log | grep -E "test (address|permit)::tests::|test result:"
```

Expected: the run fails. The five address tests still say `ok`, and the four new ones fail by name:

```text
test permit::tests::mainnet_permit_hash_matches_the_mainnet_controller ... FAILED
test permit::tests::nile_domain_separator_matches_the_sdk ... FAILED
test permit::tests::nile_permit_hash_matches_the_sdk ... FAILED
test permit::tests::struct_hash_matches_the_sdk ... FAILED
```

Also expected: warnings about unused imports and unused constants in `permit.rs`, because the stubs use nothing yet.

If the run fails for any other reason, such as a compile error, fix that first.

- [ ] **Step 5: Replace the three stub bodies**

In `crates/gasfree/src/permit.rs`, replace the three stub functions (keep the doc comment on `permit_hash`) with:

```rust
pub fn permit_hash(chain: &Chain, permit: &Permit) -> Result<[u8; 32], String> {
    Ok(keccak(
        &[&[0x19u8, 0x01][..], &domain_separator(chain)?[..], &struct_hash(permit)?[..]].concat(),
    ))
}

fn domain_separator(chain: &Chain) -> Result<[u8; 32], String> {
    Ok(keccak(
        &[
            &keccak(DOMAIN_TYPE)[..],
            &keccak(b"GasFreeController")[..],
            &keccak(b"V1.0.0")[..],
            &uint_word(chain.chain_id)[..],
            &address_word(&decode(chain.controller)?)[..],
        ]
        .concat(),
    ))
}

fn struct_hash(permit: &Permit) -> Result<[u8; 32], String> {
    Ok(keccak(
        &[
            &keccak(PERMIT_TYPE)[..],
            &address_word(&decode(permit.token)?)[..],
            &address_word(&decode(permit.service_provider)?)[..],
            &address_word(&decode(permit.user)?)[..],
            &address_word(&decode(permit.receiver)?)[..],
            &uint_word(permit.value)[..],
            &uint_word(permit.max_fee)[..],
            &uint_word(permit.deadline)[..],
            &uint_word(permit.version)[..],
            &uint_word(permit.nonce)[..],
        ]
        .concat(),
    ))
}
```

The field order in `struct_hash` must match `PERMIT_TYPE` exactly. Do not change the tests.

- [ ] **Step 6: Commit the implementation**

Overwrite `.superpowers/sdd/2026-09-24-gasfree-crate/commit-msg.txt` with:

```text
feat(gasfree): the TIP-712 hash of a GasFree permit

Standard EIP-712 with TRON addresses reduced to their 20-byte body, over
the GasFreeController V1.0.0 domain. Matches the SDK's Nile domain
separator, its five struct and permit hashes, and two permit hashes from
the mainnet controller. Nothing uses the crate yet.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

Then:

```bash
cd /d/source/clutch/clutch-treasury
git add crates/gasfree/src/permit.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-crate/commit-msg.txt
```

- [ ] **Step 7 (controller): Run CI and confirm all nine tests pass by name**

```bash
cd /d/source/clutch/clutch-treasury
git push
gh workflow run test.yml --repo clutchprotocol/clutch-treasury --ref feat/gasfree-crate
SHA=$(git rev-parse HEAD)
gh run list --repo clutchprotocol/clutch-treasury --workflow test.yml --branch feat/gasfree-crate --json databaseId,headSha --jq ".[] | select(.headSha==\"$SHA\") | .databaseId"
```

Repeat the last command until it prints an id, then:

```bash
RUN=<the id>
gh run watch "$RUN" --repo clutchprotocol/clutch-treasury --exit-status
gh run view "$RUN" --repo clutchprotocol/clutch-treasury --log | grep -E "test (address|permit)::tests::|test result:"
```

Expected: the run succeeds, and the log shows all nine by name:

```text
test address::tests::a_mistyped_address_is_refused ... ok
test address::tests::mainnet_gasfree_address_matches_the_sdk ... ok
test address::tests::nile_bytecode_hash_matches_the_sdk ... ok
test address::tests::nile_gasfree_address_matches_the_sdk ... ok
test address::tests::nile_salt_matches_the_sdk ... ok
test permit::tests::mainnet_permit_hash_matches_the_mainnet_controller ... ok
test permit::tests::nile_domain_separator_matches_the_sdk ... ok
test permit::tests::nile_permit_hash_matches_the_sdk ... ok
test permit::tests::struct_hash_matches_the_sdk ... ok
```

Every `test result:` line says `ok`, and there is no compile warning from `crates/gasfree`.

If a test fails, the checks go from the inside out: struct hash, then domain separator, then permit hash. A wrong struct hash is usually field order or a type string; a wrong domain separator is usually the chain id or the controller.

- [ ] **Step 8 (controller): Open the pull request**

Write the body to `.superpowers/sdd/2026-09-24-gasfree-crate/pr-body.md`. Fill in the three run ids: Task 1 Step 8, Task 2 Step 4 and Task 2 Step 7.

```markdown
Plan 1 of 2 for `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md`: a pure crate that computes a wallet's GasFree address and the TIP-712 hash of a GasFree permit.

**Merging this changes nothing in production.** No crate depends on `gasfree` yet. Plan 2 connects it to the orchestrator and the signer, behind `TRANSFER_RAIL`, which defaults to `trx`.

## What it does

- `gasfree_address(chain, user)`: CREATE2 with TRON's `0x41` prefix, as in the SDK's `GasFree.ts`.
- `permit_hash(chain, permit)`: EIP-712 with 20-byte addresses, over the `GasFreeController` / `V1.0.0` domain.
- `NILE` and `MAINNET` constants, and the two 997-byte creation codes, copied from `gasfreeio/gasfree-sdk-js` at `70f4d5aa12da785735903eda8efd568406ba6ffc`.

No key, no network, no async. Dependencies: `sha3`, `bs58`, `hex`.

## Evidence

CI run <id from Task 2 Step 7>, all nine by name:

- `address::tests::nile_salt_matches_the_sdk`, `nile_bytecode_hash_matches_the_sdk`, `nile_gasfree_address_matches_the_sdk`: the SDK's five Nile vectors, each step checked on its own
- `address::tests::mainnet_gasfree_address_matches_the_sdk`: the SDK's five mainnet vectors
- `address::tests::a_mistyped_address_is_refused`
- `permit::tests::nile_domain_separator_matches_the_sdk`, `struct_hash_matches_the_sdk`, `nile_permit_hash_matches_the_sdk`: the SDK's five Nile messages
- `permit::tests::mainnet_permit_hash_matches_the_mainnet_controller`: two hashes the mainnet controller returned itself

Each new test was first seen failing by name against stubs (runs <id from Task 1 Step 8> and <id from Task 2 Step 4>).

## Different from the spec

- §8 asks for a CI job that runs the JavaScript SDK. The SDK's own vectors are used instead: its suite checks them against the live controllers, and five were read again from the live contracts on 2026-09-24. A pinned JavaScript job would print the same numbers and add Node to this CI.
- §1 lists `sha3` and `bs58` as the only dependencies. The crate also uses the workspace `hex`.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

Then:

```bash
cd /d/source/clutch/clutch-treasury
gh pr create --repo clutchprotocol/clutch-treasury --base main --head feat/gasfree-crate --title "feat(gasfree): GasFree address derivation and permit hash, tested against the SDK" --body-file .superpowers/sdd/2026-09-24-gasfree-crate/pr-body.md
```

The maintainer merges it.

---

## After the merge

- In `D:\source\clutch\CLAUDE.md` (the workspace file, which is not in any repo, so there is nothing to commit), add `gasfree` to the clutch-treasury row's crate list: ``Crates: `treasury-service`, `payment-orchestrator`, `tron-signer`, `clutch-chain`, `gasfree` ``.
- Write Plan 2 from spec §§2-9: `TRANSFER_RAIL`; the orchestrator storing `G` for new users; the treasury's fee reservation, minimum deposit and `implementation()` tripwire; the signer's relay client, GasFree sweep and payout, receiver routing and one-time float activation; config, invariants and `PROBE=gasfree` in `clutch-deploy`; and the Nile rollout. Its sweep code depends on open question 1, so the answer comes first.
