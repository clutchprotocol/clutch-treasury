//! What the deposit route asks the chain about GasFree (spec §1, §5), over TronGrid.
//!
//! No key and no relay: the orchestrator computes a user's GasFree address itself, with the shared
//! `gasfree` crate, and asks the chain only whether GasFree's code is still the reviewed code,
//! whether an account is activated, and — once, at boot — whether the controller agrees with the
//! derivation.

use crate::derive::AddressDeriver;

pub struct GasFreeChain {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

/// What the boot check found.
#[derive(Debug, PartialEq)]
pub enum SelfTest {
    Passed,
    /// The chain answered, and the answer is wrong for these settings. The service must not start.
    Failed(String),
    /// TronGrid gave no usable answer. Not fatal: the code check before each GasFree address still runs.
    Unreachable(String),
}

impl GasFreeChain {
    pub fn new(base_url: String, api_key: String) -> Self {
        // Bounded: the deposit route waits on these reads.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client builder");
        Self { http, base_url, api_key }
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base_url))
            .header("TRON-PRO-API-KEY", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("trongrid {path} returned {}", resp.status()));
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    /// Whether `address` holds a deployed contract; for a GasFree account, whether it is activated.
    /// Only `{}` means no: an activated GasFree account answers with its contract record and an
    /// EMPTY bytecode (2026-09-24), and any other answer is an error, never "no".
    pub async fn has_contract(&self, address: &str) -> Result<bool, String> {
        let v = self.post("/wallet/getcontract", serde_json::json!({"value": address, "visible": true})).await?;
        if v["contract_address"].as_str().is_some_and(|a| !a.is_empty()) {
            return Ok(true);
        }
        if v.as_object().is_some_and(|o| o.is_empty()) {
            return Ok(false);
        }
        Err(format!("getcontract for {address} gave neither a contract nor {{}}: {v}"))
    }

    /// The first 32-byte word a view function returns, as 64 lowercase hex characters.
    async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String> {
        // TronGrid wants an `owner_address`, and a view does not care who asks, so the contract is
        // named as its own caller.
        let mut body = serde_json::json!({
            "owner_address": contract,
            "contract_address": contract,
            "function_selector": selector,
            "visible": true,
        });
        if let Some(p) = parameter {
            body["parameter"] = serde_json::Value::from(p);
        }
        let v = self.post("/wallet/triggerconstantcontract", body).await?;
        let word = v["constant_result"][0]
            .as_str()
            .ok_or_else(|| format!("{selector} on {contract} returned nothing: {v}"))?;
        if word.len() != 64 || !word.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{selector} on {contract} returned {word:?}, not one 32-byte word"));
        }
        Ok(word.to_ascii_lowercase())
    }

    /// Why no GasFree address may be handed out, when GasFree's code is not the reviewed code;
    /// `None` when it is (spec §5). Both proxies: the beacon behind every account, and the
    /// controller that moves money out of them.
    pub async fn code_changed(&self, settings: &gasfree::Settings) -> Result<Option<String>, String> {
        let checks = [
            ("beacon", settings.chain.beacon, &settings.expected_beacon_implementation),
            ("controller", settings.chain.controller, &settings.expected_controller_implementation),
        ];
        for (what, proxy, expected) in checks {
            let word = self.view_word(proxy, "implementation()", None).await?;
            let now = &word[24..];
            if now != expected.as_str() {
                return Ok(Some(format!("the GasFree {what} {proxy} now runs 0x{now}, not the reviewed 0x{expected}")));
            }
        }
        Ok(None)
    }

    /// Once at boot: are these GasFree constants the ones deployed where this TronGrid points, and
    /// does the controller put index 0's account where this service derives it? A Nile setting on a
    /// mainnet TronGrid fails here, instead of showing users addresses nobody controls.
    pub async fn self_test(&self, settings: &gasfree::Settings, deriver: &AddressDeriver) -> SelfTest {
        let controller = settings.chain.controller;
        match self.has_contract(controller).await {
            Ok(true) => {}
            Ok(false) => {
                return SelfTest::Failed(format!(
                    "the GasFree controller {controller} is not a contract on this TronGrid: APP_GASFREE_NETWORK does \
                     not match APP_TRONGRID_URL"
                ))
            }
            Err(e) => return SelfTest::Unreachable(e),
        }
        let owner = match deriver.address_at(0) {
            Ok(a) => a,
            Err(e) => return SelfTest::Failed(e),
        };
        let ours = match gasfree::gasfree_address(settings.chain, &owner).and_then(|g| abi_address(&g)) {
            Ok(word) => word,
            Err(e) => return SelfTest::Failed(e),
        };
        let parameter = match abi_address(&owner) {
            Ok(p) => p,
            Err(e) => return SelfTest::Failed(e),
        };
        match self.view_word(controller, "getGasFreeAddress(address)", Some(&parameter)).await {
            Ok(word) if word[24..] == ours[24..] => SelfTest::Passed,
            Ok(word) => SelfTest::Failed(format!(
                "the controller puts the GasFree account of {owner} at 0x{}, this service derives 0x{}",
                &word[24..],
                &ours[24..]
            )),
            Err(e) => SelfTest::Unreachable(e),
        }
    }
}

/// A TRON address as one 32-byte ABI word, after its base58check checksum is checked.
fn abi_address(address: &str) -> Result<String, String> {
    let bytes = bs58::decode(address)
        .with_check(Some(0x41))
        .into_vec()
        .map_err(|e| format!("address {address} failed base58check: {e}"))?;
    if bytes.len() != 21 {
        return Err(format!("address {address} decoded to {} bytes, want 21", bytes.len()));
    }
    Ok(format!("{:0>64}", hex::encode(&bytes[1..])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The canonical all-"abandon" test wallet's account xpub, as derive.rs pins it.
    const XPUB: &str = "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd";

    fn nile() -> gasfree::Settings {
        gasfree::Settings {
            chain: &gasfree::NILE,
            rail: true,
            activate_fee_max_usdt: 1_500_000,
            transfer_fee_max_usdt: 500_000,
            min_deposit_usdt: 1_000_000,
            expected_beacon_implementation: "b8eda40b467b45af107f198e94cc2fa1378adf50".into(),
            expected_controller_implementation: "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441".into(),
        }
    }

    /// TronGrid answering `getcontract` with `contract`, and `getGasFreeAddress` with `word` if given.
    async fn chain_answering(contract: serde_json::Value, word: Option<String>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/wallet/getcontract"))
            .respond_with(ResponseTemplate::new(200).set_body_json(contract))
            .mount(&server)
            .await;
        if let Some(word) = word {
            Mock::given(method("POST"))
                .and(path("/wallet/triggerconstantcontract"))
                .and(body_string_contains("getGasFreeAddress(address)"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"constant_result": [word]})))
                .mount(&server)
                .await;
        }
        server
    }

    #[tokio::test]
    async fn the_boot_check_passes_only_when_the_controller_agrees() {
        let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();
        let ours = format!(
            "{:0>64}",
            hex::encode(
                &bs58::decode(gasfree::gasfree_address(&gasfree::NILE, &deriver.address_at(0).unwrap()).unwrap())
                    .with_check(Some(0x41))
                    .into_vec()
                    .unwrap()[1..]
            )
        );
        let contract = serde_json::json!({"contract_address": "41575eb3ab6dfe7d69a6dc0e2cc0c72fa9e2d38e8b", "bytecode": ""});

        let agrees = chain_answering(contract.clone(), Some(ours)).await;
        assert_eq!(GasFreeChain::new(agrees.uri(), "k".into()).self_test(&nile(), &deriver).await, SelfTest::Passed);

        let disagrees = chain_answering(contract, Some(format!("{:0>64}", "ab".repeat(20)))).await;
        let got = GasFreeChain::new(disagrees.uri(), "k".into()).self_test(&nile(), &deriver).await;
        assert!(matches!(&got, SelfTest::Failed(e) if e.contains("derives")), "{got:?}");

        let other_network = chain_answering(serde_json::json!({}), None).await;
        let got = GasFreeChain::new(other_network.uri(), "k".into()).self_test(&nile(), &deriver).await;
        assert!(matches!(&got, SelfTest::Failed(e) if e.contains("APP_GASFREE_NETWORK")), "{got:?}");

        let down = MockServer::start().await; // 404 to everything
        let got = GasFreeChain::new(down.uri(), "k".into()).self_test(&nile(), &deriver).await;
        assert!(matches!(got, SelfTest::Unreachable(_)), "{got:?}");
    }

    #[tokio::test]
    async fn an_account_is_activated_only_with_a_contract_record_and_anything_odd_is_an_error() {
        let activated = chain_answering(serde_json::json!({"contract_address": "41ab", "bytecode": ""}), None).await;
        assert_eq!(
            GasFreeChain::new(activated.uri(), "k".into()).has_contract("TX").await,
            Ok(true),
            "an empty bytecode is still a contract"
        );
        let unused = chain_answering(serde_json::json!({}), None).await;
        assert_eq!(GasFreeChain::new(unused.uri(), "k".into()).has_contract("TX").await, Ok(false));
        let odd = chain_answering(serde_json::json!({"Error": "rate limited"}), None).await;
        assert!(GasFreeChain::new(odd.uri(), "k".into()).has_contract("TX").await.is_err(), "an error body is not 'no contract'");
    }
}
