//! Uniswap v4 on Robinhood Chain: one PoolManager, two events that matter.
//!
//! Every trade on the chain is a `Swap` log on the singleton, keyed by the
//! pool's bytes32 id. The log carries the post-trade `sqrtPriceX96`, so the
//! price series is read straight off the wire; nothing is inferred from
//! transfers. Amounts are from the trader's side: positive = received,
//! negative = paid (verified against tape fills).

use crate::rpc::Log;

pub const POOL_MANAGER: &str = "0x8366a39cc670b4001a1121b8f6a443a643e40951";
/// Swap(bytes32 indexed id, address indexed sender, int128 amount0, int128 amount1,
///      uint160 sqrtPriceX96, uint128 liquidity, int24 tick, uint24 fee)
pub const SWAP_TOPIC: &str = "0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f";
/// Initialize(bytes32 indexed id, address indexed currency0, address indexed currency1,
///            uint24 fee, int24 tickSpacing, address hooks, uint160 sqrtPriceX96, int24 tick)
pub const INIT_TOPIC: &str = "0xdd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438";
/// Global Dollar, 6 decimals: the chain's cash leg.
pub const USDG: &str = "0x5fc5360d0400a0fd4f2af552add042d716f1d168";
pub const NATIVE: &str = "0x0000000000000000000000000000000000000000";
pub const Q96: f64 = 79228162514264337593543950336.0;

#[derive(Debug, Clone)]
pub struct Swap {
    pub pool: String,
    pub block: u64,
    pub log_index: u32,
    pub tx: String,
    pub sender: String,
    /// raw token units (not scaled by decimals), trader's perspective
    pub amount0: f64,
    pub amount1: f64,
    /// sqrtPriceX96 / 2^96 -- square it for currency1-per-currency0 in raw units
    pub sqrt_ratio: f64,
    pub liquidity: f64,
    pub tick: i32,
}

#[derive(Debug, Clone)]
pub struct PoolInit {
    pub id: String,
    pub c0: String,
    pub c1: String,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: String,
    pub block: u64,
}

fn word(data: &str, i: usize) -> Option<&str> {
    data.get(2 + i * 64..2 + (i + 1) * 64)
}
/// unsigned 256-bit word to f64 (exact to 53 bits, plenty for a chart)
fn u_f64(w: &str) -> f64 {
    let hi = u128::from_str_radix(&w[..32], 16).unwrap_or(0);
    let lo = u128::from_str_radix(&w[32..], 16).unwrap_or(0);
    hi as f64 * 340282366920938463463374607431768211456.0 + lo as f64
}
/// int128 stored sign-extended in a 256-bit word
fn i128_f64(w: &str) -> f64 {
    let lo = u128::from_str_radix(&w[32..], 16).unwrap_or(0);
    (lo as i128) as f64
}
fn i24(w: &str) -> i32 {
    let v = u32::from_str_radix(&w[58..], 16).unwrap_or(0);
    if v & 0x80_0000 != 0 { (v | 0xFF00_0000) as i32 } else { v as i32 }
}
fn u24(w: &str) -> u32 {
    u32::from_str_radix(&w[58..], 16).unwrap_or(0)
}

pub fn topic_addr(t: &str) -> String {
    format!("0x{}", &t[t.len().saturating_sub(40)..])
}
/// left-pad an address to a 32-byte topic
pub fn pad_addr(a: &str) -> String {
    format!("0x{:0>64}", a.trim_start_matches("0x").to_ascii_lowercase())
}

pub fn decode_swap(l: &Log) -> Option<Swap> {
    if l.topics.len() < 3 || l.topics[0] != SWAP_TOPIC || l.data.len() < 2 + 6 * 64 {
        return None;
    }
    Some(Swap {
        pool: l.topics[1].clone(),
        block: l.block,
        log_index: l.log_index,
        tx: l.tx.clone(),
        sender: topic_addr(&l.topics[2]),
        amount0: i128_f64(word(&l.data, 0)?),
        amount1: i128_f64(word(&l.data, 1)?),
        sqrt_ratio: u_f64(word(&l.data, 2)?) / Q96,
        liquidity: u_f64(word(&l.data, 3)?),
        tick: i24(word(&l.data, 4)?),
    })
}

pub fn decode_init(l: &Log) -> Option<PoolInit> {
    if l.topics.len() < 4 || l.topics[0] != INIT_TOPIC || l.data.len() < 2 + 3 * 64 {
        return None;
    }
    Some(PoolInit {
        id: l.topics[1].clone(),
        c0: topic_addr(&l.topics[2]),
        c1: topic_addr(&l.topics[3]),
        fee: u24(word(&l.data, 0)?),
        tick_spacing: i24(word(&l.data, 1)?),
        hooks: topic_addr(word(&l.data, 2)?),
        block: l.block,
    })
}

/// currency1 per currency0, in human units
pub fn price_c1_per_c0(sqrt_ratio: f64, dec0: i32, dec1: i32) -> f64 {
    sqrt_ratio * sqrt_ratio * 10f64.powi(dec0 - dec1)
}
pub fn scale(raw: f64, dec: i32) -> f64 {
    raw / 10f64.powi(dec)
}
