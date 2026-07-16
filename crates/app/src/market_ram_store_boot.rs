//! RAM residency stores — boot install + chain-day rehydrate + stats task
//! (PR-2 of the data-completeness build; RAMSTORE-01 runbook:
//! `.claude/rules/project/ram-store-error-codes.md`).
//!
//! Operator directive 2026-07-16 (verbatim): *"how can i believe you that
//! you have all these already available in our in-memory app RAM —
//! especially for the current day and even in the future last one month
//! data should be entirely in memory app RAM, especially for trading
//! decisions of entry and exit"* — refined by *"for only spots we will
//! have minimum one month data because anyhow based on underlying spots
//! alone only trading decision will be entered or exited — but option only
//! for the current day"* and *"everything should be always available in
//! our own questdb right — our entire one month should be stored and
//! fetched from questdb even before premarket"*.
//!
//! Three responsibilities, all cold-path:
//! 1. **Install** ([`install_market_ram_stores`]): the process-global
//!    month-deep `SpotBarStore` (trading crate) + current-day
//!    `ChainDayStore` (core pipeline), gated on `[market_ram_store]`.
//!    Installed BEFORE the fold task spawns so PR-1's boot catch-up
//!    populates the spot rings — pre-market spot rehydration IS the
//!    existing catch-up (zero new spot QuestDB reads).
//! 2. **Chain rehydrate** ([`spawn_chain_day_rehydrate`]): a ONE-SHOT
//!    bounded read of TODAY's `option_chain_1m` rows per (feed,
//!    underlying, 30-minute session window) — hardened `/exec` shapes
//!    (micros WHERE window, nanos projection, explicit LIMIT tripwire,
//!    streamed 8 MiB cap, redirect-none client) — rebuilt into
//!    `ChainMoneynessSnapshot`s and recorded via `record_rehydrated`
//!    (NEVER overwriting live-published minutes). A mid-session restart
//!    gets the morning's chain history back.
//! 3. **Stats/heartbeat** ([`spawn_ram_store_stats_task`]): a supervised
//!    60 s loop publishing the depth gauges the operator's "is the month
//!    actually in RAM?" question reads — honest fill level, never a
//!    fabricated month (audit Rule 11).
//!
//! Every degrade is a coded RAMSTORE-01 `error!` (log-sink-only delivery
//! boundary per the runbook) — QuestDB remains the durable truth; a RAM
//! degrade re-fills at the next boot.

use std::time::Duration;

use metrics::{counter, gauge};
use tickvault_common::config::{MarketRamStoreConfig, QuestDbConfig};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::moneyness::{
    Moneyness, OptionLeg, atm_strike_paise, price_to_paise_guarded, strike_step_paise,
};
use tickvault_core::pipeline::chain_day_store::{
    ChainRecordOutcome, chain_day_store, install_chain_day_store,
};
use tickvault_core::pipeline::chain_snapshot::{
    ChainMoneynessSnapshot, ChainUnderlying, SnapshotRow,
};
use tickvault_trading::in_mem::spot_bar_store::{
    estimated_capacity_bytes, install_spot_bar_store, spot_bar_store,
};
use tracing::{error, info, warn};

use crate::rest_candle_fold::{
    FOLD_MAX_RESPONSE_BYTES, accumulate_capped, day_start_nanos, today_ist,
};

// ---------------------------------------------------------------------------
// Constants (all named — cold-path envelope bounds)
// ---------------------------------------------------------------------------

/// Stats/heartbeat cadence (the house 60 s stats-task cadence).
pub const RAM_STORE_STATS_INTERVAL_SECS: u64 = 60;

/// Backoff before respawning a dead stats task (house respawn pattern).
pub const RAM_STORE_STATS_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Chain rehydrate window width — 30 minutes per bounded `/exec` read so
/// one response stays well inside the 8 MiB streamed cap even at the
/// row-cap worst case.
pub const RAM_CHAIN_REHYDRATE_WINDOW_MINUTES: usize = 30;

/// Session windows per day: 13 × 30 min covers [09:15, 15:45) IST — the
/// 375-minute session plus the legs' boundary-fire margin.
pub const RAM_CHAIN_REHYDRATE_WINDOW_COUNT: usize = 13;

/// NSE session open, IST seconds-of-day (09:15).
const SESSION_OPEN_SECS_OF_DAY: i64 = 9 * 3600 + 15 * 60;

/// IST offset in seconds (UTC + 5:30).
const IST_UTC_OFFSET_SECS: i64 = 19_800;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SEC;

/// Per-request HTTP timeout for the rehydrate `/exec` reads.
const RAM_CHAIN_REHYDRATE_HTTP_TIMEOUT_SECS: u64 = 15;

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Installs BOTH process-global stores (first-wins). Called from the boot
/// path BEFORE the fold task spawns (`ram_store_wiring_guard` pins the
/// order) so the catch-up's seals land in the spot rings.
// TEST-EXEMPT: process-global OnceLock installs — pinned by the store crates' first-wins tests + ram_store_wiring_guard.
pub fn install_market_ram_stores(cfg: &MarketRamStoreConfig, catchup_days: u32) {
    let spot_ok = install_spot_bar_store(cfg.spot_days);
    let chain_ok = install_chain_day_store(cfg.chain_row_cap as usize);
    if !spot_ok || !chain_ok {
        // Defensive first-wins refusal — a duplicate install means a second
        // boot-path call in one process (loud, never silent).
        error!(
            code = ErrorCode::RamStore01Degraded.code_str(),
            stage = "install",
            spot_ok,
            chain_ok,
            "RAMSTORE-01: RAM store install refused — already installed \
             (first-wins; the first installation keeps serving)"
        );
        return;
    }
    if cfg.spot_days < catchup_days {
        warn!(
            spot_days = cfg.spot_days,
            catchup_days,
            "market_ram_store: spot_days is SHALLOWER than the fold catch-up \
             window — the rings evict the oldest catch-up days (harmless, but \
             RAM depth < the folded history; raise [market_ram_store] spot_days \
             to keep the whole window resident)"
        );
    }
    info!(
        spot_days = cfg.spot_days,
        chain_row_cap = cfg.chain_row_cap,
        spot_capacity_bytes = estimated_capacity_bytes(cfg.spot_days, 8),
        "market_ram_store: RAM residency stores installed — spots month-deep \
         (filled by the fold catch-up + live seals; depth bounded by CAPTURED \
         history, shown honestly by tv_ram_store_spot_days_depth), options \
         current-day (chain publishes + boot rehydrate)"
    );
}

// ---------------------------------------------------------------------------
// Chain-day rehydrate (one-shot, bounded)
// ---------------------------------------------------------------------------

/// One parsed `option_chain_1m` rehydrate row.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainRehydrateRow {
    /// Minute-open, IST nanos (projected `(ts / 1) * 1000`).
    pub ts_nanos: i64,
    pub strike: f64,
    /// `"CE"` / `"PE"` (SYMBOL column).
    pub leg: String,
    pub last_price: f64,
    /// `"ITM"`/`"ATM"`/`"OTM"`/`"UNKNOWN"`; pre-moneyness rows read NULL →
    /// `"UNKNOWN"` (tolerant, never a parse failure).
    pub moneyness: String,
    pub underlying_spot: f64,
    /// Expiry-day IST midnight nanos.
    pub expiry_nanos: i64,
    /// Retrieval instant, IST nanos.
    pub fetched_nanos: i64,
}

/// SQL for one (feed, underlying, window) slice of today's chain rows —
/// the hardened `/exec` shape (micros WHERE window, nanos projections,
/// explicit LIMIT tripwire).
#[must_use]
pub fn chain_rehydrate_sql(
    feed: &str,
    underlying_symbol: &str,
    window_start_nanos: i64,
    window_end_nanos: i64,
    limit: usize,
) -> String {
    let start_micros = window_start_nanos / 1_000;
    let end_micros = window_end_nanos / 1_000;
    format!(
        "SELECT (ts / 1) * 1000 AS ts_nanos, strike, leg, last_price, moneyness, \
         underlying_spot, (expiry / 1) * 1000 AS expiry_nanos, \
         (fetched_at / 1) * 1000 AS fetched_nanos \
         FROM option_chain_1m \
         WHERE feed = '{feed}' AND underlying_symbol = '{underlying_symbol}' \
         AND ts >= {start_micros} AND ts < {end_micros} \
         ORDER BY ts ASC LIMIT {limit}"
    )
}

/// Parses a rehydrate `/exec` dataset. Returns `(rows, truncated)` —
/// `truncated` means the explicit LIMIT was hit (a partial window is NEVER
/// trusted; the caller skips it loudly).
#[must_use]
pub fn parse_chain_rehydrate_rows(
    body: &str,
    limit: usize,
) -> Option<(Vec<ChainRehydrateRow>, bool)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let dataset = value.get("dataset")?.as_array()?;
    let truncated = dataset.len() >= limit;
    let mut rows = Vec::with_capacity(dataset.len());
    for row in dataset {
        let cells = row.as_array()?;
        if cells.len() < 8 {
            return None;
        }
        rows.push(ChainRehydrateRow {
            ts_nanos: cells[0].as_i64()?,
            strike: cells[1].as_f64()?,
            leg: cells[2].as_str()?.to_string(),
            last_price: cells[3].as_f64().unwrap_or(0.0),
            // Pre-moneyness-column rows read NULL — tolerate as UNKNOWN.
            moneyness: cells[4].as_str().unwrap_or("UNKNOWN").to_string(),
            underlying_spot: cells[5].as_f64().unwrap_or(0.0),
            expiry_nanos: cells[6].as_i64().unwrap_or(0),
            fetched_nanos: cells[7].as_i64().unwrap_or(0),
        });
    }
    Some((rows, truncated))
}

/// Groups ts-ordered rehydrate rows into one snapshot per minute (the
/// chain legs' publish shape). Rows with an unparsable leg or strike are
/// skipped (never fabricated); moneyness falls back to `Unknown`. The ATM
/// anchor is re-derived from the row's own spot + the const step table —
/// identical inputs to the live classification path.
#[must_use]
pub fn build_minute_snapshots(
    feed: Feed,
    underlying: ChainUnderlying,
    rows: &[ChainRehydrateRow],
) -> Vec<ChainMoneynessSnapshot> {
    let mut out: Vec<ChainMoneynessSnapshot> = Vec::with_capacity(rows.len() / 8 + 1);
    for row in rows {
        if row.ts_nanos == 0 {
            // The empty-sentinel minute value can never be recorded.
            continue;
        }
        let needs_new = match out.last() {
            Some(last) => last.minute_ts_ist_nanos != row.ts_nanos,
            None => true,
        };
        if needs_new {
            let spot_paise = price_to_paise_guarded(row.underlying_spot).unwrap_or(0);
            let atm = if spot_paise > 0 {
                strike_step_paise(underlying.as_str())
                    .and_then(|step| atm_strike_paise(spot_paise, step))
                    .unwrap_or(0)
            } else {
                0
            };
            out.push(ChainMoneynessSnapshot {
                feed,
                underlying,
                minute_ts_ist_nanos: row.ts_nanos,
                fetched_at_ist_nanos: row.fetched_nanos,
                underlying_spot: row.underlying_spot,
                underlying_spot_paise: spot_paise,
                atm_strike_paise: atm,
                expiry_ist_nanos: row.expiry_nanos,
                spot_missing: spot_paise == 0,
                rows: Vec::with_capacity(16),
            });
        }
        let Some(snap) = out.last_mut() else {
            continue;
        };
        let Some(leg) = OptionLeg::parse(&row.leg) else {
            continue;
        };
        let Some(strike_paise) = price_to_paise_guarded(row.strike) else {
            continue;
        };
        snap.rows.push(SnapshotRow {
            strike_paise,
            ltp_paise: price_to_paise_guarded(row.last_price).unwrap_or(0),
            leg,
            moneyness: Moneyness::parse(&row.moneyness).unwrap_or(Moneyness::Unknown),
        });
    }
    out
}

/// The session's 30-minute rehydrate window starts for the day — only
/// windows that have already OPENED (start < now) are read; a pre-market
/// boot reads nothing (the live publishes own the day from 09:16).
#[must_use]
pub fn rehydrate_window_starts(day_start_nanos: i64, now_ist_nanos: i64) -> Vec<i64> {
    let session_open = day_start_nanos + SESSION_OPEN_SECS_OF_DAY * NANOS_PER_SEC;
    let window_nanos = (RAM_CHAIN_REHYDRATE_WINDOW_MINUTES as i64) * NANOS_PER_MINUTE;
    let mut out = Vec::with_capacity(RAM_CHAIN_REHYDRATE_WINDOW_COUNT);
    for k in 0..RAM_CHAIN_REHYDRATE_WINDOW_COUNT {
        let start = session_open + (k as i64) * window_nanos;
        if start < now_ist_nanos {
            out.push(start);
        }
    }
    out
}

/// IST "now" in nanos (wall clock + the fixed IST offset — the
/// `option_chain_1m` `ts` convention).
fn ist_now_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp()
        .saturating_add(IST_UTC_OFFSET_SECS)
        .saturating_mul(NANOS_PER_SEC)
}

/// One bounded `/exec` GET (streamed 8 MiB cap — the fold's hardened
/// shape). Returns the body or the failing stage name.
async fn rehydrate_exec_query(
    client: &reqwest::Client,
    exec_url: &str,
    sql: &str,
) -> Result<String, &'static str> {
    let mut response = client
        .get(exec_url)
        .query(&[("query", sql)])
        .send()
        .await
        .map_err(|_| "rehydrate_query")?;
    if !response.status().is_success() {
        return Err("rehydrate_query");
    }
    if let Some(len) = response.content_length()
        && len > FOLD_MAX_RESPONSE_BYTES as u64
    {
        return Err("rehydrate_query");
    }
    let mut body: Vec<u8> = Vec::new(); // O(1) EXEMPT: cold-path bounded body read
    while let Some(chunk) = response.chunk().await.map_err(|_| "rehydrate_query")? {
        if !accumulate_capped(&mut body, &chunk, FOLD_MAX_RESPONSE_BYTES) {
            return Err("rehydrate_query");
        }
    }
    String::from_utf8(body).map_err(|_| "rehydrate_parse")
}

/// The one-shot rehydrate body (see the module doc). Every failed window
/// is skipped LOUDLY (coded error + counter) — remaining windows still run;
/// live publishes fill forward regardless.
async fn run_chain_day_rehydrate(questdb: QuestDbConfig) {
    let Some(store) = chain_day_store() else {
        // Store not installed (disabled) — the caller never spawns us then;
        // defensive no-op.
        return;
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(RAM_CHAIN_REHYDRATE_HTTP_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            counter!("tv_ram_store_errors_total", "stage" => "rehydrate_query").increment(1);
            error!(
                code = ErrorCode::RamStore01Degraded.code_str(),
                stage = "rehydrate_query",
                error = %err,
                "RAMSTORE-01: rehydrate HTTP client build failed — chain day \
                 store starts shallow; live publishes fill forward"
            );
            return;
        }
    };
    let exec_url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let today = today_ist();
    let day_start = day_start_nanos(today);
    let starts = rehydrate_window_starts(day_start, ist_now_nanos());
    if starts.is_empty() {
        info!("market_ram_store: pre-session boot — no chain minutes to rehydrate yet");
        return;
    }
    let window_nanos = (RAM_CHAIN_REHYDRATE_WINDOW_MINUTES as i64) * NANOS_PER_MINUTE;
    let window_limit = RAM_CHAIN_REHYDRATE_WINDOW_MINUTES * store.chain_row_cap();
    let mut total_minutes = 0u64;
    for &feed in Feed::ALL {
        let mut feed_minutes = 0u64;
        for &underlying in ChainUnderlying::ALL {
            for &window_start in &starts {
                let sql = chain_rehydrate_sql(
                    feed.as_str(),
                    underlying.as_str(),
                    window_start,
                    window_start + window_nanos,
                    window_limit,
                );
                let body = match rehydrate_exec_query(&client, &exec_url, &sql).await {
                    Ok(b) => b,
                    Err(stage) => {
                        counter!("tv_ram_store_errors_total", "stage" => stage).increment(1);
                        error!(
                            code = ErrorCode::RamStore01Degraded.code_str(),
                            stage,
                            feed = feed.as_str(),
                            underlying = underlying.as_str(),
                            window_start_nanos = window_start,
                            "RAMSTORE-01: chain rehydrate window read failed — \
                             window skipped (live publishes fill forward; the \
                             next boot re-runs the full rehydrate)"
                        );
                        continue;
                    }
                };
                let Some((rows, truncated)) = parse_chain_rehydrate_rows(&body, window_limit)
                else {
                    counter!("tv_ram_store_errors_total", "stage" => "rehydrate_parse")
                        .increment(1);
                    error!(
                        code = ErrorCode::RamStore01Degraded.code_str(),
                        stage = "rehydrate_parse",
                        feed = feed.as_str(),
                        underlying = underlying.as_str(),
                        window_start_nanos = window_start,
                        "RAMSTORE-01: chain rehydrate window dataset unparsable — skipped"
                    );
                    continue;
                };
                if truncated {
                    counter!("tv_ram_store_errors_total", "stage" => "rehydrate_truncated")
                        .increment(1);
                    error!(
                        code = ErrorCode::RamStore01Degraded.code_str(),
                        stage = "rehydrate_truncated",
                        feed = feed.as_str(),
                        underlying = underlying.as_str(),
                        window_start_nanos = window_start,
                        limit = window_limit,
                        "RAMSTORE-01: chain rehydrate window hit its explicit \
                         LIMIT — a partial window is never trusted; skipped \
                         (raise the bound in a reviewed PR, never silently)"
                    );
                    continue;
                }
                for snap in build_minute_snapshots(feed, underlying, &rows) {
                    if store.record_rehydrated(snap) == ChainRecordOutcome::Recorded {
                        feed_minutes += 1;
                    }
                }
            }
        }
        if feed_minutes > 0 {
            counter!("tv_ram_store_rehydrate_minutes_total", "feed" => feed.as_str())
                .increment(feed_minutes);
        }
        total_minutes += feed_minutes;
    }
    info!(
        minutes = total_minutes,
        windows = starts.len(),
        "market_ram_store: chain day rehydrate complete — today's already-\
         captured chain minutes are RAM-resident (live minutes always outrank \
         rehydrated ones)"
    );
}

/// Spawns the ONE-SHOT chain-day rehydrate with a join classifier — a
/// panicking incarnation (unwind builds) is reported loudly, never
/// silently lost; it is NOT respawned (the rehydrate is boot-scoped and
/// idempotent at the NEXT boot; live publishes fill forward meanwhile).
// TEST-EXEMPT: tokio spawn + live QuestDB read — the pure legs carry the unit tests; wiring pinned by ram_store_wiring_guard.
pub fn spawn_chain_day_rehydrate(questdb: QuestDbConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let handle = tokio::spawn(run_chain_day_rehydrate(questdb));
        let result = handle.await;
        let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&result);
        if reason != "clean_exit" {
            counter!("tv_ram_store_errors_total", "stage" => "task_respawn").increment(1);
            error!(
                code = ErrorCode::RamStore01Degraded.code_str(),
                stage = "task_respawn",
                reason,
                task = "chain_day_rehydrate",
                "RAMSTORE-01: chain rehydrate task died abnormally — chain day \
                 store starts shallow this boot (live publishes fill forward; \
                 release builds abort on panic per the honest envelope)"
            );
        }
    })
}

// ---------------------------------------------------------------------------
// Stats / heartbeat task
// ---------------------------------------------------------------------------

/// One stats pass: publish the residency gauges (the operator's "is the
/// month actually in RAM?" read surface).
fn publish_ram_store_stats() {
    let mut estimated_bytes = 0u64;
    if let Some(store) = spot_bar_store() {
        let stats = store.stats();
        for &feed in Feed::ALL {
            gauge!("tv_ram_store_spot_bars_resident", "feed" => feed.as_str())
                .set(stats.bars_resident_per_feed[feed.index()] as f64);
            gauge!("tv_ram_store_spot_days_depth", "feed" => feed.as_str())
                .set(f64::from(stats.min_depth_days_per_feed[feed.index()]));
        }
        estimated_bytes += stats.estimated_bytes;
    }
    if let Some(store) = chain_day_store() {
        let stats = store.stats();
        for &feed in Feed::ALL {
            gauge!("tv_ram_store_chain_minutes_resident", "feed" => feed.as_str())
                .set(stats.minutes_resident_per_feed[feed.index()] as f64);
        }
        estimated_bytes += stats.estimated_bytes;
    }
    gauge!("tv_ram_store_estimated_bytes").set(estimated_bytes as f64);
    counter!("tv_ram_store_heartbeat_total").increment(1);
}

/// The stats loop body (60 s cadence; dense heartbeat — a flatline means
/// the task is dead).
async fn run_ram_store_stats_loop() {
    let mut interval = tokio::time::interval(Duration::from_secs(RAM_STORE_STATS_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        publish_ram_store_stats();
    }
}

/// Spawns the SUPERVISED stats/heartbeat task (house respawn pattern —
/// DISK-WATCHER-01 family; unwind builds self-heal, release builds abort
/// per `panic = "abort"` — the honest TICK-FLUSH-01 envelope).
// TEST-EXEMPT: tokio spawn loop — gauge names pinned by ram_store_wiring_guard; stats math tested in the store crates.
pub fn spawn_ram_store_stats_task() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_ram_store_stats_loop());
            let result = handle.await;
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&result);
            counter!("tv_ram_store_errors_total", "stage" => "task_respawn").increment(1);
            error!(
                code = ErrorCode::RamStore01Degraded.code_str(),
                stage = "task_respawn",
                reason,
                task = "ram_store_stats",
                "RAMSTORE-01: RAM store stats task died — respawning after backoff \
                 (a flatlining tv_ram_store_heartbeat_total means release-build \
                 abort; restart is the recovery)"
            );
            tokio::time::sleep(Duration::from_secs(RAM_STORE_STATS_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 20_650 * 86_400 * NANOS_PER_SEC;
    const MIN: i64 = NANOS_PER_MINUTE;

    fn row(ts: i64, strike: f64, leg: &str, moneyness: &str, spot: f64) -> ChainRehydrateRow {
        ChainRehydrateRow {
            ts_nanos: ts,
            strike,
            leg: leg.to_string(),
            last_price: 123.45,
            moneyness: moneyness.to_string(),
            underlying_spot: spot,
            expiry_nanos: DAY + 3 * 86_400 * NANOS_PER_SEC,
            fetched_nanos: ts + NANOS_PER_SEC,
        }
    }

    #[test]
    fn test_chain_rehydrate_sql_shape() {
        let sql = chain_rehydrate_sql("dhan", "NIFTY", 2_000_000, 3_000_000, 30_000);
        // Micros WHERE window (nanos / 1000) + nanos projections + LIMIT.
        assert!(sql.contains("(ts / 1) * 1000 AS ts_nanos"));
        assert!(sql.contains("(expiry / 1) * 1000 AS expiry_nanos"));
        assert!(sql.contains("(fetched_at / 1) * 1000 AS fetched_nanos"));
        assert!(sql.contains("FROM option_chain_1m"));
        assert!(sql.contains("feed = 'dhan'"));
        assert!(sql.contains("underlying_symbol = 'NIFTY'"));
        assert!(sql.contains("ts >= 2000 AND ts < 3000"));
        assert!(sql.contains("ORDER BY ts ASC LIMIT 30000"));
        assert!(sql.contains("moneyness"));
        assert!(sql.contains("underlying_spot"));
    }

    #[test]
    fn test_parse_chain_rehydrate_rows_and_truncation_tripwire() {
        let body = r#"{"dataset":[
            [1000000000,24500.0,"CE",120.5,"ITM",24536.4,2000000000,1500000000],
            [1000000000,24550.0,"PE",98.0,null,24536.4,2000000000,1500000000]
        ]}"#;
        let (rows, truncated) = parse_chain_rehydrate_rows(body, 100).expect("parses");
        assert!(!truncated);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].leg, "CE");
        assert_eq!(rows[0].moneyness, "ITM");
        // NULL moneyness (pre-moneyness-column rows) tolerated as UNKNOWN.
        assert_eq!(rows[1].moneyness, "UNKNOWN");
        assert_eq!(rows[1].ts_nanos, 1_000_000_000);
        // Truncation tripwire: dataset length reaching the LIMIT flags it.
        let (_, truncated2) = parse_chain_rehydrate_rows(body, 2).expect("parses");
        assert!(truncated2, "dataset.len() >= limit must flag truncated");
        // Malformed rows fail the whole parse (never a partial trust).
        assert!(parse_chain_rehydrate_rows(r#"{"dataset":[[1]]}"#, 10).is_none());
        assert!(parse_chain_rehydrate_rows("not json", 10).is_none());
    }

    #[test]
    fn test_build_minute_snapshots_groups_rows_per_minute() {
        let m1 = DAY + 556 * MIN;
        let m2 = DAY + 557 * MIN;
        let rows = [
            row(m1, 24_500.0, "CE", "ITM", 24_536.4),
            row(m1, 24_550.0, "PE", "OTM", 24_536.4),
            row(m1, -1.0, "CE", "ITM", 24_536.4), // invalid strike — skipped
            row(m1, 24_600.0, "XX", "ITM", 24_536.4), // invalid leg — skipped
            row(m2, 24_500.0, "CE", "banana", 24_540.0), // moneyness → Unknown
        ];
        let snaps = build_minute_snapshots(Feed::Dhan, ChainUnderlying::Nifty, &rows);
        assert_eq!(snaps.len(), 2, "two distinct minutes");
        assert_eq!(snaps[0].minute_ts_ist_nanos, m1);
        assert_eq!(snaps[0].rows.len(), 2, "invalid strike/leg rows skipped");
        assert_eq!(snaps[0].underlying_spot_paise, 2_453_640);
        assert!(!snaps[0].spot_missing);
        // ATM re-derived from spot + const step (NIFTY step = 50.00).
        assert_eq!(snaps[0].atm_strike_paise, 2_453_640 / 5_000 * 5_000 + 5_000);
        assert_eq!(snaps[1].minute_ts_ist_nanos, m2);
        assert_eq!(snaps[1].rows[0].moneyness, Moneyness::Unknown);
        // A zero-spot minute is honest: spot_missing + no fabricated ATM.
        let zero_spot = [row(m1, 24_500.0, "CE", "ITM", 0.0)];
        let s = build_minute_snapshots(Feed::Groww, ChainUnderlying::Sensex, &zero_spot);
        assert_eq!(s.len(), 1);
        assert!(s[0].spot_missing);
        assert_eq!(s[0].atm_strike_paise, 0);
        // A ts==0 row (sentinel value) is never built into a snapshot.
        let sentinel = [row(0, 24_500.0, "CE", "ITM", 24_536.4)];
        assert!(build_minute_snapshots(Feed::Dhan, ChainUnderlying::Nifty, &sentinel).is_empty());
    }

    #[test]
    fn test_rehydrate_window_starts_cover_session() {
        let day = DAY;
        let open = day + SESSION_OPEN_SECS_OF_DAY * NANOS_PER_SEC;
        let win = (RAM_CHAIN_REHYDRATE_WINDOW_MINUTES as i64) * MIN;
        // Pre-market boot: nothing to rehydrate.
        assert!(rehydrate_window_starts(day, open - NANOS_PER_SEC).is_empty());
        // Mid-session (11:00 IST = open + 105 min): windows 09:15..11:00
        // have opened — 09:15, 09:45, 10:15, 10:45.
        let now = open + 105 * MIN;
        let starts = rehydrate_window_starts(day, now);
        assert_eq!(starts.len(), 4);
        assert_eq!(starts[0], open);
        assert_eq!(starts[3], open + 3 * win);
        // Post-close boot: ALL 13 windows (covering 09:15..15:45) open.
        let post = day + (15 * 3600 + 50 * 60) * NANOS_PER_SEC;
        let all = rehydrate_window_starts(day, post);
        assert_eq!(all.len(), RAM_CHAIN_REHYDRATE_WINDOW_COUNT);
        assert_eq!(
            *all.last().expect("non-empty"),
            open + 12 * win,
            "last window opens 15:15 IST and covers through 15:45"
        );
        // The 13-window grid covers the whole [09:15, 15:30) session.
        assert!(open + 13 * win >= day + (15 * 3600 + 30 * 60) * NANOS_PER_SEC);
    }
}
