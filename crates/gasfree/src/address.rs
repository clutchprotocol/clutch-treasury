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
