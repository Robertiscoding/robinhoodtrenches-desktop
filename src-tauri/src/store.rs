//! On-disk index: SQLite in WAL mode, one connection behind a mutex.
//!
//! Swaps are keyed (pool, block, log_index) so re-scans are idempotent.
//! Coverage is tracked per pool as merged [lo, hi] block ranges, so a
//! restart resumes exactly and a gap is a fact, not a guess. Block
//! timestamps are cached chain-wide: they are the same for every token.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::v4::{PoolInit, Swap};

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Pool {
    pub id: String,
    pub c0: String,
    pub c1: String,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: String,
    pub init_block: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenMeta {
    pub address: String,
    pub symbol: String,
    pub name: String,
    pub decimals: i32,
}

/// a stored swap row
#[derive(Debug, Clone)]
pub struct Row {
    pub pool: String,
    pub ts: u64,
    pub block: u64,
    pub log_index: u32,
    pub tx: String,
    pub sender: String,
    pub amount0: f64,
    pub amount1: f64,
    pub sqrt_ratio: f64,
}

pub type SqlResult<T> = rusqlite::Result<T>;

impl Store {
    pub fn open(path: &std::path::Path) -> SqlResult<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-65536;
             PRAGMA mmap_size=268435456;
             CREATE TABLE IF NOT EXISTS tokens(address TEXT PRIMARY KEY, symbol TEXT, name TEXT, decimals INTEGER);
             CREATE TABLE IF NOT EXISTS pools(id TEXT PRIMARY KEY, c0 TEXT, c1 TEXT, fee INTEGER, tick_spacing INTEGER, hooks TEXT, init_block INTEGER);
             CREATE INDEX IF NOT EXISTS pools_c0 ON pools(c0);
             CREATE INDEX IF NOT EXISTS pools_c1 ON pools(c1);
             CREATE TABLE IF NOT EXISTS discovered(token TEXT PRIMARY KEY, scanned_to INTEGER);
             CREATE TABLE IF NOT EXISTS swaps(
               pool TEXT NOT NULL, block INTEGER NOT NULL, log_index INTEGER NOT NULL, ts INTEGER NOT NULL,
               tx TEXT, sender TEXT, amount0 REAL, amount1 REAL, sqrt_ratio REAL, liquidity REAL, tick INTEGER,
               PRIMARY KEY(pool, block, log_index)) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS swaps_pool_ts ON swaps(pool, ts, block, log_index);
             CREATE TABLE IF NOT EXISTS blocks(number INTEGER PRIMARY KEY, ts INTEGER NOT NULL) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS ranges(pool TEXT NOT NULL, lo INTEGER NOT NULL, hi INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS ranges_pool ON ranges(pool);",
        )?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /* ------------------------------------------------------------ tokens */

    pub fn token_get(&self, address: &str) -> SqlResult<Option<TokenMeta>> {
        self.c()
            .query_row(
                "SELECT address, symbol, name, decimals FROM tokens WHERE address = ?1",
                params![address],
                |r| Ok(TokenMeta { address: r.get(0)?, symbol: r.get(1)?, name: r.get(2)?, decimals: r.get(3)? }),
            )
            .optional()
    }

    pub fn token_put(&self, t: &TokenMeta) -> SqlResult<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO tokens(address, symbol, name, decimals) VALUES (?1, ?2, ?3, ?4)",
            params![t.address, t.symbol, t.name, t.decimals],
        )?;
        Ok(())
    }

    /* ------------------------------------------------------------- pools */

    pub fn pools_put(&self, inits: &[PoolInit]) -> SqlResult<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT OR IGNORE INTO pools(id, c0, c1, fee, tick_spacing, hooks, init_block) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for p in inits {
                st.execute(params![p.id, p.c0, p.c1, p.fee, p.tick_spacing, p.hooks, p.block as i64])?;
            }
        }
        tx.commit()
    }

    fn row_pool(r: &rusqlite::Row<'_>) -> SqlResult<Pool> {
        Ok(Pool {
            id: r.get(0)?,
            c0: r.get(1)?,
            c1: r.get(2)?,
            fee: r.get::<_, i64>(3)? as u32,
            tick_spacing: r.get::<_, i64>(4)? as i32,
            hooks: r.get(5)?,
            init_block: r.get::<_, i64>(6)? as u64,
        })
    }

    pub fn pools_for(&self, token: &str) -> SqlResult<Vec<Pool>> {
        let c = self.c();
        let mut st = c.prepare_cached(
            "SELECT id, c0, c1, fee, tick_spacing, hooks, init_block FROM pools WHERE c0 = ?1 OR c1 = ?1 ORDER BY init_block",
        )?;
        let rows = st.query_map(params![token], Self::row_pool)?;
        rows.collect()
    }


    pub fn discovered_get(&self, token: &str) -> SqlResult<Option<u64>> {
        self.c()
            .query_row("SELECT scanned_to FROM discovered WHERE token = ?1", params![token], |r| {
                r.get::<_, i64>(0).map(|v| v as u64)
            })
            .optional()
    }

    pub fn discovered_put(&self, token: &str, scanned_to: u64) -> SqlResult<()> {
        self.c().execute(
            "INSERT OR REPLACE INTO discovered(token, scanned_to) VALUES (?1, ?2)",
            params![token, scanned_to as i64],
        )?;
        Ok(())
    }

    /* ------------------------------------------------------------ blocks */

    pub fn blocks_get(&self, nums: &[u64]) -> SqlResult<HashMap<u64, u64>> {
        let c = self.c();
        let mut st = c.prepare_cached("SELECT ts FROM blocks WHERE number = ?1")?;
        let mut out = HashMap::with_capacity(nums.len());
        for n in nums {
            if let Some(ts) = st.query_row(params![*n as i64], |r| r.get::<_, i64>(0)).optional()? {
                out.insert(*n, ts as u64);
            }
        }
        Ok(out)
    }

    pub fn blocks_put(&self, pairs: &[(u64, u64)]) -> SqlResult<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare_cached("INSERT OR IGNORE INTO blocks(number, ts) VALUES (?1, ?2)")?;
            for (n, ts) in pairs {
                st.execute(params![*n as i64, *ts as i64])?;
            }
        }
        tx.commit()
    }

    /* ------------------------------------------------------------- swaps */

    /// Insert swaps (with their timestamps) and record coverage in one transaction.
    pub fn swaps_put(&self, swaps: &[(Swap, u64)], cover: &[(String, u64, u64)]) -> SqlResult<usize> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let mut n = 0;
        {
            let mut st = tx.prepare_cached(
                "INSERT OR IGNORE INTO swaps(pool, block, log_index, ts, tx, sender, amount0, amount1, sqrt_ratio, liquidity, tick)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            )?;
            for (s, ts) in swaps {
                n += st.execute(params![
                    s.pool, s.block as i64, s.log_index as i64, *ts as i64, s.tx, s.sender,
                    s.amount0, s.amount1, s.sqrt_ratio, s.liquidity, s.tick
                ])?;
            }
        }
        for (pool, lo, hi) in cover {
            Self::range_add_tx(&tx, pool, *lo, *hi)?;
        }
        tx.commit()?;
        Ok(n)
    }

    fn range_add_tx(tx: &rusqlite::Transaction<'_>, pool: &str, lo: u64, hi: u64) -> SqlResult<()> {
        let mut ranges: Vec<(u64, u64)> = {
            let mut st = tx.prepare_cached("SELECT lo, hi FROM ranges WHERE pool = ?1")?;
            let rows = st.query_map(params![pool], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)))?;
            rows.collect::<SqlResult<_>>()?
        };
        ranges.push((lo, hi));
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for (a, b) in ranges {
            if let Some(last) = merged.last_mut() {
                if a <= last.1.saturating_add(1) {
                    last.1 = last.1.max(b);
                    continue;
                }
            }
            merged.push((a, b));
        }
        tx.execute("DELETE FROM ranges WHERE pool = ?1", params![pool])?;
        let mut st = tx.prepare_cached("INSERT INTO ranges(pool, lo, hi) VALUES (?1, ?2, ?3)")?;
        for (a, b) in merged {
            st.execute(params![pool, a as i64, b as i64])?;
        }
        Ok(())
    }

    pub fn ranges_get(&self, pool: &str) -> SqlResult<Vec<(u64, u64)>> {
        let c = self.c();
        let mut st = c.prepare_cached("SELECT lo, hi FROM ranges WHERE pool = ?1 ORDER BY lo")?;
        let rows = st.query_map(params![pool], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)))?;
        rows.collect()
    }

    fn row_swap(r: &rusqlite::Row<'_>) -> SqlResult<Row> {
        Ok(Row {
            pool: r.get(0)?,
            ts: r.get::<_, i64>(1)? as u64,
            block: r.get::<_, i64>(2)? as u64,
            log_index: r.get::<_, i64>(3)? as u32,
            tx: r.get(4)?,
            sender: r.get(5)?,
            amount0: r.get(6)?,
            amount1: r.get(7)?,
            sqrt_ratio: r.get(8)?,
        })
    }

    /// every swap of one pool, oldest first
    pub fn series(&self, pool: &str) -> SqlResult<Vec<Row>> {
        let c = self.c();
        let mut st = c.prepare_cached(
            "SELECT pool, ts, block, log_index, tx, sender, amount0, amount1, sqrt_ratio
             FROM swaps WHERE pool = ?1 ORDER BY ts, block, log_index",
        )?;
        let rows = st.query_map(params![pool], Self::row_swap)?;
        rows.collect()
    }

    /// newest swaps across a set of pools, before a timestamp
    pub fn recent(&self, pools: &[String], before_ts: u64, limit: usize) -> SqlResult<Vec<Row>> {
        if pools.is_empty() {
            return Ok(vec![]);
        }
        let marks = std::iter::repeat("?").take(pools.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT pool, ts, block, log_index, tx, sender, amount0, amount1, sqrt_ratio
             FROM swaps WHERE pool IN ({marks}) AND ts < ? ORDER BY ts DESC, block DESC, log_index DESC LIMIT ?"
        );
        let c = self.c();
        let mut st = c.prepare(&sql)?;
        let mut args: Vec<rusqlite::types::Value> = pools.iter().map(|p| p.clone().into()).collect();
        args.push((before_ts as i64).into());
        args.push((limit as i64).into());
        let rows = st.query_map(rusqlite::params_from_iter(args), Self::row_swap)?;
        rows.collect()
    }

    /// (count, first ts, last ts) over a set of pools
    pub fn stats(&self, pools: &[String]) -> SqlResult<(u64, Option<u64>, Option<u64>)> {
        if pools.is_empty() {
            return Ok((0, None, None));
        }
        let marks = std::iter::repeat("?").take(pools.len()).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT COUNT(*), MIN(ts), MAX(ts) FROM swaps WHERE pool IN ({marks})");
        let c = self.c();
        let mut st = c.prepare(&sql)?;
        let args: Vec<rusqlite::types::Value> = pools.iter().map(|p| p.clone().into()).collect();
        st.query_row(rusqlite::params_from_iter(args), |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
            ))
        })
    }
}
