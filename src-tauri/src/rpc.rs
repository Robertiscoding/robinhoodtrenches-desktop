//! JSON-RPC to the chain: batched HTTP for reads, a websocket for `eth_subscribe`.
//!
//! The same client serves a shared public node (paced, 3 in flight, small
//! log windows) and a paid one (unpaced, 8 in flight, wide windows). Errors
//! are classified so the scheduler can react: a rate limit is retried here
//! with backoff, a too-large query is handed back so the caller can split it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio_tungstenite::tungstenite::Message;

pub const PUBLIC_HTTP: &str = "https://rpc.mainnet.chain.robinhood.com";
pub const CHAIN_ID: u64 = 4663;

#[derive(Debug, Clone)]
pub enum RpcError {
    RateLimited,
    TooLarge,
    Transport(String),
    Node(i64, String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::RateLimited => write!(f, "rate limited"),
            RpcError::TooLarge => write!(f, "query too large"),
            RpcError::Transport(s) => write!(f, "transport: {s}"),
            RpcError::Node(c, m) => write!(f, "node {c}: {m}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, RpcError>;

#[derive(Debug, Clone)]
pub struct Log {
    pub block: u64,
    pub log_index: u32,
    pub tx: String,
    pub topics: Vec<String>,
    pub data: String,
}

pub fn hex_u64(s: &str) -> u64 {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

pub fn parse_log(v: &Value) -> Option<Log> {
    let topics = v["topics"]
        .as_array()?
        .iter()
        .filter_map(|t| t.as_str().map(|s| s.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    Some(Log {
        block: hex_u64(v["blockNumber"].as_str()?),
        log_index: hex_u64(v["logIndex"].as_str()?) as u32,
        tx: v["transactionHash"].as_str()?.to_ascii_lowercase(),
        topics,
        data: v["data"].as_str()?.to_ascii_lowercase(),
    })
}

pub struct Rpc {
    http: reqwest::Client,
    pub url: String,
    pub ws_url: Option<String>,
    pub paid: bool,
    /// how many log scans the backfill may keep in flight
    pub concurrency: usize,
    /// opening block window for a log scan; halves on TooLarge, grows on success
    pub chunk0: u64,
    sem: Semaphore,
    last: Mutex<Instant>,
    min_gap: Duration,
}

impl Rpc {
    pub fn new(http: reqwest::Client, url: String, ws_url: Option<String>) -> Self {
        let paid = url != PUBLIC_HTTP;
        let (concurrency, chunk0, min_gap) = if paid {
            (8, 250_000, Duration::ZERO)
        } else {
            (3, 50_000, Duration::from_millis(120))
        };
        Rpc {
            http,
            url,
            ws_url: ws_url.filter(|s| !s.is_empty()),
            paid,
            concurrency,
            chunk0,
            sem: Semaphore::new(if paid { 16 } else { 4 }),
            last: Mutex::new(Instant::now() - Duration::from_secs(1)),
            min_gap,
        }
    }

    async fn pace(&self) {
        if self.min_gap.is_zero() {
            return;
        }
        let mut last = self.last.lock().await;
        let wait = (*last + self.min_gap).saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        *last = Instant::now();
    }

    async fn post(&self, body: &Value) -> Result<Value> {
        let _permit = self.sem.acquire().await.map_err(|e| RpcError::Transport(e.to_string()))?;
        self.pace().await;
        let r = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        let st = r.status();
        if st.as_u16() == 429 {
            return Err(RpcError::RateLimited);
        }
        if !st.is_success() {
            return Err(RpcError::Transport(format!("http {}", st.as_u16())));
        }
        let bytes = r.bytes().await.map_err(|e| RpcError::Transport(e.to_string()))?;
        serde_json::from_slice::<Value>(&bytes).map_err(|e| RpcError::Transport(e.to_string()))
    }

    fn classify(err: &Value) -> RpcError {
        let code = err["code"].as_i64().unwrap_or(0);
        let msg = err["message"].as_str().unwrap_or("").to_string();
        let m = msg.to_ascii_lowercase();
        if m.contains("too many requests") || m.contains("rate limit") || code == -32005 || code == 429 {
            return RpcError::RateLimited;
        }
        if m.contains("exceed")
            || m.contains("limit")
            || m.contains("too many")
            || m.contains("timed out")
            || m.contains("timeout")
            || m.contains("range")
            || m.contains("too large")
            || m.contains("response size")
        {
            return RpcError::TooLarge;
        }
        RpcError::Node(code, msg)
    }

    async fn backoff(attempt: u32) {
        let base = 200u64 * (1u64 << attempt.min(6));
        let jitter = (Instant::now().elapsed().subsec_nanos() as u64) % 150;
        tokio::time::sleep(Duration::from_millis(base + jitter)).await;
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut attempt = 0u32;
        loop {
            match self.post(&body).await {
                Ok(v) => {
                    if let Some(e) = v.get("error") {
                        let e = Self::classify(e);
                        if matches!(e, RpcError::RateLimited) && attempt < 7 {
                            attempt += 1;
                            Self::backoff(attempt).await;
                            continue;
                        }
                        return Err(e);
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(RpcError::RateLimited) if attempt < 7 => {
                    attempt += 1;
                    Self::backoff(attempt).await;
                }
                Err(RpcError::Transport(_)) if attempt < 3 => {
                    attempt += 1;
                    Self::backoff(attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One HTTP round trip for many calls. Results come back in call order.
    pub async fn batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>> {
        if calls.is_empty() {
            return Ok(vec![]);
        }
        let body = Value::Array(
            calls
                .iter()
                .enumerate()
                .map(|(i, (m, p))| json!({ "jsonrpc": "2.0", "id": i, "method": m, "params": p }))
                .collect(),
        );
        let mut attempt = 0u32;
        loop {
            match self.post(&body).await {
                Ok(Value::Array(items)) => {
                    let mut out = vec![Value::Null; calls.len()];
                    let mut limited = false;
                    for it in &items {
                        if let Some(e) = it.get("error") {
                            match Self::classify(e) {
                                RpcError::RateLimited => limited = true,
                                other => return Err(other),
                            }
                        }
                        if let Some(id) = it["id"].as_u64() {
                            if (id as usize) < out.len() {
                                out[id as usize] = it.get("result").cloned().unwrap_or(Value::Null);
                            }
                        }
                    }
                    if limited && attempt < 7 {
                        attempt += 1;
                        Self::backoff(attempt).await;
                        continue;
                    }
                    return Ok(out);
                }
                Ok(v) => {
                    // a single error object instead of an array: the whole batch was refused
                    if let Some(e) = v.get("error") {
                        let e = Self::classify(e);
                        if matches!(e, RpcError::RateLimited) && attempt < 7 {
                            attempt += 1;
                            Self::backoff(attempt).await;
                            continue;
                        }
                        return Err(e);
                    }
                    return Err(RpcError::Transport("bad batch response".into()));
                }
                Err(RpcError::RateLimited) if attempt < 7 => {
                    attempt += 1;
                    Self::backoff(attempt).await;
                }
                Err(RpcError::Transport(_)) if attempt < 3 => {
                    attempt += 1;
                    Self::backoff(attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn chain_id(&self) -> Result<u64> {
        Ok(hex_u64(self.call("eth_chainId", json!([])).await?.as_str().unwrap_or("0x0")))
    }

    pub async fn block_number(&self) -> Result<u64> {
        Ok(hex_u64(self.call("eth_blockNumber", json!([])).await?.as_str().unwrap_or("0x0")))
    }

    pub async fn get_logs(&self, from: u64, to: u64, address: &str, topics: &Value) -> Result<Vec<Log>> {
        let params = json!([{
            "fromBlock": format!("0x{from:x}"),
            "toBlock": format!("0x{to:x}"),
            "address": address,
            "topics": topics,
        }]);
        let v = self.call("eth_getLogs", params).await?;
        Ok(v.as_array().map(|a| a.iter().filter_map(parse_log).collect()).unwrap_or_default())
    }

    /// Timestamps for a set of blocks, 100 per batch, batches in parallel.
    pub async fn block_timestamps(&self, nums: &[u64]) -> Result<Vec<(u64, u64)>> {
        let mut out = Vec::with_capacity(nums.len());
        let futs = nums.chunks(100).map(|chunk| async move {
            let calls: Vec<(&str, Value)> = chunk
                .iter()
                .map(|n| ("eth_getBlockByNumber", json!([format!("0x{n:x}"), false])))
                .collect();
            let res = self.batch(&calls).await?;
            Ok::<_, RpcError>(
                chunk
                    .iter()
                    .zip(res)
                    .filter_map(|(n, v)| v["timestamp"].as_str().map(|t| (*n, hex_u64(t))))
                    .collect::<Vec<_>>(),
            )
        });
        for r in futures_util::future::join_all(futs).await {
            out.extend(r?);
        }
        Ok(out)
    }


    /// Open a websocket and subscribe to logs. `None` when this node has no
    /// websocket or the upgrade fails; the caller then polls instead. The
    /// returned receiver closes when the socket does.
    pub async fn subscribe_logs(self: &Arc<Self>, address: String, topics: Value) -> Option<mpsc::Receiver<Log>> {
        let url = self.ws_url.clone()?;
        let (ws, _) = tokio::time::timeout(Duration::from_secs(8), tokio_tungstenite::connect_async(url))
            .await
            .ok()?
            .ok()?;
        let (mut tx, mut rx) = ws.split();
        let sub = json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_subscribe",
            "params": ["logs", { "address": address, "topics": topics }] });
        tx.send(Message::Text(sub.to_string().into())).await.ok()?;
        // first frame is the subscription ack
        let ack = tokio::time::timeout(Duration::from_secs(8), rx.next()).await.ok()??.ok()?;
        let ack: Value = serde_json::from_str(ack.to_text().ok()?).ok()?;
        ack.get("result")?.as_str()?;

        let (out, out_rx) = mpsc::channel::<Log>(1024);
        tauri::async_runtime::spawn(async move {
            let mut ping = tokio::time::interval(Duration::from_secs(25));
            ping.tick().await;
            loop {
                tokio::select! {
                    frame = rx.next() => match frame {
                        Some(Ok(Message::Text(t))) => {
                            if let Ok(v) = serde_json::from_str::<Value>(&t) {
                                if let Some(l) = parse_log(&v["params"]["result"]) {
                                    if out.send(l).await.is_err() { break; }
                                }
                            }
                        }
                        Some(Ok(Message::Ping(p))) => { let _ = tx.send(Message::Pong(p)).await; }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    },
                    _ = ping.tick() => {
                        if tx.send(Message::Ping(vec![].into())).await.is_err() { break; }
                    }
                }
            }
        });
        Some(out_rx)
    }
}
