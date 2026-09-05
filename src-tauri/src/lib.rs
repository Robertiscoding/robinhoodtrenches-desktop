//! robinhoodtrenches — native core.
//!
//! The webview owns the pixels; this side owns the wire. One pooled HTTP/2
//! client answers every `/api/*` call (keep-alive, brotli/gzip, no CORS
//! preflights, no browser cache layer), and one long-lived websocket task
//! streams fills straight into the page as events, reconnecting on its own.

mod config;
mod index;
mod rpc;
mod store;
mod v4;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use index::{Indexer, TokenView};
use rpc::Rpc;
use store::Store;

const ORIGIN: &str = "https://robinhoodtrenches.com";
const WS_URL: &str = "wss://robinhoodtrenches.com/ws";
const UA: &str = concat!("robinhoodtrenches-desktop/", env!("CARGO_PKG_VERSION"));

/// Shared network state: the pooled client and the handle of the socket task.
struct Net {
    http: reqwest::Client,
    ws: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

/* ------------------------------------------------------------------ http */

/// GET `/api{path}?{query}` and hand the raw JSON text back to the page.
/// The body is parsed exactly once, in the webview -- re-encoding it here
/// would only add work between the wire and the screen.
#[tauri::command]
async fn api(net: State<'_, Net>, path: String, query: String) -> Result<String, String> {
    if !path.starts_with('/') || path.contains("..") || path.contains("://") {
        return Err("bad path".into());
    }
    let url = format!("{ORIGIN}/api{path}?{query}");
    let resp = net
        .http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("{path} {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{path} {}", status.as_u16()));
    }
    resp.text().await.map_err(|e| format!("{path} {e}"))
}

/* ---------------------------------------------------------------- links */

/// Every link on the tape points off-app; they open in the default browser.
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("refused".into());
    }
    open::that_detached(url).map_err(|e| e.to_string())
}

/* --------------------------------------------------------------- socket */

/// Cheap jitter without a crate: 0..=span_ms from the clock's nanoseconds.
fn jitter_ms(span_ms: u64) -> u64 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    n % (span_ms + 1)
}

/// Keeps one websocket open to the tape for the life of the page.
///
/// Events to the page:
///   "ws"    payload: raw JSON text of a server frame ({type:'fills'|'hello', data})
///   "link"  payload: "open" | "closed"
///
/// The page keeps the same open-once / failure-count bookkeeping the site
/// does and falls back to polling `/api/tape` when the socket will not stay
/// up, so behaviour is identical on networks that block upgrades.
async fn ws_loop(app: AppHandle) {
    loop {
        let req = match WS_URL.into_client_request() {
            Ok(mut r) => {
                let h = r.headers_mut();
                h.insert("Origin", ORIGIN.parse().unwrap());
                h.insert("User-Agent", UA.parse().unwrap());
                r
            }
            Err(_) => return,
        };

        let connected = tokio::time::timeout(
            Duration::from_secs(8),
            tokio_tungstenite::connect_async(req),
        )
        .await;

        if let Ok(Ok((ws, _))) = connected {
            let _ = app.emit("link", "open");
            let (mut tx, mut rx) = ws.split();
            // 20s application-level ping, like the site: keeps proxies from idling us out
            let mut ping = tokio::time::interval(Duration::from_secs(20));
            ping.tick().await; // the first tick completes immediately; skip it
            loop {
                tokio::select! {
                    frame = rx.next() => match frame {
                        Some(Ok(Message::Text(t))) => { let _ = app.emit("ws", t.as_str()); }
                        Some(Ok(Message::Binary(b))) => {
                            if let Ok(t) = std::str::from_utf8(&b) { let _ = app.emit("ws", t); }
                        }
                        Some(Ok(Message::Ping(p))) => { let _ = tx.send(Message::Pong(p)).await; }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    },
                    _ = ping.tick() => {
                        if tx.send(Message::Text("p".into())).await.is_err() { break; }
                    }
                }
            }
        }
        let _ = app.emit("link", "closed");
        // jittered 2-6s before retrying, same spread as the web client
        tokio::time::sleep(Duration::from_millis(2000 + jitter_ms(4000))).await;
    }
}

/// (Re)start the socket task. Called by the page once its listeners are
/// registered, so the server's `hello` frame is never emitted into the void.
#[tauri::command]
fn ws_start(app: AppHandle, net: State<'_, Net>) {
    let mut slot = net.ws.lock().unwrap();
    if let Some(old) = slot.take() {
        old.abort();
    }
    *slot = Some(tauri::async_runtime::spawn(ws_loop(app)));
}

/* ---------------------------------------------------------------- index */

type Idx<'a> = State<'a, Arc<Indexer>>;

/// Resolve a token's pools, start indexing, return what the page needs to
/// price and label the chart. `hint` is a poolId the page already knows
/// (the site's dexscreener pair) and becomes the price source when valid.
#[tauri::command]
async fn token_open(idx: Idx<'_>, address: String, hint: Option<String>) -> Result<TokenView, String> {
    idx.inner().clone().open_token(&address, hint).await
}

/// Candles as raw little-endian f64: [t, o, h, l, c, volume_usd, trades] * n.
/// Binary, not JSON: 100k swaps fold into a few thousand candles that cross
/// IPC as one buffer the page reads without parsing.
#[tauri::command]
async fn token_candles(idx: Idx<'_>, address: String, tf: u64) -> Result<tauri::ipc::Response, String> {
    let ix = idx.inner().clone();
    let v = tauri::async_runtime::spawn_blocking(move || ix.candles(&address, tf))
        .await
        .map_err(|e| e.to_string())??;
    let mut bytes = Vec::with_capacity(v.len() * 8);
    for x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
async fn token_trades(idx: Idx<'_>, address: String, before_ts: u64, limit: usize) -> Result<Vec<Value>, String> {
    let ix = idx.inner().clone();
    tauri::async_runtime::spawn_blocking(move || ix.trades(&address, before_ts, limit))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn token_status(idx: Idx<'_>, address: String) -> Result<Value, String> {
    let ix = idx.inner().clone();
    tauri::async_runtime::spawn_blocking(move || ix.status(&address))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn rpc_get(idx: Idx<'_>) -> Result<Value, String> {
    let r = idx.rpc().await;
    Ok(json!({ "http": r.url, "ws": r.ws_url, "paid": r.paid }))
}

/// Point the indexer at a node. The endpoint must answer for chain 4663;
/// the websocket is optional and probed, not trusted.
#[tauri::command]
async fn rpc_set(app: AppHandle, idx: Idx<'_>, net: State<'_, Net>, http: String, ws: String) -> Result<Value, String> {
    let http = http.trim().to_string();
    let ws = ws.trim().to_string();
    if !http.starts_with("http") {
        return Err("http url must start with http(s)://".into());
    }
    if !ws.is_empty() && !ws.starts_with("ws") {
        return Err("websocket url must start with ws(s)://".into());
    }
    let rpc = Arc::new(Rpc::new(net.http.clone(), http.clone(), Some(ws.clone())));
    let chain = rpc.chain_id().await.map_err(|e| format!("node unreachable: {e}"))?;
    if chain != rpc::CHAIN_ID {
        return Err(format!("that node serves chain {chain}, not robinhood (4663)"));
    }
    let ws_ok = if ws.is_empty() {
        false
    } else {
        rpc.subscribe_logs(v4::POOL_MANAGER.into(), json!([v4::SWAP_TOPIC])).await.is_some()
    };
    config::save(&app, &config::Config { rpc_http: http, rpc_ws: ws })?;
    idx.set_rpc(rpc.clone()).await;
    Ok(json!({ "chain_id": chain, "ws_ok": ws_ok, "paid": rpc.paid }))
}

/* ----------------------------------------------------------------- app */

pub fn run() {
    let http = reqwest::Client::builder()
        .user_agent(UA)
        .https_only(true)
        .http2_adaptive_window(true)
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client");

    tauri::Builder::default()
        .manage(Net { http: http.clone(), ws: Mutex::new(None) })
        .invoke_handler(tauri::generate_handler![
            api, open_url, ws_start, token_open, token_candles, token_trades, token_status, rpc_get, rpc_set
        ])
        .setup(move |app| {
            // warm the TLS+H2 connection while the webview is still loading the page,
            // so the very first /api call already has a socket to ride on
            let client = http.clone();
            tauri::async_runtime::spawn(async move {
                let _ = client.get(concat!("https://robinhoodtrenches.com", "/api/status")).send().await;
            });

            // the on-chain indexer: node from config, SQLite in the app data dir
            let handle = app.handle().clone();
            let cfg = config::load(&handle);
            let rpc = Arc::new(Rpc::new(http.clone(), cfg.rpc_http, Some(cfg.rpc_ws)));
            let dir = handle.path().app_data_dir().expect("app data dir");
            std::fs::create_dir_all(&dir).expect("create app data dir");
            let store = Arc::new(Store::open(&dir.join("trench.db")).expect("open index db"));
            app.manage(Indexer::new(rpc, store, handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running robinhoodtrenches");
}
