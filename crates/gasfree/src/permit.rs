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
