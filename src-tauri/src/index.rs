//! The indexer: give it a token address, get every trade and a price series.
//!
//! Pipeline per token:
//!   discover  -- `Initialize` logs on the PoolManager filtered by the token,
//!                genesis to head, cached so later opens only scan new blocks
//!   rank      -- one `Swap` scan over all of the token's pools (OR-filter on
//!                the poolId topic) for the recent window; most active pool
//!                is the price source, the active set is what we index
//!   route     -- if the price pool is not quoted in USDG, find the quote's
//!                most active USDG pool and index it as the reference
//!   backfill  -- newest-first block chunks, N in flight, halving on
//!                "too large", coverage recorded per pool so restarts resume
//!   tail      -- one websocket subscription (or one poll loop) for every
//!                watched pool; live swaps land in the store and the page
//!
//! The chart is folded from the price pool's swaps; the trade list is every
//! swap across the active set, valued at the price pool's USD price.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::{Notify, RwLock};

use crate::rpc::{Rpc, RpcError};
use crate::store::{Pool, Row, Store, TokenMeta};
use crate::v4::{
    decode_init, decode_swap, pad_addr, price_c1_per_c0, scale, PoolInit, Swap, INIT_TOPIC, NATIVE, POOL_MANAGER,
    SWAP_TOPIC, USDG,
};

#[derive(Clone, Serialize)]
pub struct TokenView {
    pub token: TokenMeta,
    pub quote: TokenMeta,
    pub token_is_c0: bool,
    pub price_pool: String,
    pub price_pool_fee: u32,
    pub pools: Vec<Pool>,
    pub all_pools: usize,
    /// "direct" (quoted in USDG) | "via" (through a reference pool) | "none"
    pub usd: String,
    pub ref_pool: Option<String>,
    pub init_block: u64,
    pub head: u64,
}

#[derive(Clone)]
struct TokenCtx {
    view: TokenView,
    ref_pool: Option<Pool>,
    ref_quote_is_c0: bool,
}

pub struct Indexer {
    rpc: RwLock<Arc<Rpc>>,
    store: Arc<Store>,
    app: AppHandle,
    tokens: Mutex<HashMap<String, TokenCtx>>,
    watched: Mutex<HashSet<String>>,
    tail_notify: Arc<Notify>,
    jobs: Mutex<HashMap<String, Arc<AtomicU64>>>, // key -> running flag (1) / done (0)
}

type R<T> = std::result::Result<T, String>;

fn e<T: std::fmt::Display>(x: T) -> String {
    x.to_string()
}

impl Indexer {
    pub fn new(rpc: Arc<Rpc>, store: Arc<Store>, app: AppHandle) -> Arc<Self> {
        let me = Arc::new(Indexer {
            rpc: RwLock::new(rpc),
            store,
            app,
            tokens: Mutex::new(HashMap::new()),
            watched: Mutex::new(HashSet::new()),
            tail_notify: Arc::new(Notify::new()),
            jobs: Mutex::new(HashMap::new()),
        });
        tauri::async_runtime::spawn(me.clone().tail_loop());
        me
    }

    pub async fn rpc(&self) -> Arc<Rpc> {
        self.rpc.read().await.clone()
    }

    pub async fn set_rpc(&self, rpc: Arc<Rpc>) {
        *self.rpc.write().await = rpc;
        self.tail_notify.notify_one();
    }

    /* ------------------------------------------------------------ tokens */

    async fn token_meta(&self, rpc: &Rpc, addr: &str) -> R<TokenMeta> {
        if addr == NATIVE {
            return Ok(TokenMeta { address: addr.into(), symbol: "ETH".into(), name: "Ether".into(), decimals: 18 });
        }
        if let Some(t) = self.store.token_get(addr).map_err(e)? {
            return Ok(t);
        }
        let res = rpc
            .batch(&[
                ("eth_call", json!([{ "to": addr, "data": "0x95d89b41" }, "latest"])),
                ("eth_call", json!([{ "to": addr, "data": "0x313ce567" }, "latest"])),
                ("eth_call", json!([{ "to": addr, "data": "0x06fdde03" }, "latest"])),
            ])
            .await
            .map_err(e)?;
        let symbol = abi_string(res[0].as_str().unwrap_or("0x"));
        let decimals = crate::rpc::hex_u64(res[1].as_str().unwrap_or("0x12")) as i32;
        let name = abi_string(res[2].as_str().unwrap_or("0x"));
        let t = TokenMeta {
            address: addr.into(),
            symbol: if symbol.is_empty() { format!("{}…", &addr[..8]) } else { symbol },
            name,
            decimals: if decimals == 0 && res[1].as_str().is_none() { 18 } else { decimals },
        };
        self.store.token_put(&t).map_err(e)?;
        Ok(t)
    }

    /* ---------------------------------------------------------- discover */

    async fn discover(&self, rpc: &Rpc, addr: &str, head: u64) -> R<Vec<Pool>> {
        let from = self.store.discovered_get(addr).map_err(e)?.map(|b| b + 1).unwrap_or(0);
        if from <= head {
            let found = scan_inits(rpc, addr, from, head).await.map_err(e)?;
            self.store.pools_put(&found).map_err(e)?;
            self.store.discovered_put(addr, head).map_err(e)?;
        }
        self.store.pools_for(addr).map_err(e)
    }

    /// recent swap count per pool, from one OR-filtered scan over the set
    async fn rank(rpc: &Rpc, pools: &[Pool], head: u64) -> HashMap<String, u64> {
        let ids: Vec<&str> = pools.iter().map(|p| p.id.as_str()).collect();
        let topics = json!([SWAP_TOPIC, ids]);
        let oldest = pools.iter().map(|p| p.init_block).min().unwrap_or(head);
        let mut window: u64 = rpc.chunk0 * 4;
        let mut counts = HashMap::new();
        for _ in 0..6 {
            let from = head.saturating_sub(window).max(oldest);
            match rpc.get_logs(from, head, POOL_MANAGER, &topics).await {
                Ok(logs) => {
                    counts.clear();
                    for l in &logs {
                        if let Some(id) = l.topics.get(1) {
                            *counts.entry(id.clone()).or_insert(0u64) += 1;
                        }
                    }
                    if logs.is_empty() && from > oldest {
                        window *= 4;
                        continue;
                    }
                    break;
                }
                Err(RpcError::TooLarge) => {
                    window = (window / 2).max(2_000);
                }
                Err(_) => break,
            }
        }
        counts
    }

    /* -------------------------------------------------------------- open */

    pub async fn open_token(&self, addr: &str, hint: Option<String>) -> R<TokenView> {
        let addr = addr.to_ascii_lowercase();
        let rpc = self.rpc().await;
        let head = rpc.block_number().await.map_err(e)?;
        let token = self.token_meta(&rpc, &addr).await?;
        let pools = self.discover(&rpc, &addr, head).await?;
        if pools.is_empty() {
            return Err("no uniswap v4 pool for this token".into());
        }
        let ranked = Self::rank(&rpc, &pools, head).await;
        let mut sorted: Vec<&Pool> = pools.iter().collect();
        sorted.sort_by(|a, b| {
            ranked
                .get(&b.id)
                .unwrap_or(&0)
                .cmp(ranked.get(&a.id).unwrap_or(&0))
                .then(a.init_block.cmp(&b.init_block))
        });
        let hint = hint.map(|h| h.to_ascii_lowercase());
        let price_pool = hint
            .and_then(|h| pools.iter().find(|p| p.id == h).cloned())
            .unwrap_or_else(|| sorted[0].clone());
        let mut active: Vec<Pool> = sorted
            .iter()
            .filter(|p| ranked.get(&p.id).copied().unwrap_or(0) > 0)
            .take(6)
            .map(|p| (*p).clone())
            .collect();
        if !active.iter().any(|p| p.id == price_pool.id) {
            active.insert(0, price_pool.clone());
        }
        let token_is_c0 = price_pool.c0 == addr;
        let quote_addr = if token_is_c0 { price_pool.c1.clone() } else { price_pool.c0.clone() };
        let quote = self.token_meta(&rpc, &quote_addr).await?;

        // USD routing
        let (usd, ref_pool, ref_quote_is_c0) = if quote_addr == USDG {
            ("direct", None, false)
        } else {
            let qpools = self.discover(&rpc, &quote_addr, head).await.unwrap_or_default();
            let usd_pools: Vec<Pool> = qpools.into_iter().filter(|p| p.c0 == USDG || p.c1 == USDG).collect();
            if usd_pools.is_empty() {
                ("none", None, false)
            } else {
                let r = Self::rank(&rpc, &usd_pools, head).await;
                let best = usd_pools
                    .iter()
                    .max_by_key(|p| (r.get(&p.id).copied().unwrap_or(0), std::cmp::Reverse(p.init_block)))
                    .unwrap()
                    .clone();
                let is_c0 = best.c0 == quote_addr;
                ("via", Some(best), is_c0)
            }
        };

        let lo = active.iter().map(|p| p.init_block).min().unwrap_or(head);
        self.ensure_backfill(rpc.clone(), addr.clone(), active.iter().map(|p| p.id.clone()).collect(), lo, head);
        if let Some(rp) = &ref_pool {
            self.ensure_backfill(
                rpc.clone(),
                format!("ref:{}", rp.id),
                vec![rp.id.clone()],
                rp.init_block.max(lo.saturating_sub(200_000)),
                head,
            );
        }
        {
            let mut w = self.watched.lock().unwrap();
            for p in &active {
                w.insert(p.id.clone());
            }
            if let Some(rp) = &ref_pool {
                w.insert(rp.id.clone());
            }
        }
        self.tail_notify.notify_one();

        let view = TokenView {
            token,
            quote,
            token_is_c0,
            price_pool: price_pool.id.clone(),
            price_pool_fee: price_pool.fee,
            pools: active,
            all_pools: pools.len(),
            usd: usd.into(),
            ref_pool: ref_pool.as_ref().map(|p| p.id.clone()),
            init_block: lo,
            head,
        };
        self.tokens
            .lock()
            .unwrap()
            .insert(addr, TokenCtx { view: view.clone(), ref_pool, ref_quote_is_c0 });
        Ok(view)
    }

    /* ---------------------------------------------------------- backfill */

    fn ensure_backfill(&self, rpc: Arc<Rpc>, key: String, pools: Vec<String>, lo: u64, head: u64) {
        let flag = {
            let mut jobs = self.jobs.lock().unwrap();
            let f = jobs.entry(key.clone()).or_insert_with(|| Arc::new(AtomicU64::new(0))).clone();
            if f.swap(1, Ordering::SeqCst) == 1 {
                return; // already running
            }
            f
        };
        let store = self.store.clone();
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            backfill(rpc, store, app, key, pools, lo, head).await;
            flag.store(0, Ordering::SeqCst);
        });
    }

    pub fn job_running(&self, key: &str) -> bool {
        self.jobs.lock().unwrap().get(key).map(|f| f.load(Ordering::SeqCst) == 1).unwrap_or(false)
    }

    /* -------------------------------------------------------------- tail */

    async fn tail_loop(self: Arc<Self>) {
        let mut next_block: Option<u64> = None;
        loop {
            let pools: Vec<String> = self.watched.lock().unwrap().iter().cloned().collect();
            if pools.is_empty() {
                self.tail_notify.notified().await;
                continue;
            }
            let rpc = self.rpc().await;
            let topics = json!([SWAP_TOPIC, pools]);

            // websocket first
            if let Some(mut rx) = rpc.subscribe_logs(POOL_MANAGER.into(), topics.clone()).await {
                let _ = self.app.emit("rpc", json!({ "mode": "ws" }));
                // close the gap between the last polled block and the subscription
                if let Some(f) = next_block {
                    if let Ok(head) = rpc.block_number().await {
                        if head >= f {
                            if let Ok(logs) = rpc.get_logs(f, head, POOL_MANAGER, &topics).await {
                                self.handle_live(&rpc, logs, &pools, Some((f, head))).await;
                            }
                        }
                        next_block = Some(head + 1);
                    }
                }
                let mut mark = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tokio::select! {
                        l = rx.recv() => match l {
                            Some(l) => {
                                let mut batch = vec![l];
                                while let Ok(more) = rx.try_recv() { batch.push(more); }
                                self.handle_live(&rpc, batch, &pools, None).await;
                            }
                            None => break,
                        },
                        _ = mark.tick() => {
                            // the socket delivers every log, so blocks up to head count as covered
                            if let Ok(head) = rpc.block_number().await {
                                let f = next_block.unwrap_or(head);
                                if head >= f {
                                    let cover: Vec<(String,u64,u64)> = pools.iter().map(|p| (p.clone(), f, head)).collect();
                                    let _ = self.store.swaps_put(&[], &cover);
                                }
                                next_block = Some(head + 1);
                            }
                        }
                        _ = self.tail_notify.notified() => break,
                    }
                }
                continue;
            }

            // no websocket: poll
            let _ = self.app.emit("rpc", json!({ "mode": "poll" }));
            let mut from = match next_block {
                Some(f) => f,
                None => rpc.block_number().await.unwrap_or(0) + 1,
            };
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(400)) => {
                        let head = match rpc.block_number().await { Ok(h) => h, Err(_) => continue };
                        if head < from { continue; }
                        let to = head.min(from + 20_000);
                        match rpc.get_logs(from, to, POOL_MANAGER, &topics).await {
                            Ok(logs) => {
                                self.handle_live(&rpc, logs, &pools, Some((from, to))).await;
                                from = to + 1;
                                next_block = Some(from);
                            }
                            Err(RpcError::TooLarge) => {
                                let to = from + 500;
                                if let Ok(logs) = rpc.get_logs(from, to, POOL_MANAGER, &topics).await {
                                    self.handle_live(&rpc, logs, &pools, Some((from, to))).await;
                                    from = to + 1;
                                    next_block = Some(from);
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    _ = self.tail_notify.notified() => break,
                }
            }
        }
    }

    async fn handle_live(&self, rpc: &Rpc, logs: Vec<crate::rpc::Log>, pools: &[String], cover: Option<(u64, u64)>) {
        let swaps: Vec<Swap> = logs.iter().filter_map(decode_swap).collect();
        let mut blocks: Vec<u64> = swaps.iter().map(|s| s.block).collect();
        blocks.sort_unstable();
        blocks.dedup();
        let mut known = self.store.blocks_get(&blocks).unwrap_or_default();
        let missing: Vec<u64> = blocks.iter().filter(|b| !known.contains_key(b)).copied().collect();
        if !missing.is_empty() {
            if let Ok(pairs) = rpc.block_timestamps(&missing).await {
                let _ = self.store.blocks_put(&pairs);
                known.extend(pairs);
            }
        }
        let rows: Vec<(Swap, u64)> = swaps.into_iter().filter_map(|s| known.get(&s.block).map(|t| (s, *t))).collect();
        let cover: Vec<(String, u64, u64)> = match cover {
            Some((a, b)) => pools.iter().map(|p| (p.clone(), a, b)).collect(),
            None => vec![],
        };
        if let Ok(n) = self.store.swaps_put(&rows, &cover) {
            if n > 0 || !rows.is_empty() {
                let payload: Vec<Value> = rows
                    .iter()
                    .map(|(s, ts)| {
                        json!({
                            "pool": s.pool, "ts": ts, "block": s.block, "log_index": s.log_index, "tx": s.tx,
                            "sender": s.sender, "amount0": s.amount0, "amount1": s.amount1,
                            "sqrt_ratio": s.sqrt_ratio, "liquidity": s.liquidity,
                        })
                    })
                    .collect();
                let _ = self.app.emit("swap", payload);
            }
        }
    }

    /* ----------------------------------------------------------- reading */

    fn ctx(&self, addr: &str) -> R<TokenCtx> {
        self.tokens.lock().unwrap().get(&addr.to_ascii_lowercase()).cloned().ok_or_else(|| "token not open".into())
    }

    /// (block, USD per quote unit) ascending, from the reference pool
    fn ref_series(&self, ctx: &TokenCtx) -> Vec<(u64, f64)> {
        let Some(rp) = &ctx.ref_pool else { return vec![] };
        let dq = ctx.view.quote.decimals;
        self.store
            .series(&rp.id)
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                let p = if ctx.ref_quote_is_c0 {
                    price_c1_per_c0(r.sqrt_ratio, dq, 6)
                } else {
                    1.0 / price_c1_per_c0(r.sqrt_ratio, 6, dq)
                };
                (r.block, p)
            })
            .collect()
    }

    fn ref_at(refs: &[(u64, f64)], block: u64) -> f64 {
        if refs.is_empty() {
            return 1.0;
        }
        let i = refs.partition_point(|(b, _)| *b <= block);
        if i == 0 { refs[0].1 } else { refs[i - 1].1 }
    }

    /// the price pool's swaps priced in USD: (row, price_usd, signed token amount)
    fn priced(&self, ctx: &TokenCtx) -> R<Vec<(Row, f64, f64)>> {
        let v = &ctx.view;
        let rows = self.store.series(&v.price_pool).map_err(e)?;
        let refs = if v.usd == "via" { self.ref_series(ctx) } else { vec![] };
        let (dt, dq) = (v.token.decimals, v.quote.decimals);
        Ok(rows
            .into_iter()
            .map(|r| {
                let pq = if v.token_is_c0 {
                    price_c1_per_c0(r.sqrt_ratio, dt, dq)
                } else {
                    1.0 / price_c1_per_c0(r.sqrt_ratio, dq, dt)
                };
                let mult = if v.usd == "via" { Self::ref_at(&refs, r.block) } else { 1.0 };
                let amt = scale(if v.token_is_c0 { r.amount0 } else { r.amount1 }, dt);
                (r, pq * mult, amt)
            })
            .collect())
    }

    /// flat candles: [t, o, h, l, c, volume_usd, trades] * n
    pub fn candles(&self, addr: &str, tf: u64) -> R<Vec<f64>> {
        let ctx = self.ctx(addr)?;
        let tf = tf.max(1);
        let priced = self.priced(&ctx)?;
        let mut out: Vec<f64> = Vec::with_capacity(priced.len() * 7 / 4 + 7);
        let mut cur: Option<[f64; 7]> = None;
        for (r, p, amt) in priced {
            if !p.is_finite() || p <= 0.0 {
                continue;
            }
            let t = (r.ts - r.ts % tf) as f64;
            let vol = amt.abs() * p;
            match cur.as_mut() {
                Some(c) if c[0] == t => {
                    if p > c[2] { c[2] = p; }
                    if p < c[3] { c[3] = p; }
                    c[4] = p;
                    c[5] += vol;
                    c[6] += 1.0;
                }
                _ => {
                    if let Some(c) = cur.take() { out.extend_from_slice(&c); }
                    // open of a new candle continues from the previous close, so gaps do not draw as jumps
                    let o = if out.is_empty() { p } else { out[out.len() - 3] };
                    cur = Some([t, o, o.max(p), o.min(p), p, vol, 1.0]);
                }
            }
        }
        if let Some(c) = cur { out.extend_from_slice(&c); }
        Ok(out)
    }

    /// newest trades across the active set, valued at the price pool's USD price
    pub fn trades(&self, addr: &str, before_ts: u64, limit: usize) -> R<Vec<Value>> {
        let ctx = self.ctx(addr)?;
        let v = &ctx.view;
        let ids: Vec<String> = v.pools.iter().map(|p| p.id.clone()).collect();
        let rows = self.store.recent(&ids, before_ts, limit.min(500)).map_err(e)?;
        let priced = self.priced(&ctx)?;
        let px: Vec<(u64, u32, f64)> = priced.iter().map(|(r, p, _)| (r.block, r.log_index, *p)).collect();
        let dt = v.token.decimals;
        let c0_of: HashMap<&str, &str> = v.pools.iter().map(|p| (p.id.as_str(), p.c0.as_str())).collect();
        Ok(rows
            .into_iter()
            .map(|r| {
                let is_c0 = c0_of.get(r.pool.as_str()).map(|c| *c == v.token.address).unwrap_or(v.token_is_c0);
                let amt = scale(if is_c0 { r.amount0 } else { r.amount1 }, dt);
                // price at or before this swap
                let i = px.partition_point(|(b, li, _)| (*b, *li) <= (r.block, r.log_index));
                let p = if px.is_empty() { 0.0 } else if i == 0 { px[0].2 } else { px[i - 1].2 };
                json!({
                    "ts": r.ts, "block": r.block, "log_index": r.log_index, "tx": r.tx, "pool": r.pool,
                    "sender": r.sender, "side": if amt >= 0.0 { "buy" } else { "sell" },
                    "amount": amt.abs(), "price": p, "usd": amt.abs() * p,
                })
            })
            .collect())
    }

    pub fn status(&self, addr: &str) -> R<Value> {
        let ctx = self.ctx(addr)?;
        let v = &ctx.view;
        let ids: Vec<String> = v.pools.iter().map(|p| p.id.clone()).collect();
        let (n, first, last) = self.store.stats(&ids).map_err(e)?;
        let ranges = self.store.ranges_get(&v.price_pool).map_err(e)?;
        Ok(json!({
            "swaps": n, "first_ts": first, "last_ts": last,
            "ranges": ranges,
            "scanning": self.job_running(&v.token.address),
            "ref_scanning": v.ref_pool.as_ref().map(|r| self.job_running(&format!("ref:{r}"))).unwrap_or(false),
        }))
    }
}

/* ------------------------------------------------------------ helpers */

/// `Initialize` logs mentioning the token as either currency, splitting the
/// block range whenever the node says the query is too large.
async fn scan_inits(rpc: &Rpc, addr: &str, from: u64, to: u64) -> Result<Vec<PoolInit>, RpcError> {
    let pad = pad_addr(addr);
    let variants = [json!([INIT_TOPIC, Value::Null, pad]), json!([INIT_TOPIC, Value::Null, Value::Null, pad])];
    let mut out = Vec::new();
    for topics in &variants {
        let mut stack = vec![(from, to)];
        while let Some((a, b)) = stack.pop() {
            match rpc.get_logs(a, b, POOL_MANAGER, topics).await {
                Ok(logs) => out.extend(logs.iter().filter_map(decode_init)),
                Err(RpcError::TooLarge) if b > a + 1000 => {
                    let mid = a + (b - a) / 2;
                    stack.push((mid + 1, b));
                    stack.push((a, mid));
                }
                Err(err) => return Err(err),
            }
        }
    }
    Ok(out)
}

fn complement(lo: u64, hi: u64, covered: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    let mut cur = lo;
    for (a, b) in covered {
        if *b < cur { continue; }
        if *a > hi { break; }
        if *a > cur { gaps.push((cur, a - 1)); }
        cur = cur.max(b + 1);
        if cur > hi { break; }
    }
    if cur <= hi { gaps.push((cur, hi)); }
    gaps
}

fn merge(mut r: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    r.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(r.len());
    for (a, b) in r {
        if let Some(l) = out.last_mut() {
            if a <= l.1 + 1 { l.1 = l.1.max(b); continue; }
        }
        out.push((a, b));
    }
    out
}

async fn backfill(rpc: Arc<Rpc>, store: Arc<Store>, app: AppHandle, key: String, pools: Vec<String>, lo: u64, head: u64) {
    let mut gaps = Vec::new();
    for p in &pools {
        let covered = store.ranges_get(p).unwrap_or_default();
        gaps.extend(complement(lo, head, &covered));
    }
    let gaps = merge(gaps);
    let total: u64 = gaps.iter().map(|(a, b)| b - a + 1).sum();

    // newest chunks first, so the recent chart paints before history fills in
    let mut queue: VecDeque<(u64, u64, u32)> = VecDeque::new();
    for (a, b) in gaps.iter().rev() {
        let mut hi = *b;
        loop {
            let l = hi.saturating_sub(rpc.chunk0 - 1).max(*a);
            queue.push_back((l, hi, 0));
            if l == *a { break; }
            hi = l - 1;
        }
    }
    let queue = Arc::new(Mutex::new(queue));
    let done = Arc::new(AtomicU64::new(0));
    let added = Arc::new(AtomicU64::new(0));
    let topics = Arc::new(json!([SWAP_TOPIC, pools]));
    let pools = Arc::new(pools);

    let emit = |done_b: u64, added_n: u64, finished: bool| {
        let _ = app.emit("idx", json!({ "key": key, "done": done_b, "total": total, "added": added_n, "finished": finished }));
    };
    emit(0, 0, total == 0);
    if total == 0 {
        return;
    }

    let workers = (0..rpc.concurrency).map(|_| {
        let (rpc, store, app, key, queue, done, added, topics, pools) = (
            rpc.clone(), store.clone(), app.clone(), key.clone(), queue.clone(), done.clone(), added.clone(), topics.clone(), pools.clone(),
        );
        async move {
            loop {
                let job = queue.lock().unwrap().pop_front();
                let Some((a, b, tries)) = job else { break };
                match rpc.get_logs(a, b, POOL_MANAGER, &topics).await {
                    Ok(logs) => {
                        let swaps: Vec<Swap> = logs.iter().filter_map(decode_swap).collect();
                        let mut blocks: Vec<u64> = swaps.iter().map(|s| s.block).collect();
                        blocks.sort_unstable();
                        blocks.dedup();
                        let mut known = store.blocks_get(&blocks).unwrap_or_default();
                        let missing: Vec<u64> = blocks.iter().filter(|n| !known.contains_key(n)).copied().collect();
                        if !missing.is_empty() {
                            match rpc.block_timestamps(&missing).await {
                                Ok(pairs) => {
                                    let _ = store.blocks_put(&pairs);
                                    known.extend(pairs);
                                }
                                Err(_) => {
                                    if tries < 5 {
                                        queue.lock().unwrap().push_back((a, b, tries + 1));
                                        tokio::time::sleep(Duration::from_millis(600)).await;
                                    }
                                    continue;
                                }
                            }
                        }
                        let rows: Vec<(Swap, u64)> =
                            swaps.into_iter().filter_map(|s| known.get(&s.block).map(|t| (s, *t))).collect();
                        let cover: Vec<(String, u64, u64)> = pools.iter().map(|p| (p.clone(), a, b)).collect();
                        let n = store.swaps_put(&rows, &cover).unwrap_or(0) as u64;
                        let d = done.fetch_add(b - a + 1, Ordering::SeqCst) + (b - a + 1);
                        let ad = added.fetch_add(n, Ordering::SeqCst) + n;
                        let _ = app.emit("idx", json!({ "key": key, "done": d, "total": total, "added": ad, "finished": false }));
                    }
                    Err(RpcError::TooLarge) => {
                        if b - a < 64 {
                            done.fetch_add(b - a + 1, Ordering::SeqCst);
                        } else {
                            let mid = a + (b - a) / 2;
                            let mut q = queue.lock().unwrap();
                            q.push_front((a, mid, 0));
                            q.push_front((mid + 1, b, 0));
                        }
                    }
                    Err(_) => {
                        if tries < 5 {
                            queue.lock().unwrap().push_back((a, b, tries + 1));
                            tokio::time::sleep(Duration::from_millis(800)).await;
                        } else {
                            done.fetch_add(b - a + 1, Ordering::SeqCst);
                        }
                    }
                }
            }
        }
    });
    futures_util::future::join_all(workers).await;
    emit(done.load(Ordering::SeqCst), added.load(Ordering::SeqCst), true);
}

/// ABI-decode a `string` return (or a bytes32 symbol)
fn abi_string(hex: &str) -> String {
    let h = hex.trim_start_matches("0x");
    let utf8 = |s: &str| -> String {
        let bytes: Vec<u8> = (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect();
        String::from_utf8_lossy(&bytes).trim_matches('\0').trim().to_string()
    };
    if h.len() >= 128 {
        if let Ok(off) = usize::from_str_radix(&h[..64], 16) {
            let off = off * 2;
            if let Some(w) = h.get(off..off + 64) {
                if let Ok(len) = usize::from_str_radix(w, 16) {
                    if let Some(b) = h.get(off + 64..off + 64 + len * 2) {
                        return utf8(b);
                    }
                }
            }
        }
    }
    if h.len() == 64 {
        let t = h.trim_end_matches('0');
        let t = if t.len() % 2 == 1 { &h[..t.len() + 1] } else { t };
        return utf8(t);
    }
    String::new()
}
