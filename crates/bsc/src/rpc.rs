//! A BSC JSON-RPC client. It holds no key: the reads both services need, and `send_raw_transaction`,
//! which broadcasts bytes the signer has already signed (`tron-signer`'s `bsc_sweep.rs`).

use std::time::Duration;

use serde_json::{json, Value};

use crate::{address_word, decode_transfer, hex_u128, normalize_address, normalize_tx_hash, Transfer, TRANSFER_TOPIC};

/// `decimals()` and `balanceOf(address)`.
const DECIMALS_SELECTOR: &str = "0x313ce567";
const BALANCE_OF_SELECTOR: &str = "70a08231";

pub struct Client {
    http: reqwest::Client,
    url: String,
}

/// A mined transaction's outcome, and the Transfers it emitted.
#[derive(Debug, Clone)]
pub struct Receipt {
    /// `status == 1`. A reverted transaction keeps its logs out of the receipt, and is checked anyway.
    pub succeeded: bool,
    pub block_number: u64,
    pub transfers: Vec<Transfer>,
}

impl Client {
    pub fn new(url: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_default();
        Self { http, url: url.to_string() }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let resp = self
            .http
            .post(&self.url)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .map_err(|e| format!("bsc {method}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("bsc {method}: HTTP {}", resp.status()));
        }
        let mut body: Value = resp.json().await.map_err(|e| format!("bsc {method}: {e}"))?;
        if let Some(err) = body.get("error") {
            return Err(format!("bsc {method}: {err}"));
        }
        // An absent `result` is an error, never a null: "could not read" must not look like "nothing".
        match body.get_mut("result") {
            Some(result) => Ok(result.take()),
            None => Err(format!("bsc {method}: no result in {body}")),
        }
    }

    fn quantity(method: &str, v: &Value) -> Result<u64, String> {
        v.as_str()
            .and_then(hex_u128)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| format!("bsc {method}: not a quantity: {v}"))
    }

    pub async fn chain_id(&self) -> Result<u64, String> {
        Self::quantity("eth_chainId", &self.call("eth_chainId", json!([])).await?)
    }

    pub async fn block_number(&self) -> Result<u64, String> {
        Self::quantity("eth_blockNumber", &self.call("eth_blockNumber", json!([])).await?)
    }

    /// `Ok(None)` while the transaction is unknown or not yet mined.
    pub async fn receipt(&self, tx_hash: &str) -> Result<Option<Receipt>, String> {
        let r = self.call("eth_getTransactionReceipt", json!([tx_hash])).await?;
        if r.is_null() {
            return Ok(None);
        }
        let block_number = Self::quantity("eth_getTransactionReceipt", &r["blockNumber"])?;
        let succeeded = r["status"].as_str().and_then(hex_u128) == Some(1);
        let transfers = r["logs"].as_array().map(|logs| logs.iter().filter_map(decode_transfer).collect()).unwrap_or_default();
        Ok(Some(Receipt { succeeded, block_number, transfers }))
    }

    /// Every Transfer of `contract` to `to` in blocks `from_block..=to_block`.
    pub async fn transfers_to(&self, contract: &str, to: &str, from_block: u64, to_block: u64) -> Result<Vec<Transfer>, String> {
        let topic = address_word(to).ok_or_else(|| format!("{to} is not a BSC address"))?;
        let logs = self
            .call(
                "eth_getLogs",
                json!([{
                    "address": contract,
                    "fromBlock": format!("0x{from_block:x}"),
                    "toBlock": format!("0x{to_block:x}"),
                    "topics": [TRANSFER_TOPIC, null, topic],
                }]),
            )
            .await?;
        let logs = logs.as_array().ok_or_else(|| format!("bsc eth_getLogs: not a list: {logs}"))?;
        // Re-checked, not trusted: a node that ignored the filter must not credit a stranger's log.
        let contract = normalize_address(contract).unwrap_or_default();
        let to = normalize_address(to).unwrap_or_default();
        Ok(logs.iter().filter_map(decode_transfer).filter(|t| t.contract == contract && t.to == to).collect())
    }

    /// Every Transfer of `contract` to any of `tos` in blocks `from_block..=to_block`: one
    /// `eth_getLogs`, the recipients OR-ed in the third topic. Callers chunk `tos`; an empty list asks
    /// nothing, because an empty OR would match every recipient.
    pub async fn transfers_to_any(&self, contract: &str, tos: &[String], from_block: u64, to_block: u64) -> Result<Vec<Transfer>, String> {
        if tos.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: Vec<String> = tos.iter().filter_map(|t| normalize_address(t)).collect();
        let topics: Vec<String> = wanted.iter().filter_map(|t| address_word(t)).collect();
        if topics.len() != tos.len() {
            return Err(format!("not every recipient is a BSC address: {tos:?}"));
        }
        let logs = self
            .call(
                "eth_getLogs",
                json!([{
                    "address": contract,
                    "fromBlock": format!("0x{from_block:x}"),
                    "toBlock": format!("0x{to_block:x}"),
                    "topics": [TRANSFER_TOPIC, null, topics],
                }]),
            )
            .await?;
        let logs = logs.as_array().ok_or_else(|| format!("bsc eth_getLogs: not a list: {logs}"))?;
        let contract = normalize_address(contract).unwrap_or_default();
        Ok(logs.iter().filter_map(decode_transfer).filter(|t| t.contract == contract && wanted.contains(&t.to)).collect())
    }

    /// A block's timestamp, in seconds.
    pub async fn block_timestamp(&self, number: u64) -> Result<u64, String> {
        let b = self.call("eth_getBlockByNumber", json!([format!("0x{number:x}"), false])).await?;
        Self::quantity("eth_getBlockByNumber", &b["timestamp"])
    }

    async fn view(&self, contract: &str, data: String) -> Result<u128, String> {
        let r = self.call("eth_call", json!([{"to": contract, "data": data}, "latest"])).await?;
        r.as_str()
            .filter(|w| w.len() == 66)
            .and_then(hex_u128)
            .ok_or_else(|| format!("bsc eth_call to {contract}: not one word below 2^128: {r}"))
    }

    pub async fn balance_of(&self, contract: &str, owner: &str) -> Result<u128, String> {
        let word = address_word(owner).ok_or_else(|| format!("{owner} is not a BSC address"))?;
        self.view(contract, format!("0x{BALANCE_OF_SELECTOR}{}", &word[2..])).await
    }

    /// BNB held by `address`, in wei.
    pub async fn balance(&self, address: &str) -> Result<u128, String> {
        let r = self.call("eth_getBalance", json!([address, "latest"])).await?;
        r.as_str().and_then(hex_u128).ok_or_else(|| format!("bsc eth_getBalance: not a quantity: {r}"))
    }

    pub async fn gas_price(&self) -> Result<u128, String> {
        let r = self.call("eth_gasPrice", json!([])).await?;
        r.as_str().and_then(hex_u128).ok_or_else(|| format!("bsc eth_gasPrice: not a quantity: {r}"))
    }

    /// The next nonce of `address`, counting mined transactions only (`pending == false`) or the
    /// node's pool too. The two differ while a transaction from `address` is waiting.
    pub async fn nonce(&self, address: &str, pending: bool) -> Result<u64, String> {
        let tag = if pending { "pending" } else { "latest" };
        Self::quantity("eth_getTransactionCount", &self.call("eth_getTransactionCount", json!([address, tag])).await?)
    }

    /// The gas a call from `from` to `to` with `data` would use now.
    pub async fn estimate_gas(&self, from: &str, to: &str, data: &str) -> Result<u64, String> {
        Self::quantity("eth_estimateGas", &self.call("eth_estimateGas", json!([{"from": from, "to": to, "data": data}])).await?)
    }

    /// Broadcast a signed transaction (`0x` + its hex). Answers its hash as the node wrote it.
    pub async fn send_raw_transaction(&self, raw: &str) -> Result<String, String> {
        let r = self.call("eth_sendRawTransaction", json!([raw])).await?;
        r.as_str().and_then(normalize_tx_hash).ok_or_else(|| format!("bsc eth_sendRawTransaction: not a hash: {r}"))
    }

    pub async fn decimals(&self, contract: &str) -> Result<u8, String> {
        let d = self.view(contract, DECIMALS_SELECTOR.to_string()).await?;
        u8::try_from(d).map_err(|_| format!("decimals() of {contract} is {d}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ok(result: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": result}))
    }

    #[tokio::test]
    async fn balance_and_head_read() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_blockNumber"})))
            .respond_with(ok(json!("0x2a")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(ok(json!(format!("0x{:064x}", 3u128 * 10u128.pow(18)))))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri());
        assert_eq!(c.block_number().await.unwrap(), 42);
        let owner = "0x2222222222222222222222222222222222222222";
        assert_eq!(c.balance_of("0x55d398326f99059ff775485246999027b3197955", owner).await.unwrap(), 3 * 10u128.pow(18));
    }

    #[tokio::test]
    async fn an_rpc_error_or_missing_result_is_an_error_not_a_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32000, "message": "busy"}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_blockNumber"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1})))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri());
        assert!(c.balance_of("0x55d398326f99059ff775485246999027b3197955", "0x2222222222222222222222222222222222222222").await.is_err());
        assert!(c.block_number().await.is_err());
    }

    #[tokio::test]
    async fn transfers_to_any_filters_on_every_recipient_and_rechecks_them() {
        let server = MockServer::start().await;
        let usdt = "0x55d398326f99059ff775485246999027b3197955";
        let a = "0x1111111111111111111111111111111111111111";
        let b = "0x2222222222222222222222222222222222222222";
        let stranger = "0x3333333333333333333333333333333333333333";
        let log = |to: &str| {
            json!({
                "address": usdt,
                "topics": [TRANSFER_TOPIC, address_word(stranger).unwrap(), address_word(to).unwrap()],
                "data": format!("0x{:064x}", 5u128),
                "transactionHash": format!("0x{}", "ab".repeat(32)),
                "logIndex": "0x0",
                "blockNumber": "0x10",
            })
        };
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_getLogs", "params": [{"topics": [TRANSFER_TOPIC, null, [address_word(a).unwrap(), address_word(b).unwrap()]]}]})))
            .respond_with(ok(json!([log(a), log(b), log(stranger)])))
            .mount(&server)
            .await;
        let c = Client::new(&server.uri());
        let got = c.transfers_to_any(usdt, &[a.to_string(), b.to_string()], 1, 20).await.unwrap();
        assert_eq!(got.iter().map(|t| t.to.as_str()).collect::<Vec<_>>(), [a, b], "a node that ignored the filter credits nobody else");
        assert!(c.transfers_to_any(usdt, &[], 1, 20).await.unwrap().is_empty(), "an empty list asks nothing");
    }

    #[tokio::test]
    async fn an_unmined_receipt_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ok(Value::Null)).mount(&server).await;
        assert!(Client::new(&server.uri()).receipt(&format!("0x{}", "ab".repeat(32))).await.unwrap().is_none());
    }
}
