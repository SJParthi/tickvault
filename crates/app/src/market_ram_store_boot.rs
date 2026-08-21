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
//! Every degrade is a coded RAMSTORE-01 `error!`/`warn!` (the boot/
//! rehydrate/task degrades here are `error!`; the chain store's own
//! row-cap / day-drop / minute-cap degrades are `warn!` — PR-2 round-1
//! doc alignment). Log-sink-only delivery boundary per the runbook —
//! QuestDB remains the durable truth; a RAM degrade re-fills at the next
//! boot.

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
use tickvault_storage::option_chain_1m_persistence::OPTION_CHAIN_1M_TABLE;
use tickvault_trading::in_mem::spot_bar_store::{
    MAX_SPOT_BAR_SLOTS, estimated_capacity_bytes, install_spot_bar_store, spot_bar_store,
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

/// Ceiling on the spot store's PROJECTED ring capacity, in bytes — the
/// FALLBACK, used only when the host's memory cannot be read.
///
/// The spot rings are `VecDeque::with_capacity(bars_per_day × spot_days)`,
/// allocated **eagerly when a slot is created** — so this memory is committed
/// the moment an instrument is first seen, whether or not a single bar ever
/// fills it. Capacity is therefore a promise the process makes up front, not
/// a high-water mark it grows into, and it is the right thing to bound.
///
/// 10 GiB is the operator's stated current-day RAM budget (2026-08-12,
/// restated three times), sized against the r8g.xlarge 32 GiB host.
///
/// **It is a FALLBACK rather than the budget itself** (2026-08-21). A budget
/// pinned to one machine is the same shape the frame ring was repaired for:
/// it is correct only while the host never changes, and it is silently wrong
/// the moment it does — too generous on a smaller box, needlessly tight on a
/// larger one, with no signal either way. [`ram_store_spot_capacity_budget_bytes`]
/// derives it from the host at runtime and falls back to this value only when
/// the host cannot be read, loudly.
pub const RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// The spot store's share of host memory, as an exact fraction: 5/16.
///
/// Chosen so the reference host reproduces the operator's stated figure
/// EXACTLY rather than approximately — 5/16 × 32 GiB = 10 GiB to the byte.
/// A percentage would have been 31.25%, and rounding it to 31% would have
/// quietly tightened the live budget by 80 MiB while appearing to preserve
/// it. The fraction is the honest way to say "same as today, but derived".
///
/// The remaining 11/16 is not slack: it is QuestDB (`QDB_MEM_LIMIT` default
/// 12g), the aggregator's ~155 MB, the seal and frame rings, and the OS.
pub const RAM_STORE_SPOT_BUDGET_NUMERATOR: u64 = 5;
/// Denominator of [`RAM_STORE_SPOT_BUDGET_NUMERATOR`]'s fraction.
pub const RAM_STORE_SPOT_BUDGET_DENOMINATOR: u64 = 16;

/// Above this, a cgroup limit is "unlimited" rather than a real bound.
///
/// cgroup v1 reports no-limit as a saturated `u64` near `i64::MAX`, and v2
/// uses the literal `max`. 1 PiB is far above any real host and far below
/// the saturated sentinel, so it separates the two without pattern-matching
/// on a specific kernel's choice of sentinel.
const CGROUP_UNLIMITED_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024 * 1024 * 1024;

/// Parses `MemTotal:` out of `/proc/meminfo`, returning BYTES.
///
/// `/proc/meminfo` reports kB (kibibytes, despite the label). Returns `None`
/// on any shape it does not recognise rather than guessing — a wrong memory
/// figure produces a wrong budget, which is worse than no budget at all.
fn parse_meminfo_total_bytes(contents: &str) -> Option<u64> {
    for line in contents.lines() {
        let rest = match line.strip_prefix("MemTotal:") {
            Some(r) => r,
            None => continue,
        };
        let mut parts = rest.split_whitespace();
        let value: u64 = parts.next()?.parse().ok()?;
        // The unit is present in every kernel that ships this file, but a
        // missing unit is treated as kB rather than refused: the value's
        // magnitude is unambiguous and refusing would lose a real reading.
        return match parts.next() {
            Some("kB") | Some("KB") | None => value.checked_mul(1024),
            _ => None,
        };
    }
    None
}

/// Parses a cgroup memory limit (v1 `memory.limit_in_bytes`, v2
/// `memory.max`), returning `None` for "unlimited" in either dialect.
fn parse_cgroup_limit_bytes(contents: &str) -> Option<u64> {
    let trimmed = contents.trim();
    if trimmed.is_empty() || trimmed == "max" {
        return None;
    }
    let value: u64 = trimmed.parse().ok()?;
    if value >= CGROUP_UNLIMITED_THRESHOLD_BYTES {
        return None;
    }
    Some(value)
}

/// The spot budget for a given host memory figure.
///
/// Saturating rather than wrapping: a nonsensical input yields a clamped
/// number instead of a tiny one that would silently pass the projection
/// check it exists to fail.
fn budget_from_host_bytes(host_bytes: u64) -> u64 {
    host_bytes
        .saturating_mul(RAM_STORE_SPOT_BUDGET_NUMERATOR)
        .saturating_div(RAM_STORE_SPOT_BUDGET_DENOMINATOR)
}

/// The memory this process may actually use, in bytes.
///
/// Takes the MINIMUM of the machine's RAM and any cgroup limit, because both
/// bind and the smaller one is what the OOM killer enforces. Checking the
/// cgroup is what makes this the same code on the AWS box (no container
/// limit) and in a Docker dev run (limit set) — the common-runtime property
/// a hardcoded constant cannot have.
fn host_memory_limit_bytes() -> Option<u64> {
    let machine = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .as_deref()
        .and_then(parse_meminfo_total_bytes);

    let cgroup = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .filter_map(|p| std::fs::read_to_string(p).ok())
    .find_map(|c| parse_cgroup_limit_bytes(&c));

    match (machine, cgroup) {
        (Some(m), Some(c)) => Some(m.min(c)),
        (Some(m), None) => Some(m),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

/// The spot store's RAM budget, derived from the host once per process.
///
/// Resolved on first call and cached, so every later call is O(1) and the
/// value can never change mid-process — a budget that drifted between the
/// projection check and the log line would make the two disagree.
///
/// Falls back to [`RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES`] with a
/// coded `warn!` when the host cannot be read. That path is loud on purpose:
/// silently reverting to a number sized for one specific machine is exactly
/// the failure this function exists to remove.
pub fn ram_store_spot_capacity_budget_bytes() -> u64 {
    static BUDGET: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| match host_memory_limit_bytes() {
        Some(host_bytes) => budget_from_host_bytes(host_bytes),
        None => {
            warn!(
                code = ErrorCode::RamStore01Degraded.code_str(),
                stage = "capacity_budget",
                fallback_bytes = RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES,
                "RAMSTORE-01: host memory could not be read — the spot RAM \
                 budget falls back to the r8g.xlarge-sized figure. On a \
                 SMALLER host that budget is too generous and the projection \
                 check will under-report; verify the host size by hand"
            );
            RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES
        }
    })
}

/// The spot store's own slot ceiling, as the `u32` the capacity estimator
/// takes. Same 25,000 the aggregator, indicator engine and day-OHLC tracker
/// are sized to — projecting at anything smaller is what hid the overshoot.
pub const MAX_SPOT_BAR_SLOTS_U32: u32 = MAX_SPOT_BAR_SLOTS as u32;

/// A small illustrative slot count, reported ALONGSIDE the ceiling so the
/// gap between today's 4-index universe and the 25,000 target is visible in
/// one line rather than inferred. Never the only figure logged — reporting
/// this alone is precisely the bug being fixed.
pub const RAM_STORE_SAMPLE_SLOT_COUNT: u32 = 8;

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
    // The projected capacity at the store's OWN slot ceiling, not at a
    // sample size.
    //
    // This line used to pass a hardcoded `8` for slot_count, and that single
    // literal is why a 34.9 GB configuration read as harmless for months.
    // Eight slots is roughly today's universe (the 4 SPOT_1M_REST_INDICES),
    // so at `spot_days = 35` the log printed ~11 MB and every reader
    // reasonably concluded the store was cheap. The store's real ceiling is
    // `MAX_SPOT_BAR_SLOTS` (25,000) — the same number the aggregator, the
    // indicator engine and the day-OHLC tracker are all sized to — at which
    // the identical config commits **3,000× more memory**.
    //
    // A sizing log that is blind to scale is worse than no sizing log: it
    // answers the question "is this expensive?" with a number that is
    // accurate for a universe nobody is targeting. Project at the ceiling,
    // and report BOTH so the gap between today and the target is visible
    // rather than inferred.
    let projected_bytes = estimated_capacity_bytes(cfg.spot_days, MAX_SPOT_BAR_SLOTS_U32);
    let today_bytes = estimated_capacity_bytes(cfg.spot_days, RAM_STORE_SAMPLE_SLOT_COUNT);
    // Resolved ONCE and reused for both the check and the log line: reading
    // it twice would let the two disagree if the fallback path ever fired
    // between them, and a check that reports a different budget than it
    // enforced is unreadable at 3am.
    let budget_bytes_resolved = ram_store_spot_capacity_budget_bytes();

    if projected_bytes > budget_bytes_resolved {
        // Fail LOUD, not closed. Refusing the install would leave the
        // decision path with no RAM store at all, which is strictly worse
        // than an oversized one — and the overshoot only materialises as the
        // universe actually grows, so there is real time to act. The gauge +
        // this coded line are the signal; the operator lowers `spot_days`.
        error!(
            code = ErrorCode::RamStore01Degraded.code_str(),
            stage = "capacity_projection",
            spot_days = cfg.spot_days,
            projected_bytes,
            budget_bytes = budget_bytes_resolved,
            slot_ceiling = MAX_SPOT_BAR_SLOTS_U32,
            "RAMSTORE-01: spot ring capacity at the slot ceiling EXCEEDS the \
             RAM budget — the rings allocate eagerly per slot, so this much \
             memory is committed as instruments are first seen, not as bars \
             arrive. Lower [market_ram_store] spot_days until the projection \
             fits, or raise the budget deliberately"
        );
    }

    info!(
        spot_days = cfg.spot_days,
        chain_row_cap = cfg.chain_row_cap,
        spot_capacity_bytes_at_slot_ceiling = projected_bytes,
        spot_capacity_bytes_at_sample = today_bytes,
        slot_ceiling = MAX_SPOT_BAR_SLOTS_U32,
        budget_bytes = budget_bytes_resolved,
        "market_ram_store: RAM residency stores installed — spot depth bounded \
         by CAPTURED history (shown honestly by tv_ram_store_spot_days_depth), \
         options current-day (chain publishes + boot rehydrate)"
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
/// explicit LIMIT tripwire). The emitted `LIMIT` is `limit + 1` — ONE
/// extra row past the trusted bound (PR-2 round-1 fix): a returned
/// dataset of exactly `limit` rows is a legitimately-complete
/// exact-boundary window, while `> limit` rows proves genuine
/// truncation ([`parse_chain_rehydrate_rows`] flags on `len > limit`).
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
    let fetch_limit = limit.saturating_add(1);
    format!(
        "SELECT (ts / 1) * 1000 AS ts_nanos, strike, leg, last_price, moneyness, \
         underlying_spot, (expiry / 1) * 1000 AS expiry_nanos, \
         (fetched_at / 1) * 1000 AS fetched_nanos \
         FROM {OPTION_CHAIN_1M_TABLE} \
         WHERE feed = '{feed}' AND underlying_symbol = '{underlying_symbol}' \
         AND ts >= {start_micros} AND ts < {end_micros} \
         ORDER BY ts ASC LIMIT {fetch_limit}"
    )
}

/// Parses a rehydrate `/exec` dataset. Returns `(rows, truncated)` —
/// `truncated` means MORE than `limit` rows came back (the query fetches
/// `limit + 1`, so `len > limit` proves genuine truncation while an
/// exact-boundary `len == limit` window is legitimately complete — PR-2
/// round-1 fix). A truncated window is NEVER trusted; the caller skips
/// it loudly.
#[must_use]
pub fn parse_chain_rehydrate_rows(
    body: &str,
    limit: usize,
) -> Option<(Vec<ChainRehydrateRow>, bool)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let dataset = value.get("dataset")?.as_array()?;
    let truncated = dataset.len() > limit;
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
                    // PR-2 round-1 LOW: a row-cap-truncated minute is STILL a
                    // stored minute — count it (the truncation itself is
                    // separately loud via tv_ram_store_dropped_total
                    // reason="row_cap" + the chain_truncated warn).
                    if matches!(
                        store.record_rehydrated(snap),
                        ChainRecordOutcome::Recorded | ChainRecordOutcome::RecordedTruncated
                    ) {
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
    if total_minutes == 0 {
        // PR-2 round-1 LOW: zero minutes is DEGRADED/EMPTY, never worded as
        // a clean completion (audit Rule 11 — no false-OK phrasing).
        info!(
            windows = starts.len(),
            "market_ram_store: chain day rehydrate finished with ZERO minutes \
             rehydrated — either no chain rows were captured today yet, or \
             every window degraded (check RAMSTORE-01 rehydrate_* stages); \
             live publishes fill forward"
        );
    } else {
        info!(
            minutes = total_minutes,
            windows = starts.len(),
            "market_ram_store: chain day rehydrate complete — today's already-\
             captured chain minutes are RAM-resident (live minutes always outrank \
             rehydrated ones)"
        );
    }
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
        // PR-2 round-1 HIGH: the spot store is a pure ring core with NO emit
        // sites — its lifetime over-window drop total was previously an
        // UNPUBLISHED stat. Publish it here as a counter-style monotonic
        // gauge so spot drops are a real signal, not a runbook fiction
        // (chain-side drops keep their own tv_ram_store_dropped_total
        // reasons: row_cap / day_drop / minute_cap).
        gauge!("tv_ram_store_spot_dropped_over_window").set(stats.dropped_over_window as f64);
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

    // -----------------------------------------------------------------
    // Current-day RAM at the 25,000-instrument ceiling.
    //
    // The 2026-08-12 budget was computed BEFORE depth-20 / depth-200
    // became a persisted stream (2026-08-15). Depth is the only path in
    // the process whose RAM is O(ROWS) rather than O(instruments), so it
    // is the one addition that could invalidate that budget — and the
    // arithmetic below is what proves it does not, rather than assuming.
    // -----------------------------------------------------------------

    /// Every eagerly-committed current-day RAM term at the slot ceiling,
    /// derived from the real constants and `size_of` rather than quoted.
    fn current_day_ram_terms_at_ceiling() -> Vec<(&'static str, u64)> {
        use tickvault_trading::candles::multi_tf_aggregator::AGGREGATOR_MAX_SLOTS;
        use tickvault_trading::candles::seal_ring::SEAL_BUFFER_CAPACITY;
        use tickvault_trading::candles::tf_index::TF_COUNT;

        let slots = AGGREGATOR_MAX_SLOTS as u64;
        vec![
            // spot_days = 1 (current day) at the full slot ceiling.
            (
                "spot_bar_store",
                estimated_capacity_bytes(1, MAX_SPOT_BAR_SLOTS_U32),
            ),
            // The aggregator's live candle grid: one cell per (slot, TF).
            (
                "aggregator",
                slots
                    * TF_COUNT as u64
                    * core::mem::size_of::<
                        tickvault_trading::candles::live_candle_state::LiveCandleState,
                    >() as u64,
            ),
            // The seal ring is already slots × TF_COUNT entries.
            (
                "seal_ring",
                SEAL_BUFFER_CAPACITY as u64
                    * core::mem::size_of::<tickvault_trading::candles::seal_ring::BufferedSeal>()
                        as u64,
            ),
            // The frame ring's byte CEILING — a bound, not a preallocation,
            // but it must be budgeted because it can legitimately be reached.
            (
                "frame_ring_ceiling",
                crate::dhan_feed_stack::FRAME_RING_MAX_BYTES as u64,
            ),
            // Depth's ONLY RAM term: the un-flushed ILP buffer, bounded by the
            // row threshold. ~160 B of line protocol per row.
            (
                "depth_ilp_buffer",
                crate::dhan_feed_stack::DEPTH_FLUSH_ROW_THRESHOLD * 160,
            ),
        ]
    }

    // ---- host-derived RAM budget (2026-08-21) -------------------------

    #[test]
    fn the_reference_host_reproduces_the_operator_budget_exactly() {
        // The whole point of the 5/16 fraction: on the r8g.xlarge the
        // derived budget must equal the operator's stated 10 GiB TO THE
        // BYTE, so deriving it changes nothing about the live box. A
        // rounded percentage would drift here.
        let r8g_xlarge = 32u64 * 1024 * 1024 * 1024;
        assert_eq!(
            budget_from_host_bytes(r8g_xlarge),
            RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES,
            "5/16 of 32 GiB must be exactly the 10 GiB fallback"
        );
    }

    #[test]
    fn the_budget_tracks_the_host_up_and_down() {
        // The failure being fixed: the budget did not move when the host
        // did. Half the host must halve it; double must double it.
        let base = 32u64 * 1024 * 1024 * 1024;
        assert_eq!(
            budget_from_host_bytes(base / 2),
            RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES / 2
        );
        assert_eq!(
            budget_from_host_bytes(base * 2),
            RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES * 2
        );
    }

    #[test]
    fn budget_saturates_instead_of_wrapping() {
        // u64::MAX * 5 wraps to a SMALL number, which would silently pass
        // the projection check this budget exists to fail.
        assert!(budget_from_host_bytes(u64::MAX) > RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES);
        assert_eq!(budget_from_host_bytes(0), 0);
    }

    #[test]
    fn meminfo_total_is_parsed_from_a_real_file_shape() {
        let sample = "MemTotal:       32819668 kB\nMemFree:         1234 kB\n";
        assert_eq!(
            parse_meminfo_total_bytes(sample),
            Some(32_819_668u64 * 1024)
        );
    }

    #[test]
    fn meminfo_refuses_shapes_it_does_not_understand() {
        // Refusing yields the loud fallback. GUESSING yields a wrong budget
        // that looks authoritative, which is strictly worse.
        assert_eq!(parse_meminfo_total_bytes(""), None);
        assert_eq!(parse_meminfo_total_bytes("MemFree: 100 kB\n"), None);
        assert_eq!(parse_meminfo_total_bytes("MemTotal:       zzz kB\n"), None);
        assert_eq!(parse_meminfo_total_bytes("MemTotal:       12 GB\n"), None);
    }

    #[test]
    fn cgroup_unlimited_is_not_mistaken_for_a_limit() {
        // Both dialects. v1 saturates near i64::MAX; v2 writes "max".
        // Reading either as a real limit would produce an astronomically
        // large budget and disable the check entirely.
        assert_eq!(parse_cgroup_limit_bytes("max\n"), None);
        assert_eq!(parse_cgroup_limit_bytes("9223372036854771712\n"), None);
        assert_eq!(parse_cgroup_limit_bytes(""), None);
        assert_eq!(
            parse_cgroup_limit_bytes("2147483648\n"),
            Some(2_147_483_648)
        );
    }

    #[test]
    fn ram_store_spot_capacity_budget_bytes_is_stable_and_plausible() {
        // Exercises the real host path, not a fixture. Cannot assert a
        // specific figure -- CI runners differ, which is the entire point --
        // so it asserts the two properties that must hold anywhere: it is
        // non-zero, and it is CACHED so the check and the log can never
        // disagree.
        let first = ram_store_spot_capacity_budget_bytes();
        assert!(first > 0, "a zero budget would fail every projection");
        assert_eq!(
            first,
            ram_store_spot_capacity_budget_bytes(),
            "the budget must resolve once per process"
        );
    }

    #[test]
    fn current_day_ram_at_25k_instruments_fits_the_operator_budget() {
        let terms = current_day_ram_terms_at_ceiling();
        let total: u64 = terms.iter().map(|(_, b)| *b).sum();
        let report: Vec<String> = terms
            .iter()
            .map(|(n, b)| format!("{n}={:.1} MB", *b as f64 / 1_048_576.0))
            .collect();
        assert!(
            total <= RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES,
            "current-day RAM at the 25,000-instrument ceiling is {:.2} GB, over the \
             operator's 10 GiB budget (2026-08-12, stated three times). Terms: {}. \
             Raw ticks contribute ZERO by design (folded then dropped), so an \
             overshoot here means one of these structures grew — check which \
             before raising the budget.",
            total as f64 / 1_073_741_824.0,
            report.join(", ")
        );
    }

    #[test]
    fn depth_is_a_rounding_error_in_the_current_day_ram_budget() {
        // The load-bearing claim of the 2026-08-15 depth change: depth adds
        // RAM proportional to the FLUSH THRESHOLD, not to instruments, rows
        // captured, or levels. If someone raises the threshold far enough to
        // make depth a real memory term, this fails and says so.
        let terms = current_day_ram_terms_at_ceiling();
        let depth = terms
            .iter()
            .find(|(n, _)| *n == "depth_ilp_buffer")
            .map(|(_, b)| *b)
            .expect("depth term present");
        let total: u64 = terms.iter().map(|(_, b)| *b).sum();
        assert!(
            depth * 20 < total,
            "the depth ILP buffer is {depth} B of a {total} B current-day footprint \
             — no longer the rounding error the budget assumed. Depth RAM is \
             DEPTH_FLUSH_ROW_THRESHOLD-bounded; raising that constant trades drain \
             occupancy for memory and both sides must be re-argued."
        );
    }

    #[test]
    fn the_depth_flush_threshold_bounds_drain_occupancy_not_just_payload() {
        // Reusing the tick threshold (1,000) would force 10–50 synchronous
        // HTTP round trips per second on the task that also folds ticks,
        // because depth emits 20–200 rows per packet where ticks emit one.
        // The constant exists to break that coupling; this pins the ratio.
        assert!(
            crate::dhan_feed_stack::DEPTH_FLUSH_ROW_THRESHOLD
                >= crate::dhan_feed_stack::FLUSH_ROW_THRESHOLD * 10,
            "depth must flush at least 10x less often per row than ticks, or the \
             drain spends its time blocked in ILP instead of folding ticks"
        );
    }

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
        // The reader MUST target the renamed REST table (2026-08-14). Asserting
        // the literal, not the constant, is deliberate: a test that formats the
        // same constant the code formats would pass through any rename and
        // prove nothing.
        assert!(sql.contains("FROM rest_option_chain_1m"), "{sql}");
        assert!(sql.contains("feed = 'dhan'"));
        assert!(sql.contains("underlying_symbol = 'NIFTY'"));
        assert!(sql.contains("ts >= 2000 AND ts < 3000"));
        // PR-2 round-1: the query fetches LIMIT+1 — one extra row as the
        // truncation tripwire, so an exact-boundary window is not flagged.
        assert!(sql.contains("ORDER BY ts ASC LIMIT 30001"));
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
        // Truncation tripwire (PR-2 round-1, LIMIT+1 semantics): an
        // EXACT-boundary dataset (len == limit) is legitimately complete —
        // only len > limit (the +1 fetch row came back) proves truncation.
        let (_, exact_boundary) = parse_chain_rehydrate_rows(body, 2).expect("parses");
        assert!(
            !exact_boundary,
            "dataset.len() == limit is a COMPLETE exact-boundary window"
        );
        let (_, truncated2) = parse_chain_rehydrate_rows(body, 1).expect("parses");
        assert!(truncated2, "dataset.len() > limit must flag truncated");
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
