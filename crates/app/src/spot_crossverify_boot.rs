//! Post-close Dhan↔Groww `spot_1m_rest` cross-broker comparator
//! (SPOT-XVERIFY-01/02).
//!
//! Revives the cross-broker OHLC parity signal lost when
//! `cross_verify_1m_boot.rs` was deleted with the Dhan live WS. At 15:47 IST
//! each trading day (in the 15:45↔15:50 gap, after the ~15:33:30 spot
//! post-session sweep has settled the source table) a supervised comparator
//! reads OUR stored `spot_1m_rest` rows `feed='dhan'` vs `feed='groww'` for
//! the day's `IDX_I` indices, joins them by CANONICAL INDEX NAME
//! (never security_id — Dhan pins 13/25/51/21, Groww stamps its own id
//! space), and compares Open/High/Low/Close minute-by-minute as INTEGER
//! PAISE. NEITHER feed is ground truth — a divergence-TREND signal only.
//!
//! Mirrors `tf_consistency_boot.rs` (supervised daily post-close `/exec`
//! reader + Telegram summary) + `brutex_crossverify_compare.rs` (pure
//! cross-source paise-integer OHLC compare). Cold path, best-effort — never
//! on the tick hot path, never on any recovery / REST-capture path.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::NaiveDate;
use tracing::{error, info, warn};

use tickvault_common::config::{ApplicationConfig, QuestDbConfig};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::instrument::index_extractor::canonicalize_index_symbol;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::spot_crossverify_persistence::{
    SpotXverifyAuditWriter, SpotXverifyCellFinding, SpotXverifyCellKind, SpotXverifyDailyRow,
    SpotXverifyOutcome, ensure_spot_crossverify_tables,
};

// Reuse the tf_consistency pure helpers (same app crate).
use crate::tf_consistency_boot::{ist_day_start_nanos, parse_tf_verify_date, to_paise};

/// Trigger slot: 15:47:00 IST = 15*3600 + 47*60 = 56_820 (the 15:45↔15:50
/// gap, after the spot post-session sweep settles `spot_1m_rest`).
pub const SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST: u32 = 15 * 3600 + 47 * 60;
/// Whole-run wall-clock budget (cold path — a handful of queries).
pub const SPOT_XVERIFY_RUN_BUDGET_SECS: u64 = 600;
const SPOT_XVERIFY_HTTP_TIMEOUT_SECS: u64 = 15;
/// 8 MiB `/exec` response cap (the TF_VERIFY_MAX_RESPONSE_BYTES shape).
pub const SPOT_XVERIFY_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Row LIMIT — ~4 indices × 375 minutes headroom. `returned == limit` = the
/// truncation tripwire (degraded, never a silent partial).
pub const SPOT_XVERIFY_ROW_LIMIT: usize = 1_500;
/// Max cell-audit rows written per run (a pathological day is bounded).
pub const SPOT_XVERIFY_MAX_AUDIT_ROWS_PER_RUN: usize = 10_000;
const SPOT_XVERIFY_TOP_DETAIL_MAX: usize = 10;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// NSE session window (seconds-of-day IST): [09:15, 15:30).
const SESSION_OPEN_SECS_OF_DAY_IST: i64 = 33_300;
const SESSION_CLOSE_SECS_OF_DAY_IST: i64 = 55_800;
const _: () = assert!(SESSION_OPEN_SECS_OF_DAY_IST < SESSION_CLOSE_SECS_OF_DAY_IST);
const _: () = assert!(
    (SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST as i64) > SESSION_CLOSE_SECS_OF_DAY_IST,
    "the comparator must fire AFTER the 15:30 close"
);

/// Reject a non-finite / ≤0 / absurd price cell (the brutex sanity clamp).
const MAX_PRICE_RUPEES: f64 = 10_000_000.0;

/// Once-per-process spawn guard (dual-spawn safe).
static SPOT_XVERIFY_SPAWNED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Pure types + compare core
// ---------------------------------------------------------------------------

/// One minute bar in INTEGER PAISE (comparison key; nothing is written back
/// rounded). No `Eq` — the retained rupee cells are `f64`; paise fields are
/// the integer comparison key, `PartialEq` suffices.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OhlcBar {
    pub open_paise: i64,
    pub high_paise: i64,
    pub low_paise: i64,
    pub close_paise: i64,
    /// Stored volume (IDX_I is 0) — NEVER classified.
    pub volume: i64,
    /// Rupee-price cells retained for the audit rows.
    pub open_r: f64,
    pub high_r: f64,
    pub low_r: f64,
    pub close_r: f64,
}

/// Parse a rupee OHLC row into an integer-paise bar; `None` when any price
/// is non-finite / ≤0 / above the sanity cap.
#[must_use]
pub fn parse_bar(open: f64, high: f64, low: f64, close: f64, volume: i64) -> Option<OhlcBar> {
    for v in [open, high, low, close] {
        if !v.is_finite() || v <= 0.0 || v > MAX_PRICE_RUPEES {
            return None;
        }
    }
    Some(OhlcBar {
        open_paise: to_paise(open),
        high_paise: to_paise(high),
        low_paise: to_paise(low),
        close_paise: to_paise(close),
        volume,
        open_r: open,
        high_r: high,
        low_r: low,
        close_r: close,
    })
}

/// One cross-broker cell finding (pure).
#[derive(Clone, Debug, PartialEq)]
pub struct CellFinding {
    pub index_name: String,
    pub minute_nanos: i64,
    pub kind: SpotXverifyCellKind,
    pub field: &'static str,
    pub dhan_value: f64,
    pub groww_value: f64,
    pub dhan_volume: i64,
    pub groww_volume: i64,
    pub diff_paise: i64,
}

/// The pure comparison result.
#[derive(Clone, Debug, PartialEq)]
pub struct DayComparison {
    pub outcome: SpotXverifyOutcome,
    pub findings: Vec<CellFinding>,
    pub indices: u64,
    pub minutes_compared: u64,
    pub cells_diverged: u64,
    pub missing_dhan: u64,
    pub missing_groww: u64,
    pub out_of_session: u64,
    /// Divergence-magnitude noise stats (paise) — QUANTIFY the normal
    /// fluctuation so the operator SEES the baseline.
    pub noise_p50: i64,
    pub noise_p95: i64,
    pub noise_max: i64,
}

fn secs_of_day(minute_nanos: i64, day_start_nanos: i64) -> i64 {
    (minute_nanos - day_start_nanos) / NANOS_PER_SEC
}

fn in_session(secs: i64) -> bool {
    (SESSION_OPEN_SECS_OF_DAY_IST..SESSION_CLOSE_SECS_OF_DAY_IST).contains(&secs)
}

fn cmp_field(
    field: &'static str,
    d_paise: i64,
    g_paise: i64,
    d_r: f64,
    g_r: f64,
    tolerance_paise: i64,
    index_name: &str,
    minute_nanos: i64,
    d_vol: i64,
    g_vol: i64,
) -> Option<(CellFinding, i64)> {
    let diff = (d_paise - g_paise).abs();
    if diff > tolerance_paise {
        Some((
            CellFinding {
                index_name: index_name.to_string(),
                minute_nanos,
                kind: SpotXverifyCellKind::Diverged,
                field,
                dhan_value: d_r,
                groww_value: g_r,
                dhan_volume: d_vol,
                groww_volume: g_vol,
                diff_paise: diff,
            },
            diff,
        ))
    } else {
        None
    }
}

/// Compare one trading day of `spot_1m_rest` rows, cross-feed, by canonical
/// index name. `tolerance_paise` default 0 = paise-exact. Volume is stored
/// on findings but NEVER a divergence field. Pure.
#[must_use]
pub fn compare_day(
    dhan: &BTreeMap<(String, i64), OhlcBar>,
    groww: &BTreeMap<(String, i64), OhlcBar>,
    day_start_nanos: i64,
    tolerance_paise: i64,
) -> DayComparison {
    let mut findings: Vec<CellFinding> = Vec::new();
    let mut minutes_compared = 0u64;
    let mut cells_diverged = 0u64;
    let mut missing_dhan = 0u64;
    let mut missing_groww = 0u64;
    let mut out_of_session = 0u64;
    let mut diffs: Vec<i64> = Vec::new();
    let mut indices_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    let mut keys: std::collections::BTreeSet<&(String, i64)> = std::collections::BTreeSet::new();
    keys.extend(dhan.keys());
    keys.extend(groww.keys());

    for key in keys {
        let (index_name, minute_nanos) = (&key.0, key.1);
        indices_seen.insert(index_name.clone());
        let d = dhan.get(key);
        let g = groww.get(key);
        if !in_session(secs_of_day(minute_nanos, day_start_nanos)) {
            out_of_session = out_of_session.saturating_add(1);
            findings.push(CellFinding {
                index_name: index_name.clone(),
                minute_nanos,
                kind: SpotXverifyCellKind::OutOfSession,
                field: "none",
                dhan_value: d.map(|b| b.close_r).unwrap_or(0.0),
                groww_value: g.map(|b| b.close_r).unwrap_or(0.0),
                dhan_volume: d.map(|b| b.volume).unwrap_or(0),
                groww_volume: g.map(|b| b.volume).unwrap_or(0),
                diff_paise: 0,
            });
            continue;
        }
        match (d, g) {
            (Some(d), Some(g)) => {
                minutes_compared = minutes_compared.saturating_add(1);
                for (field, dp, gp, dr, gr) in [
                    ("open", d.open_paise, g.open_paise, d.open_r, g.open_r),
                    ("high", d.high_paise, g.high_paise, d.high_r, g.high_r),
                    ("low", d.low_paise, g.low_paise, d.low_r, g.low_r),
                    ("close", d.close_paise, g.close_paise, d.close_r, g.close_r),
                ] {
                    if let Some((finding, diff)) = cmp_field(
                        field,
                        dp,
                        gp,
                        dr,
                        gr,
                        tolerance_paise,
                        index_name,
                        minute_nanos,
                        d.volume,
                        g.volume,
                    ) {
                        cells_diverged = cells_diverged.saturating_add(1);
                        diffs.push(diff);
                        findings.push(finding);
                    }
                }
            }
            (Some(d), None) => {
                missing_groww = missing_groww.saturating_add(1);
                findings.push(CellFinding {
                    index_name: index_name.clone(),
                    minute_nanos,
                    kind: SpotXverifyCellKind::MissingGroww,
                    field: "none",
                    dhan_value: d.close_r,
                    groww_value: 0.0,
                    dhan_volume: d.volume,
                    groww_volume: 0,
                    diff_paise: 0,
                });
            }
            (None, Some(g)) => {
                missing_dhan = missing_dhan.saturating_add(1);
                findings.push(CellFinding {
                    index_name: index_name.clone(),
                    minute_nanos,
                    kind: SpotXverifyCellKind::MissingDhan,
                    field: "none",
                    dhan_value: 0.0,
                    groww_value: g.close_r,
                    dhan_volume: 0,
                    groww_volume: g.volume,
                    diff_paise: 0,
                });
            }
            (None, None) => {}
        }
    }

    diffs.sort_unstable();
    let pct = |p: f64| -> i64 {
        if diffs.is_empty() {
            return 0;
        }
        // APPROVED: cold-path percentile index — truncation impossible in range
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let idx = ((diffs.len() as f64 - 1.0) * p).round() as usize;
        diffs[idx.min(diffs.len() - 1)]
    };
    let noise_p50 = pct(0.50);
    let noise_p95 = pct(0.95);
    let noise_max = diffs.last().copied().unwrap_or(0);

    let rows_seen = !dhan.is_empty() || !groww.is_empty();
    let outcome = if minutes_compared > 0 {
        if cells_diverged > 0 || missing_dhan > 0 || missing_groww > 0 {
            SpotXverifyOutcome::Diverged
        } else {
            SpotXverifyOutcome::Clean
        }
    } else if missing_dhan > 0 && missing_groww > 0 {
        // Both feeds contributed distinct IN-SESSION minutes with zero overlap:
        // a genuine divergence, never a silent Blind (each feed is missing the
        // other's minutes). One-sided in-session coverage (only one of the two
        // > 0) stays Blind — we could not compare.
        SpotXverifyOutcome::Diverged
    } else if rows_seen {
        SpotXverifyOutcome::Blind
    } else {
        SpotXverifyOutcome::NoData
    };

    DayComparison {
        outcome,
        findings,
        indices: indices_seen.len() as u64,
        minutes_compared,
        cells_diverged,
        missing_dhan,
        missing_groww,
        out_of_session,
        noise_p50,
        noise_p95,
        noise_max,
    }
}

// ---------------------------------------------------------------------------
// Scheduling (pure)
// ---------------------------------------------------------------------------

/// When the daily comparator should fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotXverifyStart {
    /// Not a trading day and not forced → do not run.
    SkipNonTradingDay,
    /// Forced (`TICKVAULT_SPOT_XVERIFY_NOW`) or a past-trigger trading-day
    /// boot → run once, immediately (reruns are DEDUP-idempotent).
    RunNow,
    /// Sleep this many seconds, then run at 15:47:00 IST.
    SleepThenRun(u64),
}

/// Pure decision: when should the comparator fire.
#[must_use]
pub fn decide_spot_xverify_start(
    now_secs_of_day_ist: u32,
    is_trading_day: bool,
    force_now: bool,
) -> SpotXverifyStart {
    if force_now {
        return SpotXverifyStart::RunNow;
    }
    if !is_trading_day {
        return SpotXverifyStart::SkipNonTradingDay;
    }
    if now_secs_of_day_ist >= SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST {
        return SpotXverifyStart::RunNow;
    }
    SpotXverifyStart::SleepThenRun(u64::from(
        SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST - now_secs_of_day_ist,
    ))
}

/// A forced run refusal reason, or `None` if the forced run may proceed. A
/// NOW without a DATE is refused on a non-trading day, and — for any run
/// targeting today — on a trading day BEFORE the 15:47 trigger (the source
/// table is not yet settled). A NOW + DATE for a PAST trading day is allowed.
#[must_use]
pub fn spot_xverify_forced_refusal(
    has_date_override: bool,
    date_is_today: bool,
    is_trading_day: bool,
    now_secs_of_day_ist: u32,
) -> Option<&'static str> {
    let targets_today = !has_date_override || date_is_today;
    if !targets_today {
        return None;
    }
    if !is_trading_day {
        return Some("today is not a trading day");
    }
    if now_secs_of_day_ist < SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST {
        return Some("before 3:47 PM IST — today's spot rows are not settled yet");
    }
    None
}

/// The DETERMINISTIC audit run timestamp for a target day — that day's
/// 15:47:00 IST regardless of when the run fired, so reruns UPSERT. Pure.
#[must_use]
pub fn deterministic_run_ts_nanos(day_start_ist_nanos: i64) -> i64 {
    day_start_ist_nanos.saturating_add(
        i64::from(SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST).saturating_mul(NANOS_PER_SEC),
    )
}

/// `no_data` is log-only (never a page); everything else (or a forced run)
/// notifies. Pure.
#[must_use]
pub fn should_notify_summary(status_label: &str, forced_run: bool) -> bool {
    forced_run || status_label != "no_data"
}

// ---------------------------------------------------------------------------
// /exec reads
// ---------------------------------------------------------------------------

fn day_bounds_micros(day_start_ist_nanos: i64) -> (i64, i64) {
    let start = day_start_ist_nanos / 1_000;
    (start, start.saturating_add(SECS_PER_DAY * MICROS_PER_SEC))
}

/// Per-feed `spot_1m_rest` IDX_I day query — micros WHERE window,
/// nanos-projected key, feed-scoped, ordered, explicitly LIMIT-bounded. Pure.
#[must_use]
pub fn select_spot_1m_sql(feed: &str, day_start_ist_nanos: i64) -> String {
    let (start, end) = day_bounds_micros(day_start_ist_nanos);
    format!(
        "SELECT (ts / 1) * 1000 AS ts_nanos, symbol, open, high, low, close, volume \
         FROM spot_1m_rest \
         WHERE feed = '{feed}' AND exchange_segment = 'IDX_I' \
         AND ts >= {start} AND ts < {end} ORDER BY ts ASC LIMIT {SPOT_XVERIFY_ROW_LIMIT}"
    )
}

/// A raw parsed source row.
#[derive(Clone, Debug, PartialEq)]
pub struct SpotRow {
    pub ts_nanos: i64,
    pub symbol: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

fn row_i64(cols: &[serde_json::Value], idx: usize) -> Option<i64> {
    cols.get(idx).and_then(|c| {
        c.as_i64().or_else(|| {
            // APPROVED: cold-path float-to-i64 column fallback precedent
            #[allow(clippy::cast_possible_truncation)]
            c.as_f64().map(|f| f as i64)
        })
    })
}

/// Parse the per-feed `spot_1m_rest` dataset. `truncated` = the dataset hit
/// the query's LIMIT (the tripwire — never a silent partial compare).
///
/// # Errors
/// Malformed body (not JSON / no dataset). Individual bad rows are skipped.
pub fn parse_spot_dataset(body: &str, limit: usize) -> Result<(Vec<SpotRow>, bool), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("malformed JSON: {e}"))?;
    let rows = v
        .get("dataset")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "missing dataset".to_string())?;
    let truncated = rows.len() >= limit;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let Some(cols) = r.as_array() else { continue };
        let (Some(ts_nanos), Some(symbol)) = (
            row_i64(cols, 0),
            cols.get(1).and_then(|c| c.as_str()).map(str::to_string),
        ) else {
            continue;
        };
        let (Some(open), Some(high), Some(low), Some(close)) = (
            cols.get(2).and_then(serde_json::Value::as_f64),
            cols.get(3).and_then(serde_json::Value::as_f64),
            cols.get(4).and_then(serde_json::Value::as_f64),
            cols.get(5).and_then(serde_json::Value::as_f64),
        ) else {
            continue;
        };
        out.push(SpotRow {
            ts_nanos,
            symbol,
            open,
            high,
            low,
            close,
            volume: row_i64(cols, 6).unwrap_or(0),
        });
    }
    Ok((out, truncated))
}

/// Canonicalize a spot-index symbol into the cross-feed join key: strip a
/// leading `NSE-` / `BSE-` exchange prefix (case-insensitive), THEN
/// [`canonicalize_index_symbol`] (§31.1 / §37.4 — strip the prefix, THEN
/// canonicalize). The Groww spot leg stores the underlying as the
/// `groww_symbol` (`NSE-NIFTY` / `BSE-SENSEX`) while the Dhan spot leg stores
/// the bare canonical form (`NIFTY` / `SENSEX`); the shared
/// `canonicalize_index_symbol` alone does NOT strip the exchange prefix, so
/// without this bridge the two feeds' rows never join and every day reads
/// Blind.
#[must_use]
fn canonicalize_spot_index_symbol(symbol: &str) -> String {
    let upper = symbol.trim().to_ascii_uppercase();
    let bare = upper
        .strip_prefix("NSE-")
        .or_else(|| upper.strip_prefix("BSE-"))
        .map_or(upper.as_str(), str::trim);
    canonicalize_index_symbol(bare)
}

/// Build the canonical (index, minute) → bar map from parsed rows (last wins
/// on a duplicate key; bad-price rows skipped).
#[must_use]
pub fn rows_to_bar_map(rows: &[SpotRow]) -> BTreeMap<(String, i64), OhlcBar> {
    let mut map = BTreeMap::new();
    for r in rows {
        let canonical = canonicalize_spot_index_symbol(&r.symbol);
        if let Some(bar) = parse_bar(r.open, r.high, r.low, r.close, r.volume) {
            map.insert((canonical, r.ts_nanos), bar);
        }
    }
    map
}

async fn http_get_text(
    client: &reqwest::Client,
    url: &str,
    query: &str,
) -> Result<String, (String, &'static str)> {
    let resp = client
        .get(url)
        .query(&[("query", query)])
        .send()
        .await
        .map_err(|e| (format!("send: {e}"), "questdb_unreachable"))?;
    if !resp.status().is_success() {
        return Err((format!("http {}", resp.status()), "query_failed"));
    }
    if let Some(declared) = resp.content_length()
        && declared > SPOT_XVERIFY_MAX_RESPONSE_BYTES as u64
    {
        return Err((
            format!("declared content-length {declared} bytes exceeds cap"),
            "oversize",
        ));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| (format!("read: {e}"), "query_failed"))?;
    if body.len() > SPOT_XVERIFY_MAX_RESPONSE_BYTES {
        return Err((
            format!("response body {} bytes exceeds cap", body.len()),
            "oversize",
        ));
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
// The daily run
// ---------------------------------------------------------------------------

/// Data returned by one comparator run (for the Telegram summary).
#[derive(Clone, Debug, PartialEq)]
pub struct SpotXverifySummaryData {
    pub trading_date_ist: String,
    pub indices: u64,
    pub minutes_compared: u64,
    pub mismatches: u64,
    pub missing_dhan: u64,
    pub missing_groww: u64,
    pub out_of_session: u64,
    pub degraded: bool,
    pub truncated: bool,
    pub status_label: String,
    pub top_detail: Vec<String>,
}

async fn read_feed_map(
    client: &reqwest::Client,
    exec_url: &str,
    feed: &str,
    day_start_nanos: i64,
    degraded: &mut bool,
    truncated: &mut bool,
) -> BTreeMap<(String, i64), OhlcBar> {
    let sql = select_spot_1m_sql(feed, day_start_nanos);
    match http_get_text(client, exec_url, &sql).await {
        Ok(body) => match parse_spot_dataset(&body, SPOT_XVERIFY_ROW_LIMIT) {
            Ok((rows, trunc)) => {
                if trunc {
                    *truncated = true;
                    *degraded = true;
                    metrics::counter!("tv_spot_xverify_query_failures_total", "stage" => "truncated")
                        .increment(1);
                    error!(
                        code = ErrorCode::SpotXverify02RunDegraded.code_str(),
                        stage = "truncated",
                        %feed,
                        "SPOT-XVERIFY-02: {feed} spot query hit the row LIMIT — partial compare not trusted"
                    );
                }
                rows_to_bar_map(&rows)
            }
            Err(msg) => {
                *degraded = true;
                metrics::counter!("tv_spot_xverify_query_failures_total", "stage" => "query_failed")
                    .increment(1);
                error!(
                    code = ErrorCode::SpotXverify02RunDegraded.code_str(),
                    stage = "query_failed",
                    %feed,
                    %msg,
                    "SPOT-XVERIFY-02: {feed} spot dataset parse failed"
                );
                BTreeMap::new()
            }
        },
        Err((msg, stage)) => {
            *degraded = true;
            metrics::counter!("tv_spot_xverify_query_failures_total", "stage" => stage)
                .increment(1);
            error!(
                code = ErrorCode::SpotXverify02RunDegraded.code_str(),
                stage,
                %feed,
                %msg,
                "SPOT-XVERIFY-02: {feed} spot query failed"
            );
            BTreeMap::new()
        }
    }
}

/// Run the comparator for one trading day. Best-effort — ensures tables,
/// reads both feeds, compares, writes findings + the daily row, emits the
/// coded logs + counters, and returns the summary for the Telegram card.
// TEST-EXEMPT: live-deps async orchestrator (compare/parse/SQL/schedule decisions are pure fns unit-tested below); wiring pinned by crates/app/tests/spot_crossverify_wiring_guard.rs — the run_tf_consistency precedent.
pub async fn run_spot_crossverify(
    questdb_config: &QuestDbConfig,
    date: NaiveDate,
) -> SpotXverifySummaryData {
    ensure_spot_crossverify_tables(questdb_config).await;

    let date_label = date.format("%Y-%m-%d").to_string();
    let day_start = ist_day_start_nanos(date);
    let run_ts = deterministic_run_ts_nanos(day_start);
    let exec_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            SPOT_XVERIFY_HTTP_TIMEOUT_SECS,
        ))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            metrics::counter!("tv_spot_xverify_query_failures_total", "stage" => "client_build")
                .increment(1);
            error!(
                code = ErrorCode::SpotXverify02RunDegraded.code_str(),
                stage = "client_build",
                ?err,
                "SPOT-XVERIFY-02: HTTP client build failed — comparison skipped (blind)"
            );
            metrics::counter!("tv_spot_xverify_runs_total", "status" => "blind").increment(1);
            return SpotXverifySummaryData {
                trading_date_ist: date_label,
                indices: 0,
                minutes_compared: 0,
                mismatches: 0,
                missing_dhan: 0,
                missing_groww: 0,
                out_of_session: 0,
                degraded: true,
                truncated: false,
                status_label: "blind".to_string(),
                top_detail: Vec::new(),
            };
        }
    };

    let mut degraded = false;
    let mut truncated = false;
    let dhan = read_feed_map(
        &client,
        &exec_url,
        "dhan",
        day_start,
        &mut degraded,
        &mut truncated,
    )
    .await;
    let groww = read_feed_map(
        &client,
        &exec_url,
        "groww",
        day_start,
        &mut degraded,
        &mut truncated,
    )
    .await;

    let tolerance = questdb_config_tolerance();
    let cmp = compare_day(&dhan, &groww, day_start, tolerance);

    // Final outcome: a degraded leg with a partial compare reads Partial; a
    // degraded leg with nothing compared reads Degraded; else the pure
    // verdict (Clean/Diverged/Blind/NoData).
    let outcome = if degraded {
        if cmp.minutes_compared > 0 {
            SpotXverifyOutcome::Partial
        } else if cmp.outcome == SpotXverifyOutcome::NoData {
            // A read failed AND both maps empty → we vouch for nothing.
            SpotXverifyOutcome::Degraded
        } else {
            cmp.outcome
        }
    } else {
        cmp.outcome
    };

    let mismatches = cmp.cells_diverged;
    let mut top_detail: Vec<String> = Vec::new();
    for f in cmp.findings.iter().take(SPOT_XVERIFY_TOP_DETAIL_MAX) {
        top_detail.push(format!(
            "{} {} {}: dhan={:.2} groww={:.2} ({}p)",
            f.index_name,
            f.kind.as_str(),
            f.field,
            f.dhan_value,
            f.groww_value,
            f.diff_paise
        ));
    }

    // Persist cell findings (bounded) + the daily row.
    let mut writer = SpotXverifyAuditWriter::new(questdb_config);
    for f in cmp
        .findings
        .iter()
        .take(SPOT_XVERIFY_MAX_AUDIT_ROWS_PER_RUN)
    {
        metrics::counter!("tv_spot_xverify_findings_total", "kind" => f.kind.as_str()).increment(1);
        if let Err(err) = writer.append_cell(&SpotXverifyCellFinding {
            run_ts_ist_nanos: run_ts,
            trading_date_ist_nanos: day_start,
            index_name: f.index_name.clone(),
            exchange_segment: "IDX_I".to_string(),
            minute_ts_ist_nanos: f.minute_nanos,
            kind: f.kind,
            field: f.field,
            dhan_value: f.dhan_value,
            groww_value: f.groww_value,
            dhan_volume: f.dhan_volume,
            groww_volume: f.groww_volume,
            diff_paise: f.diff_paise,
        }) {
            warn!(?err, "spot_crossverify: cell append failed (skipped)");
        }
    }
    if let Err(err) = writer.append_daily(&SpotXverifyDailyRow {
        run_ts_ist_nanos: run_ts,
        trading_date_ist_nanos: day_start,
        indices: cmp.indices as i64,
        minutes_compared: cmp.minutes_compared as i64,
        mismatches: mismatches as i64,
        missing_dhan: cmp.missing_dhan as i64,
        missing_groww: cmp.missing_groww as i64,
        out_of_session: cmp.out_of_session as i64,
        outcome,
    }) {
        warn!(?err, "spot_crossverify: daily append failed (skipped)");
    }
    if let Err(err) = writer.flush() {
        degraded = true;
        metrics::counter!("tv_spot_xverify_query_failures_total", "stage" => "flush").increment(1);
        error!(
            code = ErrorCode::SpotXverify02RunDegraded.code_str(),
            stage = "flush",
            ?err,
            "SPOT-XVERIFY-02: audit flush failed — pending rows discarded (rerun DEDUP-idempotent)"
        );
    }

    let paging = mismatches + cmp.missing_dhan + cmp.missing_groww;
    if paging > 0 {
        // ONE coalesced emission per run (never per row).
        error!(
            code = ErrorCode::SpotXverify01MismatchFound.code_str(),
            trading_date_ist = %date_label,
            mismatches,
            missing_dhan = cmp.missing_dhan,
            missing_groww = cmp.missing_groww,
            noise_p50 = cmp.noise_p50,
            noise_p95 = cmp.noise_p95,
            noise_max = cmp.noise_max,
            "SPOT-XVERIFY-01: Dhan↔Groww spot OHLC divergence(s) found — track the trend"
        );
    }

    let status_label = outcome.as_str().to_string();
    metrics::counter!("tv_spot_xverify_runs_total", "status" => outcome.as_str()).increment(1);
    info!(
        %date_label,
        status = %status_label,
        minutes_compared = cmp.minutes_compared,
        mismatches,
        "PROOF: spot_crossverify comparator fired @ 15:47:00 IST"
    );

    SpotXverifySummaryData {
        trading_date_ist: date_label,
        indices: cmp.indices,
        minutes_compared: cmp.minutes_compared,
        mismatches,
        missing_dhan: cmp.missing_dhan,
        missing_groww: cmp.missing_groww,
        out_of_session: cmp.out_of_session,
        degraded,
        truncated,
        status_label,
        top_detail,
    }
}

/// Read the configured tolerance (paise). Env override for tests / ops; the
/// config field is the source of truth — kept as a helper so `run_*` stays
/// arg-light. Defaults to 0 (paise-exact).
fn questdb_config_tolerance() -> i64 {
    std::env::var("TICKVAULT_SPOT_XVERIFY_TOLERANCE_PAISE")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v >= 0)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Supervised dual-spawn
// ---------------------------------------------------------------------------

/// Spawn the supervised daily comparator. Config-gated (default OFF); the
/// once-per-process guard is dual-spawn safe. Env overrides:
/// `TICKVAULT_SPOT_XVERIFY_NOW=1` (+ `TICKVAULT_SPOT_XVERIFY_DATE=YYYY-MM-DD`)
/// force / backfill a run.
pub fn spawn_spot_crossverify_tasks(
    config: &ApplicationConfig,
    trading_calendar: &std::sync::Arc<TradingCalendar>,
    notifier: &std::sync::Arc<NotificationService>,
) {
    if !config.spot_crossverify.enabled {
        info!("spot_crossverify: disabled by [spot_crossverify] config — nothing spawned");
        return;
    }
    if SPOT_XVERIFY_SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    let qcfg = config.questdb.clone();
    let calendar = std::sync::Arc::clone(trading_calendar);
    let inner: tokio::task::JoinHandle<Result<Option<(SpotXverifySummaryData, bool)>, String>> =
        tokio::spawn(async move {
            use chrono::{FixedOffset, TimeZone, Timelike, Utc};
            use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
            let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                return Err("IST offset construction failed".to_string());
            };
            let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
            let today_ist = now_ist.date_naive();
            let now_secs_of_day = now_ist.time().num_seconds_from_midnight();
            let force_now = std::env::var("TICKVAULT_SPOT_XVERIFY_NOW")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let date_override: Option<NaiveDate> = if force_now {
                match std::env::var("TICKVAULT_SPOT_XVERIFY_DATE") {
                    Ok(raw) => match parse_tf_verify_date(&raw) {
                        Some(d) if d <= today_ist && calendar.is_trading_day(d) => Some(d),
                        Some(d) => {
                            return Err(format!(
                                "TICKVAULT_SPOT_XVERIFY_DATE {d} must be a past-or-today \
                                 trading day — refusing the forced backfill"
                            ));
                        }
                        None => {
                            return Err(format!(
                                "invalid TICKVAULT_SPOT_XVERIFY_DATE {raw:?} — expected YYYY-MM-DD"
                            ));
                        }
                    },
                    Err(_) => None,
                }
            } else {
                if std::env::var("TICKVAULT_SPOT_XVERIFY_DATE").is_ok() {
                    warn!(
                        "spot_crossverify: TICKVAULT_SPOT_XVERIFY_DATE is ignored without \
                         TICKVAULT_SPOT_XVERIFY_NOW=1 — set both to backfill a past trading day"
                    );
                }
                None
            };
            let is_trading_day = calendar.is_trading_day(today_ist);
            match decide_spot_xverify_start(now_secs_of_day, is_trading_day, force_now) {
                SpotXverifyStart::SkipNonTradingDay => {
                    info!("spot_crossverify: skipping (non-trading day)");
                    return Ok(None);
                }
                SpotXverifyStart::RunNow => {
                    if force_now
                        && let Some(reason) = spot_xverify_forced_refusal(
                            date_override.is_some(),
                            date_override == Some(today_ist),
                            is_trading_day,
                            now_secs_of_day,
                        )
                    {
                        info!(
                            %reason,
                            "spot_crossverify: forced run refused (pass \
                             TICKVAULT_SPOT_XVERIFY_DATE=YYYY-MM-DD for a PAST trading day, \
                             or re-run after 3:47 PM IST)"
                        );
                        return Ok(None);
                    }
                    info!(
                        backfill_date = date_override
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| "today".to_string()),
                        "spot_crossverify: running now (scheduled catch-up / forced backfill)"
                    );
                }
                SpotXverifyStart::SleepThenRun(secs_until) => {
                    info!(secs_until, "spot_crossverify: sleeping until 15:47:00 IST");
                    tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;
                }
            }
            let target = date_override.unwrap_or(today_ist);
            let summary = run_spot_crossverify(&qcfg, target).await;
            Ok(Some((summary, force_now)))
        });

    let sx_notifier = std::sync::Arc::clone(notifier);
    tokio::spawn(async move {
        match inner.await {
            Ok(Ok(Some((s, forced_run)))) => {
                let status_label = s.status_label.clone();
                if should_notify_summary(&status_label, forced_run) {
                    sx_notifier.notify(NotificationEvent::SpotCrossverifySummary {
                        trading_date_ist: s.trading_date_ist,
                        indices: s.indices,
                        minutes_compared: s.minutes_compared,
                        mismatches: s.mismatches,
                        missing_dhan: s.missing_dhan,
                        missing_groww: s.missing_groww,
                        out_of_session: s.out_of_session,
                        degraded: s.degraded,
                        truncated: s.truncated,
                        status_label: s.status_label,
                        top_detail: s.top_detail,
                    });
                } else {
                    warn!(
                        status = %status_label,
                        trading_date = %s.trading_date_ist,
                        "spot_crossverify: nothing to compare today (both feeds empty) — \
                         Telegram summary suppressed (log-only; a divergence/degraded/blind \
                         day still notifies)"
                    );
                }
            }
            Ok(Ok(None)) => {} // non-trading skip / refused force — no page.
            Ok(Err(reason)) => {
                error!(
                    code = ErrorCode::SpotXverify02RunDegraded.code_str(),
                    stage = "daily_run",
                    %reason,
                    "SPOT-XVERIFY-02: the daily spot cross-verify run failed"
                );
                sx_notifier.notify(NotificationEvent::SpotCrossverifyAborted { detail: reason });
            }
            Err(join_err) if join_err.is_panic() => {
                error!(
                    code = ErrorCode::SpotXverify02RunDegraded.code_str(),
                    stage = "daily_panic",
                    %join_err,
                    "SPOT-XVERIFY-02: the daily spot cross-verify task crashed"
                );
                sx_notifier.notify(NotificationEvent::SpotCrossverifyAborted {
                    detail: format!("the spot cross-verify task crashed: {join_err}"),
                });
            }
            Err(_) => {
                info!("spot_crossverify: task cancelled during shutdown");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    const DAY0: i64 = 0; // day_start; minute nanos are secs*1e9 relative to it.

    fn min_nanos(secs_of_day: i64) -> i64 {
        DAY0 + secs_of_day * NANOS_PER_SEC
    }
    fn bar(o: f64, h: f64, l: f64, c: f64) -> OhlcBar {
        parse_bar(o, h, l, c, 0).unwrap()
    }
    fn nifty(secs: i64) -> (String, i64) {
        ("NIFTY".to_string(), min_nanos(secs))
    }

    #[test]
    fn paise_exact_match_is_clean() {
        let mut d = BTreeMap::new();
        let mut g = BTreeMap::new();
        d.insert(nifty(33_360), bar(100.0, 101.0, 99.0, 100.5));
        g.insert(nifty(33_360), bar(100.0, 101.0, 99.0, 100.5));
        let cmp = compare_day(&d, &g, DAY0, 0);
        assert_eq!(cmp.outcome, SpotXverifyOutcome::Clean);
        assert_eq!(cmp.minutes_compared, 1);
        assert_eq!(cmp.cells_diverged, 0);
        assert!(cmp.findings.is_empty());
    }

    #[test]
    fn one_paise_diff_is_one_diverged_finding() {
        let mut d = BTreeMap::new();
        let mut g = BTreeMap::new();
        d.insert(nifty(33_360), bar(100.00, 101.00, 99.00, 100.50));
        g.insert(nifty(33_360), bar(100.00, 101.01, 99.00, 100.50));
        let cmp = compare_day(&d, &g, DAY0, 0);
        assert_eq!(cmp.outcome, SpotXverifyOutcome::Diverged);
        assert_eq!(cmp.cells_diverged, 1);
        assert_eq!(cmp.findings.len(), 1);
        assert_eq!(cmp.findings[0].field, "high");
        assert_eq!(cmp.findings[0].diff_paise, 1);
        assert_eq!(cmp.noise_max, 1);
    }

    #[test]
    fn tolerance_absorbs_a_one_paise_diff() {
        let mut d = BTreeMap::new();
        let mut g = BTreeMap::new();
        d.insert(nifty(33_360), bar(100.00, 101.00, 99.00, 100.50));
        g.insert(nifty(33_360), bar(100.00, 101.01, 99.00, 100.50));
        let cmp = compare_day(&d, &g, DAY0, 1);
        assert_eq!(cmp.outcome, SpotXverifyOutcome::Clean);
        assert_eq!(cmp.cells_diverged, 0);
    }

    #[test]
    fn missing_in_each_feed_is_named() {
        let mut d = BTreeMap::new();
        let mut g = BTreeMap::new();
        d.insert(nifty(33_360), bar(100.0, 101.0, 99.0, 100.5)); // dhan only
        g.insert(nifty(33_420), bar(100.0, 101.0, 99.0, 100.5)); // groww only
        let cmp = compare_day(&d, &g, DAY0, 0);
        assert_eq!(cmp.missing_groww, 1);
        assert_eq!(cmp.missing_dhan, 1);
        assert_eq!(cmp.minutes_compared, 0);
        // Both feeds had rows but zero overlap ⇒ never a silent clean.
        assert_eq!(cmp.outcome, SpotXverifyOutcome::Diverged);
    }

    #[test]
    fn out_of_session_is_filtered_not_compared() {
        let mut d = BTreeMap::new();
        let mut g = BTreeMap::new();
        // 09:00 IST = 32_400 secs < session open 33_300.
        d.insert(nifty(32_400), bar(100.0, 101.0, 99.0, 100.5));
        g.insert(nifty(32_400), bar(100.0, 199.0, 99.0, 100.5));
        let cmp = compare_day(&d, &g, DAY0, 0);
        assert_eq!(cmp.out_of_session, 1);
        assert_eq!(cmp.minutes_compared, 0);
        assert_eq!(cmp.cells_diverged, 0);
        assert_eq!(cmp.outcome, SpotXverifyOutcome::Blind);
    }

    #[test]
    fn blind_when_one_side_has_rows_and_zero_overlap() {
        let mut d = BTreeMap::new();
        let g: BTreeMap<(String, i64), OhlcBar> = BTreeMap::new();
        d.insert(nifty(33_360), bar(100.0, 101.0, 99.0, 100.5));
        let cmp = compare_day(&d, &g, DAY0, 0);
        // dhan-only present minute ⇒ missing_groww, minutes_compared 0.
        assert_eq!(cmp.missing_groww, 1);
        assert_eq!(cmp.minutes_compared, 0);
        assert_eq!(cmp.outcome, SpotXverifyOutcome::Blind);
    }

    #[test]
    fn no_data_when_both_empty() {
        let d: BTreeMap<(String, i64), OhlcBar> = BTreeMap::new();
        let g: BTreeMap<(String, i64), OhlcBar> = BTreeMap::new();
        let cmp = compare_day(&d, &g, DAY0, 0);
        assert_eq!(cmp.outcome, SpotXverifyOutcome::NoData);
        assert_eq!(cmp.minutes_compared, 0);
        assert!(cmp.findings.is_empty());
    }

    #[test]
    fn volume_is_stored_not_classified() {
        let mut d = BTreeMap::new();
        let mut g = BTreeMap::new();
        let mut db = bar(100.0, 101.0, 99.0, 100.5);
        db.volume = 10;
        let mut gb = bar(100.0, 101.0, 99.0, 100.5);
        gb.volume = 999;
        d.insert(nifty(33_360), db);
        g.insert(nifty(33_360), gb);
        let cmp = compare_day(&d, &g, DAY0, 0);
        assert_eq!(cmp.outcome, SpotXverifyOutcome::Clean);
        assert_eq!(cmp.cells_diverged, 0);
    }

    #[test]
    fn noise_stats_p50_p95_max() {
        let mut d = BTreeMap::new();
        let mut g = BTreeMap::new();
        for (i, diff_r) in [0.01_f64, 0.02, 0.03, 0.10].iter().enumerate() {
            let sec = 33_360 + i as i64 * 60;
            d.insert(nifty(sec), bar(100.0, 101.0, 99.0, 100.5));
            g.insert(nifty(sec), bar(100.0, 101.0 + diff_r, 99.0, 100.5));
        }
        let cmp = compare_day(&d, &g, DAY0, 0);
        assert_eq!(cmp.cells_diverged, 4);
        assert_eq!(cmp.noise_max, 10);
        assert!(cmp.noise_p50 >= 1 && cmp.noise_p50 <= 10);
        assert!(cmp.noise_p95 >= cmp.noise_p50);
    }

    #[test]
    fn parse_bar_rejects_bad_prices() {
        assert!(parse_bar(-1.0, 1.0, 1.0, 1.0, 0).is_none());
        assert!(parse_bar(0.0, 1.0, 1.0, 1.0, 0).is_none());
        assert!(parse_bar(f64::NAN, 1.0, 1.0, 1.0, 0).is_none());
        assert!(parse_bar(1.0, 1.0, 1.0, 1.0, 0).is_some());
    }

    #[test]
    fn decide_start_boundaries() {
        assert_eq!(
            decide_spot_xverify_start(0, false, true),
            SpotXverifyStart::RunNow
        );
        assert_eq!(
            decide_spot_xverify_start(0, false, false),
            SpotXverifyStart::SkipNonTradingDay
        );
        assert_eq!(
            decide_spot_xverify_start(SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST, true, false),
            SpotXverifyStart::RunNow
        );
        assert_eq!(
            decide_spot_xverify_start(SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST - 100, true, false),
            SpotXverifyStart::SleepThenRun(100)
        );
    }

    #[test]
    fn forced_refusal_before_trigger_and_non_trading() {
        // bare NOW on a trading day before trigger ⇒ refused.
        assert!(spot_xverify_forced_refusal(false, false, true, 100).is_some());
        // bare NOW on a non-trading day ⇒ refused.
        assert!(spot_xverify_forced_refusal(false, false, false, 60_000).is_some());
        // NOW + DATE for a past day (not today) ⇒ allowed.
        assert!(spot_xverify_forced_refusal(true, false, true, 100).is_none());
        // NOW after the trigger on a trading day ⇒ allowed.
        assert!(
            spot_xverify_forced_refusal(
                false,
                false,
                true,
                SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST + 10
            )
            .is_none()
        );
    }

    #[test]
    fn should_notify_suppresses_only_scheduled_no_data() {
        assert!(!should_notify_summary("no_data", false));
        assert!(should_notify_summary("no_data", true));
        assert!(should_notify_summary("clean", false));
        assert!(should_notify_summary("blind", false));
    }

    #[test]
    fn select_sql_is_feed_and_segment_scoped_and_limited() {
        let sql = select_spot_1m_sql("dhan", 0);
        assert!(sql.contains("feed = 'dhan'"));
        assert!(sql.contains("exchange_segment = 'IDX_I'"));
        assert!(sql.contains("FROM spot_1m_rest"));
        assert!(sql.contains(&format!("LIMIT {SPOT_XVERIFY_ROW_LIMIT}")));
    }

    #[test]
    fn parse_dataset_truncation_tripwire() {
        let body = r#"{"dataset":[[1,"NIFTY",1.0,2.0,0.5,1.5,0]]}"#;
        let (rows, trunc) = parse_spot_dataset(body, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(trunc); // returned == limit
        let (_, trunc2) = parse_spot_dataset(body, 10).unwrap();
        assert!(!trunc2);
        assert!(parse_spot_dataset("not json", 10).is_err());
    }

    #[test]
    fn canonicalize_joins_cross_feed_index_names() {
        // The Groww spot leg stores the `groww_symbol` (`NSE-NIFTY`); the Dhan
        // spot leg stores the bare canonical `NIFTY` (SPOT_1M_REST_INDICES).
        // Both must land on the SAME join key — strip the exchange prefix,
        // THEN canonicalize (§31.1 / §37.4). Bare `canonicalize_index_symbol`
        // does NOT strip the prefix, which is the bug this bridges.
        let groww = canonicalize_spot_index_symbol("NSE-NIFTY");
        let dhan = canonicalize_spot_index_symbol("NIFTY");
        assert_eq!(
            groww, dhan,
            "cross-feed NIFTY forms must join: {groww} vs {dhan}"
        );
        assert_eq!(groww, "NIFTY");
        // SENSEX crosses the BSE- prefix; BANKNIFTY the NSE- prefix.
        assert_eq!(
            canonicalize_spot_index_symbol("BSE-SENSEX"),
            canonicalize_spot_index_symbol("SENSEX"),
        );
        assert_eq!(
            canonicalize_spot_index_symbol("NSE-BANKNIFTY"),
            canonicalize_spot_index_symbol("BANKNIFTY"),
        );
        // The raw shared fn (no strip) would NOT join these — proving the
        // bridge is load-bearing.
        assert_ne!(
            canonicalize_index_symbol("NSE-NIFTY"),
            canonicalize_index_symbol("NIFTY"),
        );
    }

    #[test]
    fn test_compare_day_mixed_scenario_counts_each_category_exactly_once() {
        let mut d = BTreeMap::new();
        let mut g = BTreeMap::new();
        // Minute 1: clean on both feeds.
        d.insert(nifty(33_360), bar(100.0, 101.0, 99.0, 100.5));
        g.insert(nifty(33_360), bar(100.0, 101.0, 99.0, 100.5));
        // Minute 2: low diverges by 2 paise.
        d.insert(nifty(33_420), bar(100.0, 101.0, 99.00, 100.5));
        g.insert(nifty(33_420), bar(100.0, 101.0, 99.02, 100.5));
        // Minute 3: dhan-only (missing on groww).
        d.insert(nifty(33_480), bar(100.0, 101.0, 99.0, 100.5));
        // Minute 4: out-of-session (pre-open) on both.
        d.insert(nifty(32_400), bar(100.0, 101.0, 99.0, 100.5));
        g.insert(nifty(32_400), bar(100.0, 199.0, 99.0, 100.5));
        let cmp = compare_day(&d, &g, DAY0, 0);
        assert_eq!(cmp.outcome, SpotXverifyOutcome::Diverged);
        assert_eq!(cmp.indices, 1);
        assert_eq!(cmp.minutes_compared, 2); // clean + diverged minutes
        assert_eq!(cmp.cells_diverged, 1);
        assert_eq!(cmp.missing_groww, 1);
        assert_eq!(cmp.missing_dhan, 0);
        assert_eq!(cmp.out_of_session, 1);
        assert_eq!(cmp.findings.len(), 3); // diverged + missing + out-of-session
        assert_eq!(cmp.noise_max, 2);
        // Pure: same inputs ⇒ same result.
        assert_eq!(cmp, compare_day(&d, &g, DAY0, 0));
    }

    #[test]
    fn test_decide_spot_xverify_start_sleep_is_exact_seconds_until_trigger() {
        // One second before the 15:47:00 IST trigger ⇒ sleep exactly 1s.
        assert_eq!(
            decide_spot_xverify_start(SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST - 1, true, false),
            SpotXverifyStart::SleepThenRun(1)
        );
        // Midnight on a trading day ⇒ sleep the full trigger offset.
        assert_eq!(
            decide_spot_xverify_start(0, true, false),
            SpotXverifyStart::SleepThenRun(u64::from(SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST))
        );
        // Force wins even before the trigger on a trading day.
        assert_eq!(
            decide_spot_xverify_start(100, true, true),
            SpotXverifyStart::RunNow
        );
        // Past the trigger on a NON-trading day (not forced) ⇒ still skipped.
        assert_eq!(
            decide_spot_xverify_start(SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST + 1, false, false),
            SpotXverifyStart::SkipNonTradingDay
        );
    }

    #[test]
    fn test_spot_xverify_forced_refusal_date_today_behaves_like_bare_now() {
        // NOW + DATE=today targets today ⇒ the pre-trigger refusal applies.
        assert!(spot_xverify_forced_refusal(true, true, true, 100).is_some());
        // NOW + DATE=today on a non-trading day ⇒ refused with a reason.
        assert!(spot_xverify_forced_refusal(true, true, false, 60_000).is_some());
        // AT the trigger second exactly ⇒ allowed (refusal is strictly <).
        assert!(
            spot_xverify_forced_refusal(true, true, true, SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST)
                .is_none()
        );
        // A PAST-day backfill never consults today's clock or calendar.
        assert!(spot_xverify_forced_refusal(true, false, false, 0).is_none());
    }

    #[test]
    fn test_select_spot_1m_sql_window_is_micros_of_the_ist_day() {
        // day_start = 1 day in nanos ⇒ WHERE window in MICROS, [start, +24h).
        let day_start_nanos = 86_400i64 * 1_000_000_000;
        let sql = select_spot_1m_sql("groww", day_start_nanos);
        assert!(sql.contains("ts >= 86400000000"), "start micros: {sql}");
        assert!(sql.contains("ts < 172800000000"), "end micros: {sql}");
        assert!(sql.contains("feed = 'groww'"));
        // Micros→nanos projection so the join key is nanos like the bar maps.
        assert!(sql.contains("(ts / 1) * 1000 AS ts_nanos"));
        assert!(sql.contains("ORDER BY ts ASC"));
    }

    #[test]
    fn test_parse_spot_dataset_skips_bad_rows_and_errors_on_missing_dataset() {
        // One good row + one row with a non-numeric price + one non-array row:
        // only the good row survives; no truncation below the limit.
        let body = r#"{"dataset":[
            [60000000000,"NIFTY",100.0,101.0,99.0,100.5,7],
            [60060000000,"NIFTY","bad",101.0,99.0,100.5,0],
            {"not":"a row"}
        ]}"#;
        let (rows, trunc) = parse_spot_dataset(body, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!trunc);
        assert_eq!(rows[0].symbol, "NIFTY");
        assert_eq!(rows[0].ts_nanos, 60_000_000_000);
        assert_eq!(rows[0].volume, 7);
        // A JSON body without a dataset is a typed error, never a panic.
        assert!(parse_spot_dataset(r#"{"columns":[]}"#, 10).is_err());
    }

    #[test]
    fn rows_to_bar_map_canonicalizes_and_skips_bad_prices() {
        let rows = vec![
            SpotRow {
                ts_nanos: 100,
                symbol: "NSE-NIFTY".to_string(),
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.5,
                volume: 0,
            },
            SpotRow {
                ts_nanos: 200,
                symbol: "NSE-NIFTY".to_string(),
                open: -1.0, // bad → skipped
                high: 101.0,
                low: 99.0,
                close: 100.5,
                volume: 0,
            },
        ];
        let map = rows_to_bar_map(&rows);
        assert_eq!(map.len(), 1);
    }
}
