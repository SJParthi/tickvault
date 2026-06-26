//! Post-market 1-minute cross-verification (operator directive 2026-06-02).
//!
//! At **15:31 IST** each trading day, for every subscribed **spot** instrument
//! (the daily-universe SIDs — indices + F&O underlyings), fetch Dhan's
//! authoritative intraday 1-minute candles (`POST /v2/charts/intraday`,
//! interval `"1"`) and compare OHLCV **timestamp-by-timestamp, EXACT match**
//! against our live `candles_1m`. Mismatches → `cross_verify_1m_audit` table +
//! `data/cross-verify/cross-verify-1m-YYYY-MM-DD.csv` + Telegram count.
//!
//! Operator: *"csv and exact match cross verification is needed."*
//!
//! **NOT the deleted 90-day `cross_verify.rs` chain** — narrowed: 1-minute
//! only, spot only, today only, post-market only, cold path, fail-soft. See
//! `live-feed-purity.md` rule 11. No synthesized ticks into `ticks`.
//!
//! ## Timestamps
//! Dhan intraday `timestamp[]` is UTC epoch seconds → `+IST_UTC_OFFSET` for the
//! IST minute bucket (per `data-integrity.md`). Our `candles_1m.ts` is already
//! IST nanoseconds. Both are keyed in IST nanoseconds, so the join is exact.

use std::collections::HashMap;
use std::time::Duration;

use chrono::NaiveDate;
use secrecy::ExposeSecret;
use serde_json::json;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::sanitize::{capture_rest_error_body, redact_url_params};
use tickvault_common::url_join::join_api_url;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_storage::feed_parity_1m_audit_persistence::{
    FeedParity1mAuditWriter, FeedParity1mMismatch, MismatchField, csv_header,
    ensure_feed_parity_1m_audit_table, mismatch_to_csv_line,
};

/// Dhan Data-API rate limit: 5 requests/second.
const DATA_API_RPS: u32 = 5;
/// Per-request REST timeout.
const REST_TIMEOUT_SECS: u64 = 15;
/// Dhan intraday endpoint (base v2 URL from constants).
const INTRADAY_PATH: &str = "/charts/intraday";
/// Max spins on the Data-API rate gate before giving up this symbol's turn.
const RATE_GATE_MAX_SPINS: u32 = 50;
/// Backoff between rate-gate spins.
const RATE_GATE_BACKOFF_MS: u64 = 50;
/// Directory for the per-day mismatch CSV (operator: "easily accessible").
const CROSS_VERIFY_CSV_DIR: &str = "data/cross-verify";
/// If the Dhan intraday fetch errors for more than this FRACTION of spot SIDs,
/// the run is flagged degraded (CROSS-VERIFY-1M-02) so the operator knows the
/// day's verification could not vouch for the full universe (false-OK guard).
const FETCH_DEGRADED_FAIL_FRACTION: f64 = 0.10;
/// Seconds per IST trading minute bucket (1m candle).
const SECONDS_PER_MINUTE: i64 = 60;
/// Nanoseconds per second (IST-epoch → nanos).
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// IST seconds-of-day for the post-market cross-verify trigger (15:31:00).
const CROSS_VERIFY_TRIGGER_SECS_OF_DAY_IST: u32 = 15 * 3600 + 31 * 60; // 55_860

/// Decision for WHEN the post-market 1-minute cross-verify should fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossVerifyStart {
    /// Not a trading day and not forced → do not run.
    SkipNonTradingDay,
    /// Past 15:31 IST on a normal (non-forced) boot → mid-evening boot, skip.
    SkipPastTrigger,
    /// Run immediately — operator forced an on-demand run.
    RunNow,
    /// Sleep this many seconds, then run at 15:31:00 IST.
    SleepThenRun(u64),
}

/// Pure decision: given the current IST seconds-of-day, whether today is a
/// trading day, and whether the operator forced an on-demand run, decide when
/// the cross-verify should fire.
///
/// `force_now` (set by the `TICKVAULT_CROSS_VERIFY_NOW` env var via
/// `make cross-verify-now`) overrides BOTH the trading-day gate and the 15:31
/// schedule so an operator can prove the pipeline end-to-end on demand without
/// waiting for 15:31 IST on a live trading day. The run itself remains
/// fail-soft on empty/partial data, so a forced run on a quiet day simply
/// produces an empty/degraded report rather than fabricating anything.
pub fn decide_cross_verify_start(
    now_secs_of_day_ist: u32,
    is_trading_day: bool,
    force_now: bool,
) -> CrossVerifyStart {
    if force_now {
        return CrossVerifyStart::RunNow;
    }
    if !is_trading_day {
        return CrossVerifyStart::SkipNonTradingDay;
    }
    if now_secs_of_day_ist >= CROSS_VERIFY_TRIGGER_SECS_OF_DAY_IST {
        return CrossVerifyStart::SkipPastTrigger;
    }
    CrossVerifyStart::SleepThenRun(u64::from(
        CROSS_VERIFY_TRIGGER_SECS_OF_DAY_IST - now_secs_of_day_ist,
    ))
}

#[cfg(test)]
mod start_decision_tests {
    use super::{
        CROSS_VERIFY_TRIGGER_SECS_OF_DAY_IST, CrossVerifyStart, decide_cross_verify_start,
    };

    #[test]
    fn test_decide_cross_verify_start_trigger_constant_is_1531_ist() {
        assert_eq!(CROSS_VERIFY_TRIGGER_SECS_OF_DAY_IST, 55_860);
    }

    #[test]
    fn test_decide_cross_verify_start_trading_day_before_1531_sleeps() {
        // 15:30:00 IST → sleep 60s to 15:31:00.
        let now = 15 * 3600 + 30 * 60;
        assert_eq!(
            decide_cross_verify_start(now, true, false),
            CrossVerifyStart::SleepThenRun(60)
        );
    }

    #[test]
    fn test_decide_cross_verify_start_at_exact_trigger_skips_past() {
        let now = CROSS_VERIFY_TRIGGER_SECS_OF_DAY_IST;
        assert_eq!(
            decide_cross_verify_start(now, true, false),
            CrossVerifyStart::SkipPastTrigger
        );
    }

    #[test]
    fn test_decide_cross_verify_start_after_1531_skips_past() {
        let now = 16 * 3600;
        assert_eq!(
            decide_cross_verify_start(now, true, false),
            CrossVerifyStart::SkipPastTrigger
        );
    }

    #[test]
    fn test_decide_cross_verify_start_non_trading_day_skips() {
        let now = 10 * 3600;
        assert_eq!(
            decide_cross_verify_start(now, false, false),
            CrossVerifyStart::SkipNonTradingDay
        );
    }

    #[test]
    fn test_decide_cross_verify_start_force_overrides_non_trading_day() {
        assert_eq!(
            decide_cross_verify_start(10 * 3600, false, true),
            CrossVerifyStart::RunNow
        );
    }

    #[test]
    fn test_decide_cross_verify_start_force_overrides_past_trigger() {
        assert_eq!(
            decide_cross_verify_start(20 * 3600, true, true),
            CrossVerifyStart::RunNow
        );
    }

    #[test]
    fn test_decide_cross_verify_start_force_overrides_before_trigger() {
        assert_eq!(
            decide_cross_verify_start(9 * 3600, true, true),
            CrossVerifyStart::RunNow
        );
    }
}

/// One 1-minute candle, keyed by its IST-minute bucket. `volume` is `i64`
/// (exact integer compare). Prices are `f64` (our `candles_1m` and Dhan REST
/// are both f64 — exact compare per the operator's "exact match").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MinuteCandle {
    /// IST-minute bucket start, in nanoseconds (the join key).
    pub minute_ts_ist_nanos: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

/// Outcome counts for one instrument's compare (and aggregated for the day).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompareStats {
    /// Minutes present in BOTH our candles and Dhan's (the comparable set).
    pub compared: usize,
    /// Field-cells that disagreed (each O/H/L/C/V counted separately).
    pub mismatches: usize,
    /// Minutes Dhan has that our `candles_1m` is MISSING (missed live data).
    pub missing_ours: usize,
}

impl CompareStats {
    /// Fold another instrument's stats into this aggregate. Pure, saturating.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            compared: self.compared.saturating_add(other.compared),
            mismatches: self.mismatches.saturating_add(other.mismatches),
            missing_ours: self.missing_ours.saturating_add(other.missing_ours),
        }
    }
}

/// EXACT timestamp-keyed OHLCV diff for ONE instrument. Pure — O(N) with an
/// O(1) HashMap join on the IST-minute bucket. For every minute Dhan reports:
/// if we have the same minute, compare all 5 fields exactly and emit one
/// `FeedParity1mMismatch` per differing field; otherwise count it as
/// `missing_ours`. Minutes we have but Dhan does NOT are ignored (Dhan's tape
/// is authoritative — we only flag where Dhan disagrees or we are missing).
#[must_use]
pub fn diff_minute_candles(
    security_id: i64,
    segment: &str,
    symbol: &str,
    run_ts_ist_nanos: i64,
    trading_date_ist_nanos: i64,
    ours: &[MinuteCandle],
    dhan: &[MinuteCandle],
) -> (Vec<FeedParity1mMismatch>, CompareStats) {
    let mut by_minute: HashMap<i64, MinuteCandle> = HashMap::with_capacity(ours.len());
    for c in ours {
        by_minute.insert(c.minute_ts_ist_nanos, *c);
    }
    let mut out = Vec::new();
    let mut stats = CompareStats::default();
    for d in dhan {
        let Some(o) = by_minute.get(&d.minute_ts_ist_nanos) else {
            stats.missing_ours = stats.missing_ours.saturating_add(1);
            continue;
        };
        stats.compared = stats.compared.saturating_add(1);
        let mut push = |field: MismatchField, live_value: f64, backtest_value: f64| {
            // Cold path (post-market, once/day) — owning the segment/symbol
            // Strings per cell is irrelevant to performance.
            out.push(FeedParity1mMismatch {
                // Dhan parity (Dhan live vs Dhan REST tape = the backtest). Routes
                // to the unified feed_parity_1m_audit table (SP5); `feed` is in the
                // DEDUP key so Dhan + Groww rows for the same cell both persist.
                feed: tickvault_common::feed::Feed::Dhan.as_str(),
                run_ts_ist_nanos,
                trading_date_ist_nanos,
                security_id,
                segment: segment.to_string(),
                symbol: symbol.to_string(),
                minute_ts_ist_nanos: d.minute_ts_ist_nanos,
                field,
                live_value,
                backtest_value,
            });
            stats.mismatches = stats.mismatches.saturating_add(1);
        };
        if o.open != d.open {
            push(MismatchField::Open, o.open, d.open);
        }
        if o.high != d.high {
            push(MismatchField::High, o.high, d.high);
        }
        if o.low != d.low {
            push(MismatchField::Low, o.low, d.low);
        }
        if o.close != d.close {
            push(MismatchField::Close, o.close, d.close);
        }
        if o.volume != d.volume {
            // Compare as exact integers; widen to f64 only for the audit cell.
            push(MismatchField::Volume, o.volume as f64, d.volume as f64);
        }
    }
    (out, stats)
}

/// Build the `/v2/charts/intraday` request body for ONE spot symbol, interval
/// `"1"`, for a single trading day. `to_date` is the next calendar day so the
/// whole `from_date` session is captured (intraday uses datetime strings).
/// Pure.
#[must_use]
pub fn intraday_request_body(
    security_id: &str,
    exchange_segment: &str,
    instrument: &str,
    from_date: NaiveDate,
    to_date: NaiveDate,
) -> serde_json::Value {
    json!({
        "securityId": security_id,
        "exchangeSegment": exchange_segment,
        "instrument": instrument,
        "interval": "1",
        "oi": false,
        "fromDate": from_date.format("%Y-%m-%d 00:00:00").to_string(),
        "toDate": to_date.format("%Y-%m-%d 00:00:00").to_string(),
    })
}

/// Convert a Dhan intraday UTC-epoch-second timestamp into the IST-minute
/// bucket nanoseconds our `candles_1m` is keyed by. `data-integrity.md`:
/// historical/intraday REST timestamps are UTC epoch seconds → `+IST_UTC_OFFSET`
/// then floor to the minute, then ×1e9. O(1), saturating.
#[must_use]
pub fn intraday_utc_secs_to_ist_minute_nanos(utc_epoch_secs: i64) -> i64 {
    let ist_secs = utc_epoch_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let minute_floor = ist_secs - ist_secs.rem_euclid(SECONDS_PER_MINUTE);
    minute_floor.saturating_mul(NANOS_PER_SEC)
}

/// Parse Dhan's columnar intraday response into `MinuteCandle`s. Parallel
/// arrays `open/high/low/close/volume/timestamp`; all must be the same
/// non-zero length. Returns empty on malformed/empty. Pure — never panics.
#[must_use]
pub fn parse_intraday_1m_candles(body: &str) -> Vec<MinuteCandle> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let arr = |k: &str| v.get(k).and_then(|x| x.as_array());
    let (Some(open), Some(high), Some(low), Some(close), Some(vol), Some(ts)) = (
        arr("open"),
        arr("high"),
        arr("low"),
        arr("close"),
        arr("volume"),
        arr("timestamp"),
    ) else {
        return Vec::new();
    };
    let n = ts.len();
    if n == 0
        || open.len() != n
        || high.len() != n
        || low.len() != n
        || close.len() != n
        || vol.len() != n
    {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let (Some(o), Some(h), Some(l), Some(c), Some(t)) = (
            open[i].as_f64(),
            high[i].as_f64(),
            low[i].as_f64(),
            close[i].as_f64(),
            ts[i].as_i64(),
        ) else {
            continue;
        };
        let volume = vol[i]
            .as_i64()
            .or_else(|| vol[i].as_f64().map(|f| f as i64))
            .unwrap_or(0);
        out.push(MinuteCandle {
            minute_ts_ist_nanos: intraday_utc_secs_to_ist_minute_nanos(t),
            open: o,
            high: h,
            low: l,
            close: c,
            volume,
        });
    }
    out
}

/// Parse our `candles_1m` QuestDB `/exec` dataset into `MinuteCandle`s for ONE
/// instrument. Expected row order: `[ts_nanos, open, high, low, close, volume]`
/// (the SELECT projects exactly these). `ts` is already IST nanoseconds — used
/// directly (NEVER add the IST offset to a WebSocket-sourced ts). Pure.
#[must_use]
pub fn parse_our_candles_dataset(body: &str) -> Vec<MinuteCandle> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(rows) = v.get("dataset").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(cols) = row.as_array() else { continue };
        if cols.len() < 6 {
            continue;
        }
        let (Some(ts), Some(o), Some(h), Some(l), Some(c)) = (
            cols[0].as_i64(),
            cols[1].as_f64(),
            cols[2].as_f64(),
            cols[3].as_f64(),
            cols[4].as_f64(),
        ) else {
            continue;
        };
        let volume = cols[5]
            .as_i64()
            .or_else(|| cols[5].as_f64().map(|f| f as i64))
            .unwrap_or(0);
        out.push(MinuteCandle {
            minute_ts_ist_nanos: ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume,
        });
    }
    out
}

/// Build the QuestDB SELECT for ONE instrument's `candles_1m` rows on the
/// trading day. Pure (testable). `trading_date` is the IST date; the day window
/// is `[date 00:00, date+1 00:00)` in IST nanoseconds (QuestDB stores `ts` in
/// IST nanos for live candles).
///
/// **Feed-scoped (operator 2026-06-19, "same tables + feed column"):** since the
/// `candles_1m` table is now shared by Dhan and Groww (distinguished by the
/// `feed` column), this cross-verify — which compares OUR live candles against
/// Dhan's REST intraday — MUST filter `feed = 'dhan'`. Without it, once Groww is
/// enabled the SELECT would return two rows per minute (one per feed) and the
/// minute-by-minute exact compare would see phantom mismatches / double minutes.
#[must_use]
pub fn our_candles_select_sql(security_id: i64, segment: &str, day_start_ist_nanos: i64) -> String {
    let day_end = day_start_ist_nanos.saturating_add(86_400 * NANOS_PER_SEC);
    let feed = tickvault_storage::shadow_candle_writer::CANDLE_FEED_DHAN;
    format!(
        "SELECT (ts / 1) AS ts_nanos, open, high, low, close, volume \
         FROM candles_1m \
         WHERE security_id = {security_id} AND segment = '{segment}' AND feed = '{feed}' \
         AND ts >= {day_start_ist_nanos} AND ts < {day_end} ORDER BY ts ASC"
    )
}

/// Aggregated outcome of a cross-verify run (returned to the boot wiring for
/// the typed Telegram event).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrossVerify1mSummary {
    pub instruments_checked: usize,
    pub fetch_failures: usize,
    pub stats: CompareStats,
    pub degraded: bool,
}

/// False-clean classification of a cross-verify run (DHAN-REST-400 task
/// item 2, audit Rule 11). A 776/776-fetch-failure day previously produced a
/// header-only CSV indistinguishable from a perfect day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    /// Zero minute-cells compared — the run vouches for NOTHING. An empty
    /// mismatch list under this status is meaningless, not a pass.
    Blind,
    /// Some coverage, but the fetch-failure fraction breached the degraded
    /// threshold — the mismatch count covers only part of the universe.
    Degraded,
    /// Full(-enough) coverage; the mismatch count is trustworthy.
    Pass,
}

impl RunStatus {
    /// Stable wire-format label (CSV status line + Telegram + counter label).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blind => "BLIND",
            Self::Degraded => "DEGRADED",
            Self::Pass => "PASS",
        }
    }
}

/// Classify a run. Pure.
///
/// `compared == 0` is BLIND regardless of why (all fetches failed, or a
/// forced run on a quiet day) — Rule 11: never report clean on an empty
/// compare set. With coverage, the existing degraded flag (fetch-failure
/// fraction > [`FETCH_DEGRADED_FAIL_FRACTION`]) maps to DEGRADED.
#[must_use]
pub fn classify_run_status(summary: &CrossVerify1mSummary) -> RunStatus {
    if summary.stats.compared == 0 {
        return RunStatus::Blind;
    }
    if summary.degraded {
        return RunStatus::Degraded;
    }
    RunStatus::Pass
}

/// The status line written as the FIRST line of the per-day CSV, before the
/// header. `#`-prefixed so the data grid is untouched; one glance at the file
/// now distinguishes "perfect day" from "checked nothing". Pure.
#[must_use]
pub fn csv_status_line(summary: &CrossVerify1mSummary, status: RunStatus) -> String {
    format!(
        "# status={} instruments={} fetch_failures={} compared={} mismatches={} missing_ours={}",
        status.as_str(),
        summary.instruments_checked,
        summary.fetch_failures,
        summary.stats.compared,
        summary.stats.mismatches,
        summary.stats.missing_ours,
    )
}

/// Outcome of the end-of-run audit flush (DHAN-REST-400 task item 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    /// Nothing buffered — flushing was correctly skipped.
    SkippedEmpty,
    /// All pending rows flushed.
    Flushed,
    /// Flush failed with this many rows still pending.
    Failed { pending: usize },
}

/// Final audit flush, skip-when-empty.
///
/// Root cause of the 2026-06-10 "final audit flush failed — rows may be
/// unpersisted" false alarm: `flush()` was called unconditionally, and on a
/// zero-mismatch day (every fetch failed ⇒ zero appends) a disconnected ILP
/// sender still returns `Err("not connected")` — alarming about unpersisted
/// rows when pending = 0. Skip the flush entirely when nothing is buffered;
/// when something IS buffered and the flush fails, report the exact count.
pub fn final_flush(writer: &mut FeedParity1mAuditWriter) -> FlushOutcome {
    if writer.pending() == 0 {
        return FlushOutcome::SkippedEmpty;
    }
    match writer.flush() {
        Ok(()) => FlushOutcome::Flushed,
        Err(err) => {
            let pending = writer.pending();
            error!(
                code = tickvault_common::error_code::ErrorCode::CrossVerify1m01MismatchFound
                    .code_str(),
                pending,
                error_chain = %format!("{err:#}"),
                "cross_verify_1m: final audit flush failed — {pending} mismatch rows unpersisted"
            );
            FlushOutcome::Failed { pending }
        }
    }
}

/// Run the post-market 1-minute cross-verification for every spot instrument in
/// the universe. Cold path, fail-soft per symbol; never blocks. Writes the
/// audit table + CSV; returns the summary for the caller to emit Telegram.
// Orchestrator needs live Dhan REST + QuestDB; the pure helpers
// (diff_minute_candles, intraday_request_body, parse_*; our_candles_select_sql)
// are unit-tested below.
// TEST-EXEMPT: live-deps async orchestrator (pure helpers unit-tested above).
// APPROVED: cold-path boot orchestrator — 8 independent live deps (targets, token, questdb cfg, base url, two date forms, run ts, csv dir); bundling would only move the arity.
#[allow(clippy::too_many_arguments)]
pub async fn run_cross_verify_1m(
    spot_targets: &[CrossVerifyTarget],
    token_handle: TokenHandle,
    questdb_config: QuestDbConfig,
    base_url: String,
    trading_date: NaiveDate,
    day_start_ist_nanos: i64,
    run_ts_ist_nanos: i64,
    csv_dir: &str,
) -> CrossVerify1mSummary {
    ensure_feed_parity_1m_audit_table(&questdb_config).await;

    let jwt = {
        let guard = token_handle.load();
        match guard.as_ref() {
            Some(state) => state.access_token().expose_secret().to_string(),
            None => {
                error!(
                    code = tickvault_common::error_code::ErrorCode::CrossVerify1m02FetchDegraded
                        .code_str(),
                    "cross_verify_1m: no JWT at run time — verification skipped (degraded)"
                );
                return CrossVerify1mSummary {
                    degraded: true,
                    ..Default::default()
                };
            }
        }
    };

    // DHAN-REST-400 item 1b (2026-06-10): join via join_api_url so a
    // trailing-slash base-URL override can never produce `/v2//charts/...`.
    let intraday_url = join_api_url(&base_url, INTRADAY_PATH);
    let questdb_exec_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(REST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(?err, "cross_verify_1m: HTTP client build failed — skipped");
            return CrossVerify1mSummary {
                degraded: true,
                ..Default::default()
            };
        }
    };
    let limiter = tickvault_trading::oms::rate_limiter::OrderRateLimiter::new(DATA_API_RPS);
    let mut writer = FeedParity1mAuditWriter::new(&questdb_config);

    let trading_date_ist_nanos = day_start_ist_nanos;
    let to_date = trading_date.succ_opt().unwrap_or(trading_date);
    let mut summary = CrossVerify1mSummary::default();
    // First captured fetch-failure reason (status + URL + bounded redacted
    // body) — carried into the CROSS-VERIFY-1M-02 log so a degraded day
    // names its cause instead of just counting failures (DHAN-REST-400).
    let mut sample_fetch_failure: Option<String> = None;
    // CSV is built in-memory then written once at the end (one file open).
    let mut csv = String::from(csv_header());
    csv.push('\n');

    for target in spot_targets {
        summary.instruments_checked = summary.instruments_checked.saturating_add(1);

        // Our candles_1m for the day (QuestDB).
        let our_sql =
            our_candles_select_sql(target.security_id, &target.segment, day_start_ist_nanos);
        let ours = match http_get_text(&client, &questdb_exec_url, &[("query", our_sql.as_str())])
            .await
        {
            Ok(body) => parse_our_candles_dataset(&body),
            Err(reason) => {
                warn!(security_id = target.security_id, %reason, "cross_verify_1m: candles_1m query failed (skip)");
                Vec::new()
            }
        };

        // Dhan intraday 1m (rate-gated).
        for _ in 0..RATE_GATE_MAX_SPINS {
            if limiter.check().is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(RATE_GATE_BACKOFF_MS)).await;
        }
        let body = intraday_request_body(
            target.security_id.to_string().as_str(),
            &target.segment,
            target.instrument,
            trading_date,
            to_date,
        );
        let dhan = match dhan_intraday_fetch(&client, &intraday_url, &jwt, &body).await {
            Ok(candles) => candles,
            Err(reason) => {
                summary.fetch_failures = summary.fetch_failures.saturating_add(1);
                if sample_fetch_failure.is_none() {
                    sample_fetch_failure = Some(reason.clone());
                }
                warn!(security_id = target.security_id, %reason, "cross_verify_1m: intraday fetch failed (skip)");
                continue;
            }
        };

        let (mismatches, stats) = diff_minute_candles(
            target.security_id,
            &target.segment,
            &target.symbol,
            run_ts_ist_nanos,
            trading_date_ist_nanos,
            &ours,
            &dhan,
        );
        summary.stats = summary.stats.merge(stats);
        for m in &mismatches {
            if writer.append_mismatch(m).is_err() {
                error!(
                    code = tickvault_common::error_code::ErrorCode::CrossVerify1m01MismatchFound
                        .code_str(),
                    security_id = target.security_id,
                    "cross_verify_1m: audit append failed"
                );
            }
            csv.push_str(&mismatch_to_csv_line(m));
            csv.push('\n');
        }
    }

    // DHAN-REST-400 item 4: skip-when-empty final flush — the 2026-06-10
    // false alarm fired on an empty buffer over a disconnected sender.
    let _ = final_flush(&mut writer);

    // Degraded if too many symbols failed to fetch (false-OK guard).
    if summary.instruments_checked > 0 {
        let fail_frac = summary.fetch_failures as f64 / summary.instruments_checked as f64;
        summary.degraded = fail_frac > FETCH_DEGRADED_FAIL_FRACTION;
    }

    // DHAN-REST-400 item 2 (audit Rule 11): classify the run and write the
    // status as the FIRST line of the CSV so a header-only file can never
    // again read as a perfect day. The per-day summary JSON (#1097) is
    // written alongside for the portal card + API endpoint.
    let status = classify_run_status(&summary);
    let csv_with_status = format!("{}\n{csv}", csv_status_line(&summary, status));
    write_csv_file(csv_dir, trading_date, &csv_with_status).await;
    write_summary_file(csv_dir, trading_date, &summary).await;
    metrics::counter!("tv_cross_verify_1m_runs_total", "status" => status.as_str()).increment(1);

    if summary.stats.mismatches > 0 {
        error!(
            code = tickvault_common::error_code::ErrorCode::CrossVerify1m01MismatchFound.code_str(),
            compared = summary.stats.compared,
            mismatches = summary.stats.mismatches,
            missing = summary.stats.missing_ours,
            "cross_verify_1m: OHLCV mismatches found vs Dhan intraday"
        );
    }
    if summary.degraded || status == RunStatus::Blind {
        // DHAN-REST-400 item 1: the degraded/blind page now NAMES its cause —
        // the first captured failure carries status + final URL + bounded
        // secret-redacted Dhan error body.
        error!(
            code = tickvault_common::error_code::ErrorCode::CrossVerify1m02FetchDegraded.code_str(),
            status = status.as_str(),
            fetch_failures = summary.fetch_failures,
            instruments = summary.instruments_checked,
            compared = summary.stats.compared,
            sample_failure = %sample_fetch_failure.as_deref().unwrap_or("none captured"),
            "cross_verify_1m: intraday fetch degraded — partial or zero coverage"
        );
    }
    info!(
        status = status.as_str(),
        instruments = summary.instruments_checked,
        compared = summary.stats.compared,
        mismatches = summary.stats.mismatches,
        missing = summary.stats.missing_ours,
        fetch_failures = summary.fetch_failures,
        "cross_verify_1m: post-market verification complete"
    );
    summary
}

/// One spot instrument to verify. Owns its runtime strings (built from the
/// daily-universe targets at the boot-wiring site).
#[derive(Clone, Debug)]
pub struct CrossVerifyTarget {
    pub security_id: i64,
    pub segment: String,
    pub symbol: String,
    /// Dhan `instrument` enum string (`"INDEX"` / `"EQUITY"`).
    pub instrument: &'static str,
}

/// One intraday REST round-trip → parsed candles. `Ok(empty)` for an
/// empty/malformed body (no candles), `Err` for transport/HTTP failure.
///
/// DHAN-REST-400 (2026-06-10): the previous error was just `"http 400"` —
/// Dhan's `errorType`/`errorCode`/`errorMessage` body and the final request
/// URL were dropped, leaving a full day of 776/776 failures with zero
/// root-cause signal. The error string now carries the status, the EXACT
/// final URL (token-redacted) and a bounded (≤300 chars) secret-redacted
/// body capture.
async fn dhan_intraday_fetch(
    client: &reqwest::Client,
    url: &str,
    jwt: &str,
    body: &serde_json::Value,
) -> Result<Vec<MinuteCandle>, String> {
    let resp = client
        .post(url)
        .header("access-token", jwt)
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("send: {}", redact_url_params(&e.to_string())))?;
    let status = resp.status();
    if !status.is_success() {
        let error_body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "http {status} url={} body={}",
            redact_url_params(url),
            capture_rest_error_body(&error_body)
        ));
    }
    let text = resp.text().await.map_err(|e| format!("read: {e}"))?;
    Ok(parse_intraday_1m_candles(&text))
}

/// GET helper returning the response text (or an error string). Used for the
/// QuestDB `/exec` SELECT.
async fn http_get_text(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
) -> Result<String, String> {
    let resp = client
        .get(url)
        .query(query)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
    }
    resp.text().await.map_err(|e| format!("read: {e}"))
}

/// Write the day's CSV (header + mismatch lines) to
/// `<csv_dir>/cross-verify-1m-YYYY-MM-DD.csv`. Fail-soft: a write error logs but
/// never blocks (the audit table is the durable record; CSV is the convenience
/// artefact). The path is constructed from a fixed dir + a strict date string,
/// so it cannot traverse outside `csv_dir`.
async fn write_csv_file(csv_dir: &str, trading_date: NaiveDate, contents: &str) {
    if let Err(err) = tokio::fs::create_dir_all(csv_dir).await {
        warn!(?err, csv_dir, "cross_verify_1m: could not create CSV dir");
        return;
    }
    let file_name = format!("cross-verify-1m-{}.csv", trading_date.format("%Y-%m-%d"));
    let path = std::path::Path::new(csv_dir).join(file_name);
    if let Err(err) = tokio::fs::write(&path, contents).await {
        error!(?err, path = %path.display(), "cross_verify_1m: CSV write failed");
    } else {
        info!(path = %path.display(), "cross_verify_1m: mismatch CSV written");
    }
}

/// Build the per-day machine-readable summary JSON written next to the
/// mismatch CSV (`cross-verify-1m-YYYY-MM-DD.summary.json`). Consumed by
/// `GET /api/debug/cross-verify/latest` and the operator-portal
/// Cross-verify card (visibility directive 2026-06-10). Pure.
#[must_use]
pub fn summary_json_contents(trading_date: NaiveDate, summary: &CrossVerify1mSummary) -> String {
    json!({
        "trading_date": trading_date.format("%Y-%m-%d").to_string(),
        "instruments_checked": summary.instruments_checked,
        "compared": summary.stats.compared,
        "mismatches": summary.stats.mismatches,
        "missing_ours": summary.stats.missing_ours,
        "fetch_failures": summary.fetch_failures,
        "degraded": summary.degraded,
    })
    .to_string()
}

/// Write the day's summary JSON to
/// `<csv_dir>/cross-verify-1m-YYYY-MM-DD.summary.json`. Fail-soft like the
/// CSV: a write error logs but never blocks (the audit table + the Telegram
/// event built from the in-memory summary remain the durable record). The
/// path is a fixed dir + strict date string — cannot traverse.
async fn write_summary_file(
    csv_dir: &str,
    trading_date: NaiveDate,
    summary: &CrossVerify1mSummary,
) {
    if let Err(err) = tokio::fs::create_dir_all(csv_dir).await {
        warn!(
            ?err,
            csv_dir, "cross_verify_1m: could not create summary dir"
        );
        return;
    }
    let file_name = format!(
        "cross-verify-1m-{}.summary.json",
        trading_date.format("%Y-%m-%d")
    );
    let path = std::path::Path::new(csv_dir).join(file_name);
    let contents = summary_json_contents(trading_date, summary);
    if let Err(err) = tokio::fs::write(&path, contents).await {
        error!(?err, path = %path.display(), "cross_verify_1m: summary JSON write failed");
    } else {
        info!(path = %path.display(), "cross_verify_1m: summary JSON written");
    }
}

/// Default CSV directory accessor (boot wiring passes this).
#[must_use]
pub const fn default_csv_dir() -> &'static str {
    CROSS_VERIFY_CSV_DIR
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mc(minute_ts: i64, o: f64, h: f64, l: f64, c: f64, v: i64) -> MinuteCandle {
        MinuteCandle {
            minute_ts_ist_nanos: minute_ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    #[test]
    fn diff_minute_candles_no_mismatch_when_identical() {
        let ours = vec![mc(60 * NANOS_PER_SEC, 100.0, 101.0, 99.0, 100.5, 10)];
        let dhan = ours.clone();
        let (out, stats) = diff_minute_candles(13, "IDX_I", "NIFTY", 1, 1, &ours, &dhan);
        assert!(out.is_empty());
        assert_eq!(stats.compared, 1);
        assert_eq!(stats.mismatches, 0);
        assert_eq!(stats.missing_ours, 0);
    }

    #[test]
    fn diff_minute_candles_flags_each_differing_field() {
        let ts = 60 * NANOS_PER_SEC;
        let ours = vec![mc(ts, 100.0, 101.0, 99.0, 100.5, 10)];
        // open + close + volume differ; high + low match.
        let dhan = vec![mc(ts, 100.25, 101.0, 99.0, 100.75, 12)];
        let (out, stats) = diff_minute_candles(13, "IDX_I", "NIFTY", 1, 1, &ours, &dhan);
        assert_eq!(stats.compared, 1);
        assert_eq!(stats.mismatches, 3, "open + close + volume");
        let fields: Vec<_> = out.iter().map(|m| m.field).collect();
        assert!(fields.contains(&MismatchField::Open));
        assert!(fields.contains(&MismatchField::Close));
        assert!(fields.contains(&MismatchField::Volume));
        assert!(!fields.contains(&MismatchField::High));
    }

    #[test]
    fn diff_minute_candles_counts_missing_when_we_lack_a_minute() {
        let ours: Vec<MinuteCandle> = Vec::new();
        let dhan = vec![mc(60 * NANOS_PER_SEC, 100.0, 101.0, 99.0, 100.5, 10)];
        let (out, stats) = diff_minute_candles(13, "IDX_I", "NIFTY", 1, 1, &ours, &dhan);
        assert!(
            out.is_empty(),
            "no field mismatch — the whole minute is missing"
        );
        assert_eq!(stats.compared, 0);
        assert_eq!(stats.missing_ours, 1);
    }

    #[test]
    fn diff_minute_candles_ignores_minutes_we_have_but_dhan_lacks() {
        // Dhan is authoritative — extra minutes on our side are not flagged.
        let ours = vec![
            mc(60 * NANOS_PER_SEC, 1.0, 1.0, 1.0, 1.0, 1),
            mc(120 * NANOS_PER_SEC, 2.0, 2.0, 2.0, 2.0, 2),
        ];
        let dhan = vec![mc(60 * NANOS_PER_SEC, 1.0, 1.0, 1.0, 1.0, 1)];
        let (out, stats) = diff_minute_candles(13, "IDX_I", "NIFTY", 1, 1, &ours, &dhan);
        assert!(out.is_empty());
        assert_eq!(stats.compared, 1);
        assert_eq!(stats.missing_ours, 0);
    }

    #[test]
    fn diff_minute_candles_carries_identity_into_mismatch() {
        let ts = 60 * NANOS_PER_SEC;
        let ours = vec![mc(ts, 100.0, 101.0, 99.0, 100.5, 10)];
        let dhan = vec![mc(ts, 200.0, 101.0, 99.0, 100.5, 10)];
        let (out, _) = diff_minute_candles(49081, "NSE_FNO", "RELIANCE", 777, 555, &ours, &dhan);
        assert_eq!(out.len(), 1);
        let m = out[0].clone();
        assert_eq!(m.security_id, 49081);
        assert_eq!(m.segment, "NSE_FNO");
        assert_eq!(m.symbol, "RELIANCE");
        assert_eq!(m.run_ts_ist_nanos, 777);
        assert_eq!(m.trading_date_ist_nanos, 555);
        assert_eq!(m.minute_ts_ist_nanos, ts);
        assert_eq!(m.live_value, 100.0);
        assert_eq!(m.backtest_value, 200.0);
        // Dhan parity rows route to the unified table with feed='dhan'.
        assert_eq!(m.feed, tickvault_common::feed::Feed::Dhan.as_str());
    }

    #[test]
    fn intraday_request_body_has_interval_1_and_string_sid() {
        let from = NaiveDate::from_ymd_opt(2026, 6, 2).expect("from");
        let to = NaiveDate::from_ymd_opt(2026, 6, 3).expect("to");
        let b = intraday_request_body("1333", "NSE_EQ", "EQUITY", from, to);
        assert_eq!(b["securityId"], "1333", "string sid");
        assert_eq!(b["interval"], "1", "1-minute interval as STRING");
        assert_eq!(b["exchangeSegment"], "NSE_EQ");
        assert_eq!(b["fromDate"], "2026-06-02 00:00:00");
        assert_eq!(b["toDate"], "2026-06-03 00:00:00");
    }

    #[test]
    fn intraday_utc_secs_to_ist_minute_nanos_floors_to_minute() {
        // 2026-06-02 06:02:07 UTC = 11:32:07 IST. Floor to 11:32:00 IST.
        let utc = 1_780_380_127_i64; // arbitrary epoch second
        let got = intraday_utc_secs_to_ist_minute_nanos(utc);
        // It must be a whole minute in IST nanos.
        assert_eq!(
            got % (SECONDS_PER_MINUTE * NANOS_PER_SEC),
            0,
            "floored to minute"
        );
        // And exactly IST offset + minute floor.
        let ist = utc + i64::from(IST_UTC_OFFSET_SECONDS);
        let expected = (ist - ist.rem_euclid(60)) * NANOS_PER_SEC;
        assert_eq!(got, expected);
    }

    #[test]
    fn parse_intraday_1m_candles_happy_path() {
        let body = r#"{"open":[100.0,101.0],"high":[100.5,101.5],"low":[99.5,100.5],"close":[100.2,101.2],"volume":[10,20],"timestamp":[1780380120,1780380180]}"#;
        let c = parse_intraday_1m_candles(body);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].open, 100.0);
        assert_eq!(c[1].volume, 20);
        // minute buckets differ by exactly 60s in nanos.
        assert_eq!(
            c[1].minute_ts_ist_nanos - c[0].minute_ts_ist_nanos,
            SECONDS_PER_MINUTE * NANOS_PER_SEC
        );
    }

    #[test]
    fn parse_intraday_1m_candles_rejects_length_mismatch_and_malformed() {
        assert!(parse_intraday_1m_candles("not json").is_empty());
        assert!(parse_intraday_1m_candles("{}").is_empty());
        let mismatch = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"timestamp":[1,2]}"#;
        assert!(parse_intraday_1m_candles(mismatch).is_empty());
    }

    #[test]
    fn parse_intraday_1m_candles_volume_as_float_truncates() {
        let body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[12345.0],"timestamp":[1780380120]}"#;
        let c = parse_intraday_1m_candles(body);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].volume, 12345);
    }

    #[test]
    fn parse_our_candles_dataset_happy_path() {
        let body = r#"{"dataset":[[1780380120000000000,100.0,101.0,99.0,100.5,10],[1780380180000000000,101.0,102.0,100.0,101.5,20]]}"#;
        let c = parse_our_candles_dataset(body);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].minute_ts_ist_nanos, 1_780_380_120_000_000_000);
        assert_eq!(c[0].open, 100.0);
        assert_eq!(c[1].volume, 20);
    }

    #[test]
    fn parse_our_candles_dataset_rejects_short_rows_and_malformed() {
        assert!(parse_our_candles_dataset("nope").is_empty());
        assert!(parse_our_candles_dataset(r#"{"dataset":[]}"#).is_empty());
        // short row (5 cols) skipped.
        let short = r#"{"dataset":[[1,2.0,3.0,4.0,5.0]]}"#;
        assert!(parse_our_candles_dataset(short).is_empty());
    }

    #[test]
    fn our_candles_select_sql_scopes_to_instrument_and_day() {
        let sql = our_candles_select_sql(13, "IDX_I", 1_780_000_000_000_000_000);
        assert!(sql.contains("FROM candles_1m"));
        assert!(sql.contains("security_id = 13"));
        assert!(sql.contains("segment = 'IDX_I'"));
        // feed-scoped to Dhan so a shared (Dhan+Groww) candles_1m table does
        // not double-count minutes in the Dhan-vs-Dhan-historical compare.
        assert!(sql.contains("feed = 'dhan'"));
        assert!(sql.contains("ORDER BY ts ASC"));
        // day window upper bound = start + 24h in nanos.
        assert!(
            sql.contains(&(1_780_000_000_000_000_000_i64 + 86_400 * NANOS_PER_SEC).to_string())
        );
    }

    #[test]
    fn test_compare_stats_merge_is_saturating_sum() {
        let a = CompareStats {
            compared: 3,
            mismatches: 1,
            missing_ours: 2,
        };
        let b = CompareStats {
            compared: 4,
            mismatches: 5,
            missing_ours: 0,
        };
        let m = a.merge(b);
        assert_eq!(m.compared, 7);
        assert_eq!(m.mismatches, 6);
        assert_eq!(m.missing_ours, 2);
    }

    #[test]
    fn full_round_trip_parse_then_diff() {
        // Our candle vs a Dhan candle that disagrees on close only.
        let our_body = r#"{"dataset":[[1780380120000000000,100.0,101.0,99.0,100.5,10]]}"#;
        let ours = parse_our_candles_dataset(our_body);
        // Dhan ts 1780380120 IST-nanos? parse uses UTC→IST. Pick a UTC sec that
        // floors to the SAME IST minute as our 1780380120000000000 ns bucket.
        let our_minute = ours[0].minute_ts_ist_nanos;
        let utc_sec = our_minute / NANOS_PER_SEC - i64::from(IST_UTC_OFFSET_SECONDS);
        let dhan_body = format!(
            r#"{{"open":[100.0],"high":[101.0],"low":[99.0],"close":[100.99],"volume":[10],"timestamp":[{utc_sec}]}}"#
        );
        let dhan = parse_intraday_1m_candles(&dhan_body);
        assert_eq!(dhan.len(), 1);
        assert_eq!(
            dhan[0].minute_ts_ist_nanos, our_minute,
            "minute buckets align"
        );
        let (out, stats) = diff_minute_candles(13, "IDX_I", "NIFTY", 1, 1, &ours, &dhan);
        assert_eq!(stats.compared, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].field, MismatchField::Close);
    }

    #[test]
    fn default_csv_dir_is_under_data() {
        assert_eq!(default_csv_dir(), "data/cross-verify");
    }

    // -----------------------------------------------------------------
    // RunStatus + CSV status line + final flush (DHAN-REST-400, 2026-06-10)
    // -----------------------------------------------------------------

    fn summary(instruments: usize, fetch_failures: usize, compared: usize) -> CrossVerify1mSummary {
        let degraded = instruments > 0
            && (fetch_failures as f64 / instruments as f64) > FETCH_DEGRADED_FAIL_FRACTION;
        CrossVerify1mSummary {
            instruments_checked: instruments,
            fetch_failures,
            stats: CompareStats {
                compared,
                mismatches: 0,
                missing_ours: 0,
            },
            degraded,
        }
    }

    /// RATCHET (the 2026-06-10 incident shape): 776/776 fetch failures,
    /// zero compared → BLIND, never PASS.
    #[test]
    fn test_classify_run_status_blind_when_zero_compared() {
        let s = summary(776, 776, 0);
        assert_eq!(classify_run_status(&s), RunStatus::Blind);
    }

    /// Rule 11: zero compared is BLIND even with zero fetch failures
    /// (forced run on a quiet day) — an empty compare set vouches for
    /// nothing.
    #[test]
    fn test_classify_run_status_blind_even_with_zero_fetch_failures() {
        let s = summary(10, 0, 0);
        assert_eq!(classify_run_status(&s), RunStatus::Blind);
        let empty = CrossVerify1mSummary::default();
        assert_eq!(classify_run_status(&empty), RunStatus::Blind);
    }

    #[test]
    fn test_classify_run_status_degraded_when_fraction_breached() {
        // 200/776 ≈ 26% > 10% threshold, but compared > 0.
        let s = summary(776, 200, 50_000);
        assert_eq!(classify_run_status(&s), RunStatus::Degraded);
    }

    #[test]
    fn test_classify_run_status_pass_when_clean() {
        let s = summary(776, 0, 290_000);
        assert_eq!(classify_run_status(&s), RunStatus::Pass);
        // At-threshold (exactly 10%) is NOT degraded (strict >).
        let at_threshold = summary(100, 10, 30_000);
        assert_eq!(classify_run_status(&at_threshold), RunStatus::Pass);
    }

    #[test]
    fn test_run_status_as_str_labels_are_stable() {
        assert_eq!(RunStatus::Blind.as_str(), "BLIND");
        assert_eq!(RunStatus::Degraded.as_str(), "DEGRADED");
        assert_eq!(RunStatus::Pass.as_str(), "PASS");
    }

    #[test]
    fn test_csv_status_line_format() {
        let s = summary(776, 776, 0);
        let line = csv_status_line(&s, classify_run_status(&s));
        assert_eq!(
            line,
            "# status=BLIND instruments=776 fetch_failures=776 compared=0 mismatches=0 missing_ours=0"
        );
        assert!(
            line.starts_with('#'),
            "comment-prefixed — data grid untouched"
        );
    }

    /// The composed file shape: status line FIRST, header second — one
    /// glance distinguishes "perfect day" from "checked nothing".
    #[test]
    fn test_csv_assembly_status_line_first_then_header() {
        let s = summary(776, 776, 0);
        let status = classify_run_status(&s);
        let body = format!("{}\n", csv_header());
        let composed = format!("{}\n{body}", csv_status_line(&s, status));
        let mut lines = composed.lines();
        assert!(lines.next().unwrap_or("").starts_with("# status=BLIND"));
        assert_eq!(lines.next().unwrap_or(""), csv_header());
    }

    /// RATCHET (2026-06-10 false alarm): zero pending rows must NOT
    /// produce a flush error — even on a disconnected writer.
    #[test]
    fn test_final_flush_skips_when_no_pending_rows() {
        let mut w = FeedParity1mAuditWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(final_flush(&mut w), FlushOutcome::SkippedEmpty);
    }

    /// A REAL failure (rows buffered, sender dead) reports the exact
    /// pending count.
    #[test]
    fn test_final_flush_errors_with_pending_count_when_disconnected() {
        let mut w = FeedParity1mAuditWriter::for_test();
        let m = FeedParity1mMismatch {
            feed: "dhan",
            run_ts_ist_nanos: 1,
            trading_date_ist_nanos: 1,
            security_id: 13,
            segment: "IDX_I".to_string(),
            symbol: "NIFTY".to_string(),
            minute_ts_ist_nanos: 60 * NANOS_PER_SEC,
            field: MismatchField::Open,
            live_value: 1.0,
            backtest_value: 2.0,
        };
        w.append_mismatch(&m).expect("append");
        w.append_mismatch(&m).expect("append");
        assert_eq!(final_flush(&mut w), FlushOutcome::Failed { pending: 2 });
    }

    #[test]
    fn summary_json_contents_round_trips_fields() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).expect("date");
        let summary = CrossVerify1mSummary {
            instruments_checked: 243,
            fetch_failures: 2,
            stats: CompareStats {
                compared: 91_230,
                mismatches: 42,
                missing_ours: 15,
            },
            degraded: false,
        };
        let json = summary_json_contents(date, &summary);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["trading_date"], "2026-06-10");
        assert_eq!(v["instruments_checked"], 243);
        assert_eq!(v["compared"], 91_230);
        assert_eq!(v["mismatches"], 42);
        assert_eq!(v["missing_ours"], 15);
        assert_eq!(v["fetch_failures"], 2);
        assert_eq!(v["degraded"], false);
    }

    #[test]
    fn summary_json_marks_degraded() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).expect("date");
        let summary = CrossVerify1mSummary {
            degraded: true,
            ..Default::default()
        };
        let v: serde_json::Value =
            serde_json::from_str(&summary_json_contents(date, &summary)).expect("valid JSON");
        assert_eq!(v["degraded"], true);
        assert_eq!(v["compared"], 0);
    }
}
