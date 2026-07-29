//! Port of clutch-hub-api's `clutch_node_client` (client.rs + connection.rs + types.rs merged
//! into one module): persistent WebSocket JSON-RPC client to clutch-node, a background
//! reconnect loop, and a oneshot-per-request demux with a 10s timeout. Ride-listing helpers
//! (`list_ride_requests` etc.) are dropped — out of scope for the treasury.

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{error, info};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
struct JSONRPCRequest {
    jsonrpc: String,
    method: String,
    params: serde_json::Value,
    id: String,
}

#[derive(Serialize, Deserialize)]
struct JSONRPCResponse {
    jsonrpc: String,
    result: Option<serde_json::Value>,
    error: Option<JSONRPCError>,
    id: String,
}

#[derive(Serialize, Deserialize)]
struct JSONRPCError {
    code: i32,
    message: String,
    data: Option<serde_json::Value>,
}

pub struct NodeClient {
    ws_sink: Arc<Mutex<Option<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>>,
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

impl NodeClient {
    /// Creates a new client and starts the background connection task.
    pub fn new(url: String) -> Arc<Self> {
        let ws_sink = Arc::new(Mutex::new(None));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));

        let ws_sink_clone = ws_sink.clone();
        let pending_requests_clone = pending_requests.clone();
        tokio::spawn(async move {
            start_connection_loop(url, ws_sink_clone, pending_requests_clone).await;
        });

        Arc::new(NodeClient {
            ws_sink,
            pending_requests,
        })
    }

    /// Sends a request and awaits the response.
    pub async fn send_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = Uuid::new_v4().to_string();

        // send_raw_transaction takes a bare JSON string, not an object — special-cased to
        // match the node's RPC contract (see clutch-hub-api client.rs).
        let request = if method == "send_raw_transaction" {
            let tx_string = match &params {
                serde_json::Value::String(s) => s.clone(),
                _ => params.as_str().unwrap_or_default().to_string(),
            };
            JSONRPCRequest {
                jsonrpc: "2.0".to_string(),
                method: method.to_string(),
                params: serde_json::Value::String(tx_string),
                id: id.clone(),
            }
        } else {
            JSONRPCRequest {
                jsonrpc: "2.0".to_string(),
                method: method.to_string(),
                params,
                id: id.clone(),
            }
        };

        let request_json = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        info!("Sending request to node: {}", request_json);

        let mut ws_sink_lock = self.ws_sink.lock().await;
        if let Some(ws_sink) = ws_sink_lock.as_mut() {
            let (resp_tx, resp_rx) = oneshot::channel();

            {
                let mut pending = self.pending_requests.lock().await;
                pending.insert(id.clone(), resp_tx);
            }

            if let Err(e) = ws_sink.send(Message::Text(request_json)).await {
                let mut pending = self.pending_requests.lock().await;
                pending.remove(&id);
                return Err(format!("Failed to send request: {}", e));
            }

            let response_result = timeout(Duration::from_secs(10), resp_rx).await;

            match response_result {
                Ok(Ok(response_json)) => {
                    if response_json.is_empty() {
                        return Err("Connection lost before receiving response".to_string());
                    }

                    info!("Received response: {}", response_json);

                    let response: JSONRPCResponse =
                        serde_json::from_str(&response_json).map_err(|e| e.to_string())?;
                    if response.id != id {
                        return Err("Mismatched response ID".to_string());
                    }
                    if let Some(error) = response.error {
                        Err(error.message)
                    } else if let Some(result) = response.result {
                        Ok(result)
                    } else {
                        Err("No result or error in response".to_string())
                    }
                }
                Ok(Err(_)) => Err("Failed to receive response".to_string()),
                Err(_) => {
                    let mut pending = self.pending_requests.lock().await;
                    pending.remove(&id);
                    Err("Request timed out".to_string())
                }
            }
        } else {
            Err("WebSocket connection not established".to_string())
        }
    }

    /// Gets the next nonce value for the given address. Propagates node errors rather than
    /// falling back to a guessed nonce (a down node silently producing nonce 1 would either
    /// collide with an already-used nonce or skip ahead).
    pub async fn get_next_nonce(&self, address: &str) -> Result<u64, String> {
        let result = self
            .send_request("get_next_nonce", serde_json::json!({ "address": address }))
            .await
            .map_err(|e| format!("Failed to get nonce for address {}: {}", address, e))?;

        result
            .get("nonce")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| {
                format!(
                    "Failed to parse nonce value from node response for address {}: {:?}",
                    address, result
                )
            })
    }

    /// Gets the current balance for the given address. Propagates node errors rather than
    /// falling back to balance 0, which callers could mistake for "known-empty".
    pub async fn get_account_balance(&self, address: &str) -> Result<u64, String> {
        let result = self
            .send_request("get_account_balance", serde_json::json!({ "address": address }))
            .await
            .map_err(|e| format!("Failed to get balance for address {}: {}", address, e))?;

        result
            .get("balance")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| {
                format!(
                    "Failed to parse balance value from node response for address {}: {:?}",
                    address, result
                )
            })
    }

    /// Fetches genesis-committed chain parameters plus `total_supply` and `latest_block_index`.
    pub async fn get_chain_info(&self) -> Result<ChainInfo, String> {
        let v = self.send_request("get_chain_info", serde_json::Value::Null).await?;
        serde_json::from_value(v).map_err(|e| format!("bad get_chain_info payload: {}", e))
    }

    /// `index` is the node's own param field name (`GetBlockByIndexParams`, node
    /// wss/websocket.rs) — confirmed directly against the handler, not assumed.
    pub async fn get_block_by_index(&self, index: u64) -> Result<serde_json::Value, String> {
        self.send_request("get_block_by_index", serde_json::json!({ "index": index })).await
    }

    pub async fn send_raw_transaction(&self, raw_hex: &str) -> Result<serde_json::Value, String> {
        self.send_request("send_raw_transaction", serde_json::Value::String(raw_hex.to_string()))
            .await
    }
}

async fn start_connection_loop(
    url: String,
    ws_sink: Arc<Mutex<Option<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>>,
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
) {
    loop {
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                info!("Connected to clutch-node at {}", url);
                let (sink, mut stream) = ws_stream.split();

                {
                    let mut ws_sink_lock = ws_sink.lock().await;
                    *ws_sink_lock = Some(sink);
                }

                let pending_requests_clone = pending_requests.clone();
                let ws_sink_clone = ws_sink.clone();

                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            handle_incoming_message(text, pending_requests_clone.clone()).await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            error!("WebSocket error: {}", e);
                            break;
                        }
                    }
                }

                {
                    let mut ws_sink_lock = ws_sink_clone.lock().await;
                    *ws_sink_lock = None;
                }

                let mut pending = pending_requests.lock().await;
                for (_, sender) in pending.drain() {
                    let _ = sender.send("".to_string());
                }

                info!("Connection to clutch-node lost");
            }
            Err(e) => {
                error!("Failed to connect to clutch-node at {}: {}", url, e);
            }
        }

        let retry_seconds = 5;
        error!("Reconnecting to clutch-node in {} seconds...", retry_seconds);
        tokio::time::sleep(Duration::from_secs(retry_seconds)).await;
    }
}

async fn handle_incoming_message(
    text: String,
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
) {
    match serde_json::from_str::<JSONRPCResponse>(&text) {
        Ok(response) => {
            let mut pending = pending_requests.lock().await;
            if let Some(resp_tx) = pending.remove(&response.id) {
                let _ = resp_tx.send(text);
            } else {
                info!("Received unexpected message: {}", text);
            }
        }
        Err(e) => {
            error!("Failed to parse response: {}. Error: {}", text, e);
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChainInfo {
    pub chain_id: u64,
    pub is_testnet: bool,
    pub tx_fee: u64,
    pub mint_authority: String,
    /// A decimal STRING on the wire, not a JSON number — it is the one field that can exceed
    /// 2^53 (~$9B at this peg), so the node deliberately stringifies it and every other numeric
    /// field stays bare. Verified live against the running node. Deserializing this as `u64`
    /// fails at runtime, and reconciliation treats a supply mismatch as a P1 — so parse it,
    /// never coerce it.
    #[serde(deserialize_with = "de_u64_from_str")]
    pub total_supply: u64,
    pub latest_block_index: u64,
}

fn de_u64_from_str<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    s.parse::<u64>().map_err(serde::de::Error::custom)
}
