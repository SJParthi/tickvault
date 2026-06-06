//! PR4 (operator 2026-06-01) — boot-time previous-day OHLCV fetch.
//!
//! After auth + instruments load, fetch yesterday's SINGLE daily candle
//! (O/H/L/C/V) for every subscribed SID via Dhan REST
//! `POST /v2/charts/historical` (daily) and persist to the separate
//! `prev_day_ohlcv` table (NEVER `ticks`). Re-allowed by
//! `live-feed-purity.md` rule 9. Strictly bounded: one day, one candle,
//! per symbol; fail-soft (a symbol REST can't return is skipped + logged;
//! boot never blocks); cold path (background task, off the live tick path).
//!
//! This is NOT the deleted 90-day `candle_fetcher.rs`/`cross_verify.rs`
//! chain and does NOT live under `crates/core/src/historical/`.

use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDate;
use secrecy::ExposeSecret;
use serde_json::json;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_core::instrument::daily_universe::{DailyUniverse, InstrumentRole};
use tickvault_storage::prev_day_ohlcv_persistence::{
    PrevDayOhlcvRow, PrevDayOhlcvWriter, ensure_prev_day_ohlcv_table,
    prev_day_ist_nanos_from_utc_secs,
};
use tickvault_trading::oms::rate_limiter::OrderRateLimiter;

/// Dhan Data-API rate limit: 5 requests/second.
const DATA_API_RPS: u32 = 5;
/// Per-request REST timeout.
const REST_TIMEOUT_SECS: u64 = 15;
/// Dhan daily-historical endpoint (base v2 URL from constants).
const HISTORICAL_PATH: &str = "/charts/historical";
/// Max look-back when computing the previous trading day (safety bound).
const MAX_TRADING_DAY_LOOKBACK: i64 = 14;
/// Max spins on the Data-API rate gate before giving up on this symbol's
/// turn (bounded busy-wait: `RATE_GATE_MAX_SPINS × RATE_GATE_BACKOFF_MS`).
const RATE_GATE_MAX_SPINS: u32 = 50;
/// Backoff between rate-gate spins.
const RATE_GATE_BACKOFF_MS: u64 = 50;

/// Boot transport for the daily universe Arc — stashed in `cold_build`
/// (where the universe is fresh), read at the prev-day-fetch spawn site
/// (where token + REST base URL are in scope). Mirrors the `always_on`
/// transport. `None` ⇒ no fetch (Indices4Only scope or build failed).
static UNIVERSE_SNAPSHOT: std::sync::OnceLock<Arc<DailyUniverse>> = std::sync::OnceLock::new();

/// Stash the freshly-built universe (first call wins; boot runs once).
pub fn stash_universe(universe: Arc<DailyUniverse>) {
    drop(UNIVERSE_SNAPSHOT.set(universe));
}

/// Take a clone of the stashed universe Arc, if any.
#[must_use]
pub fn stashed_universe() -> Option<Arc<DailyUniverse>> {
    UNIVERSE_SNAPSHOT.get().cloned()
}

/// One parsed daily candle from the columnar REST response.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DailyCandle {
    pub utc_epoch_secs: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    /// Previous-day open interest. Sources the prev-OI cache (1d
    /// historical-only directive 2026-06-02). Dhan returns `open_interest`
    /// as a columnar array, all-zero for equities/indices (historical-data.md
    /// rule 11). Defaults to 0 when the array is absent/mismatched so an
    /// OI-less equity candle is still accepted.
    pub oi: i64,
}

/// Walk back from `today` to the most recent trading day strictly before it.
/// `is_trading_day` decides each candidate (weekend + holiday aware at the
/// call site via `TradingCalendar::is_trading_day`). Bounded by
/// [`MAX_TRADING_DAY_LOOKBACK`] (returns the last candidate if exceeded —
/// the REST call then simply returns no candle and the symbol is skipped).
/// Pure — O(≤14), closure-based so it's trivially unit-testable.
#[must_use]
pub fn previous_trading_day<F: Fn(NaiveDate) -> bool>(
    today: NaiveDate,
    is_trading_day: F,
) -> NaiveDate {
    let mut day = today;
    for _ in 0..MAX_TRADING_DAY_LOOKBACK {
        day = day.pred_opt().unwrap_or(day);
        if is_trading_day(day) {
            return day;
        }
    }
    day
}

/// The Dhan `instrument` enum value for a subscription target's role.
/// Index rows → `"INDEX"`, F&O-underlying spot rows → `"EQUITY"`.
#[must_use]
pub fn instrument_type_for_role(role: InstrumentRole) -> &'static str {
    match role {
        InstrumentRole::Index => "INDEX",
        InstrumentRole::FnoUnderlying => "EQUITY",
    }
}

/// Build the `/v2/charts/historical` JSON request body for ONE symbol.
/// `to_date` is NON-INCLUSIVE, so `from = prev_trading_day`, `to = today`
/// yields exactly the previous trading day's single daily candle. Pure.
#[must_use]
pub fn historical_request_body(
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
        // Request open-interest (historical-data.md rule 4 — `oi` BOOLEAN
        // optional). Drives the prev-OI cache for F&O; all-zero for
        // equities/indices.
        "oi": true,
        "fromDate": from_date.format("%Y-%m-%d").to_string(),
        "toDate": to_date.format("%Y-%m-%d").to_string(),
    })
}

/// Parse the LAST daily candle from Dhan's columnar response. The response
/// is parallel arrays `open[]/high[]/low[]/close[]/volume[]/timestamp[]`;
/// for a 1-day window there is one element, but we defensively take the
/// last index (and require all arrays to be the same non-zero length).
/// Returns `None` on empty / malformed / length-mismatch. Pure — never panics.
#[must_use]
pub fn parse_last_daily_candle(body: &str) -> Option<DailyCandle> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let arr = |k: &str| v.get(k).and_then(|x| x.as_array());
    let (open, high, low, close, vol, ts) = (
        arr("open")?,
        arr("high")?,
        arr("low")?,
        arr("close")?,
        arr("volume")?,
        arr("timestamp")?,
    );
    let n = ts.len();
    if n == 0
        || open.len() != n
        || high.len() != n
        || low.len() != n
        || close.len() != n
        || vol.len() != n
    {
        return None;
    }
    let i = n - 1; // last candle
    // open_interest is OPTIONAL (only present when `oi:true` was requested AND
    // the instrument is F&O). Equities/indices omit it or send all-zero
    // (historical-data.md rule 11). Default to 0 — never reject the candle for
    // a missing/short OI array.
    let oi = arr("open_interest")
        .filter(|a| a.len() == n)
        .and_then(|a| a[i].as_i64().or_else(|| a[i].as_f64().map(|f| f as i64)))
        .unwrap_or(0);
    Some(DailyCandle {
        utc_epoch_secs: ts[i].as_i64()?,
        open: open[i].as_f64()?,
        high: high[i].as_f64()?,
        low: low[i].as_f64()?,
        close: close[i].as_f64()?,
        // Dhan volume may serialize as float; truncate to i64 defensively.
        volume: vol[i]
            .as_i64()
            .or_else(|| vol[i].as_f64().map(|f| f as i64))?,
        oi,
    })
}

/// Outcome counts for the boot summary (operator Telegram + log).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PrevDayFetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    pub failed: usize,
}

/// Minimum % of subscription targets that must have yesterday's daily candle
/// for the pre-market fetch to count as healthy (mirrors the 90% live-vs-
/// historical cross-match bar). Below this → degraded (false-OK guard,
/// audit-findings Rule 11 — never report OK on a thin/empty set).
pub const PREV_DAY_COVERAGE_MIN_PCT: u32 = 90;

/// Directory for the per-day pre-market coverage report CSV.
const PREV_DAY_CSV_DIR: &str = "data/prev-day";

/// Where the pre-market coverage CSV is written (operator-visible surface).
pub const fn prev_day_csv_dir() -> &'static str {
    PREV_DAY_CSV_DIR
}

/// Verdict for the pre-market prev-day fetch coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrevDayCoverage {
    /// Coverage met the bar.
    Ok { pct: u32 },
    /// Some symbols missing yesterday's candle (below the bar).
    Degraded { pct: u32 },
    /// No yesterday candles at all (or no targets) — nothing to vouch for.
    Empty,
}

/// Pure verification: did the pre-market fetch get yesterday's daily candle for
/// enough of the subscribed universe? `expected` = number of subscription
/// targets; `fetched` = how many got a valid candle persisted. Never reports
/// `Ok` on an empty set (audit-findings Rule 11 — no false-OK).
pub fn evaluate_prev_day_coverage(expected: usize, fetched: usize) -> PrevDayCoverage {
    if expected == 0 || fetched == 0 {
        return PrevDayCoverage::Empty;
    }
    let pct = ((fetched as u64).saturating_mul(100) / expected as u64) as u32;
    if pct >= PREV_DAY_COVERAGE_MIN_PCT {
        PrevDayCoverage::Ok { pct }
    } else {
        PrevDayCoverage::Degraded { pct }
    }
}

/// Render the one-row coverage report as CSV text (pure — unit-tested). A human
/// opens this in Excel; `make prev-day-show` prints it.
pub fn coverage_csv(
    trading_date_ist: NaiveDate,
    expected: usize,
    summary: &PrevDayFetchSummary,
) -> String {
    let outcome = match evaluate_prev_day_coverage(expected, summary.fetched) {
        PrevDayCoverage::Ok { .. } => "ok",
        PrevDayCoverage::Degraded { .. } => "degraded",
        PrevDayCoverage::Empty => "empty",
    };
    let pct = if expected == 0 {
        0
    } else {
        ((summary.fetched as u64).saturating_mul(100) / expected as u64) as u32
    };
    format!(
        "trading_date_ist,expected,fetched,skipped,failed,coverage_pct,outcome\n\
         {trading_date_ist},{expected},{f},{s},{fl},{pct},{outcome}\n",
        f = summary.fetched,
        s = summary.skipped,
        fl = summary.failed,
    )
}

/// Write the coverage CSV to `data/prev-day/prev-day-coverage-YYYY-MM-DD.csv`.
/// Thin FS wrapper around `coverage_csv`; the caller logs on `Err` (fail-soft).
pub fn write_prev_day_coverage_csv(
    dir: &str,
    trading_date_ist: NaiveDate,
    expected: usize,
    summary: &PrevDayFetchSummary,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = std::path::Path::new(dir).join(format!("prev-day-coverage-{trading_date_ist}.csv"));
    std::fs::write(&path, coverage_csv(trading_date_ist, expected, summary))?;
    Ok(path)
}

#[cfg(test)]
mod coverage_tests {
    use super::{
        PREV_DAY_COVERAGE_MIN_PCT, PrevDayCoverage, PrevDayFetchSummary, coverage_csv,
        evaluate_prev_day_coverage, prev_day_csv_dir, write_prev_day_coverage_csv,
    };
    use chrono::NaiveDate;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 4).unwrap()
    }

    #[test]
    fn test_evaluate_prev_day_coverage_full_is_ok() {
        assert_eq!(
            evaluate_prev_day_coverage(243, 243),
            PrevDayCoverage::Ok { pct: 100 }
        );
    }

    #[test]
    fn test_evaluate_prev_day_coverage_at_threshold_is_ok() {
        // 90 of 100 = exactly the bar.
        assert_eq!(
            evaluate_prev_day_coverage(100, PREV_DAY_COVERAGE_MIN_PCT as usize),
            PrevDayCoverage::Ok { pct: 90 }
        );
    }

    #[test]
    fn test_evaluate_prev_day_coverage_below_threshold_is_degraded() {
        assert_eq!(
            evaluate_prev_day_coverage(100, 89),
            PrevDayCoverage::Degraded { pct: 89 }
        );
    }

    #[test]
    fn test_evaluate_prev_day_coverage_zero_fetched_is_empty() {
        assert_eq!(evaluate_prev_day_coverage(243, 0), PrevDayCoverage::Empty);
    }

    #[test]
    fn test_evaluate_prev_day_coverage_zero_expected_is_empty() {
        assert_eq!(evaluate_prev_day_coverage(0, 0), PrevDayCoverage::Empty);
    }

    #[test]
    fn test_coverage_csv_has_header_and_row() {
        let s = PrevDayFetchSummary {
            fetched: 240,
            skipped: 2,
            failed: 1,
        };
        let csv = coverage_csv(date(), 243, &s);
        assert!(csv.starts_with(
            "trading_date_ist,expected,fetched,skipped,failed,coverage_pct,outcome\n"
        ));
        assert!(csv.contains("2026-06-04,243,240,2,1,98,ok\n"));
    }

    #[test]
    fn test_coverage_csv_marks_empty_outcome() {
        let s = PrevDayFetchSummary {
            fetched: 0,
            skipped: 0,
            failed: 243,
        };
        let csv = coverage_csv(date(), 243, &s);
        assert!(csv.contains(",0,0,243,0,empty\n"));
    }

    #[test]
    fn test_write_prev_day_coverage_csv_writes_expected_content() {
        let dir = std::env::temp_dir().join(format!("tv-prevday-{}", std::process::id()));
        let dir_str = dir.to_string_lossy().to_string();
        let s = PrevDayFetchSummary {
            fetched: 243,
            skipped: 0,
            failed: 0,
        };
        let path = write_prev_day_coverage_csv(&dir_str, date(), 243, &s).expect("write ok");
        let read = std::fs::read_to_string(&path).expect("read ok");
        assert_eq!(read, coverage_csv(date(), 243, &s));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prev_day_csv_dir_is_under_data() {
        assert_eq!(prev_day_csv_dir(), "data/prev-day");
    }
}

/// Fetch + persist the previous trading day's daily candle for every
/// subscription target. Fail-soft per symbol; boot never blocks.
///
/// Cold path — spawned as a background task AFTER the universe is built.
/// Rate-limited to the Data-API 5/sec budget; a `DH-904` (429) waits and
/// retries the symbol ONCE, then skips it.
// Orchestrator needs a live Dhan REST endpoint + QuestDB; the pure helpers
// (previous_trading_day, historical_request_body, parse_last_daily_candle,
// instrument_type_for_role) are unit-tested below; boot wiring is in main.rs.
// TEST-EXEMPT: live-deps async orchestrator (pure helpers unit-tested above).
pub async fn run_prev_day_ohlcv_fetch(
    universe: Arc<DailyUniverse>,
    token_handle: TokenHandle,
    questdb_config: QuestDbConfig,
    base_url: String,
    from_date: NaiveDate,
    to_date: NaiveDate,
) -> PrevDayFetchSummary {
    // Ensure the destination table (idempotent CREATE TABLE IF NOT EXISTS,
    // schema-self-heal pattern). Fail-soft: per-symbol writes no-op if the
    // table is somehow absent.
    ensure_prev_day_ohlcv_table(&questdb_config).await;

    let jwt = {
        let guard = token_handle.load();
        match guard.as_ref() {
            Some(state) => state.access_token().expose_secret().to_string(),
            None => {
                error!("prev_day_ohlcv: no JWT available at boot — fetch skipped");
                return PrevDayFetchSummary::default();
            }
        }
    };

    // from_date = previous trading day, to_date = today (non-inclusive →
    // yields exactly the previous trading day's candle). Both computed at
    // the call site where the calendar is in scope.
    let url = format!("{base_url}{HISTORICAL_PATH}");

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(REST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                ?err,
                "prev_day_ohlcv: HTTP client build failed — fetch skipped"
            );
            return PrevDayFetchSummary::default();
        }
    };
    let limiter = OrderRateLimiter::new(DATA_API_RPS);
    let mut writer = PrevDayOhlcvWriter::new(&questdb_config);
    let mut summary = PrevDayFetchSummary::default();

    for target in &universe.subscription_targets {
        let row = &target.csv_row;
        let instrument = instrument_type_for_role(target.role);
        let body = historical_request_body(
            row.security_id.trim(),
            row.segment.trim(),
            instrument,
            from_date,
            to_date,
        );

        // Data-API rate gate (5/sec). Busy-wait the GCRA cell, bounded.
        for _ in 0..RATE_GATE_MAX_SPINS {
            if limiter.check().is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(RATE_GATE_BACKOFF_MS)).await;
        }

        match fetch_one(&client, &url, &jwt, &body).await {
            Ok(Some(candle)) => {
                let Ok(sid) = row.security_id.trim().parse::<i64>() else {
                    summary.skipped = summary.skipped.saturating_add(1);
                    continue;
                };
                let prow = PrevDayOhlcvRow {
                    ts_ist_nanos: prev_day_ist_nanos_from_utc_secs(candle.utc_epoch_secs),
                    security_id: sid,
                    segment: segment_static(&row.segment),
                    open: candle.open,
                    high: candle.high,
                    low: candle.low,
                    close: candle.close,
                    volume: candle.volume,
                    oi: candle.oi,
                };
                if writer.append_row(&prow).is_ok() {
                    summary.fetched = summary.fetched.saturating_add(1);
                } else {
                    summary.failed = summary.failed.saturating_add(1);
                }
            }
            Ok(None) => {
                // No candle for this symbol (illiquid / holiday / far OTM).
                summary.skipped = summary.skipped.saturating_add(1);
            }
            Err(reason) => {
                warn!(security_id = %row.security_id, %reason, "prev_day_ohlcv: symbol fetch failed (skipped)");
                summary.failed = summary.failed.saturating_add(1);
            }
        }
    }

    if writer.flush().is_err() {
        error!("prev_day_ohlcv: final flush failed — rows may be unpersisted");
    }
    info!(
        fetched = summary.fetched,
        skipped = summary.skipped,
        failed = summary.failed,
        from = %from_date,
        "prev_day_ohlcv: boot fetch complete"
    );
    summary
}

/// One symbol's REST round-trip. Returns `Ok(None)` for an empty/malformed
/// body (skip), `Err` for a transport/HTTP error (counts as failed).
async fn fetch_one(
    client: &reqwest::Client,
    url: &str,
    jwt: &str,
    body: &serde_json::Value,
) -> Result<Option<DailyCandle>, String> {
    let resp = client
        .post(url)
        .header("access-token", jwt)
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| format!("read: {e}"))?;
    Ok(parse_last_daily_candle(&text))
}

/// Map a runtime segment string to the `&'static str` the storage row needs.
/// Covers the segments our universe actually carries (indices + NSE_EQ).
#[must_use]
fn segment_static(segment: &str) -> &'static str {
    match segment.trim() {
        "IDX_I" => "IDX_I",
        "NSE_EQ" => "NSE_EQ",
        "NSE_FNO" => "NSE_FNO",
        "BSE_EQ" => "BSE_EQ",
        "BSE_FNO" => "BSE_FNO",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Weekday-only "trading day" predicate for tests (Mon–Fri).
    fn is_weekday(d: NaiveDate) -> bool {
        use chrono::Datelike;
        d.weekday().num_days_from_monday() < 5
    }

    #[test]
    fn previous_trading_day_skips_weekend() {
        // 2026-06-01 is a Monday → previous trading day is Friday 2026-05-29.
        let mon = NaiveDate::from_ymd_opt(2026, 6, 1).expect("date");
        let prev = previous_trading_day(mon, is_weekday);
        assert_eq!(prev, NaiveDate::from_ymd_opt(2026, 5, 29).expect("fri"));
        assert!(is_weekday(prev));
    }

    #[test]
    fn previous_trading_day_is_strictly_before_today() {
        let tue = NaiveDate::from_ymd_opt(2026, 6, 2).expect("date");
        let prev = previous_trading_day(tue, is_weekday);
        assert!(prev < tue);
    }

    #[test]
    fn instrument_type_for_role_maps_role() {
        assert_eq!(instrument_type_for_role(InstrumentRole::Index), "INDEX");
        assert_eq!(
            instrument_type_for_role(InstrumentRole::FnoUnderlying),
            "EQUITY"
        );
    }

    #[test]
    fn historical_request_body_noninclusive_dates_string_sid() {
        let from = NaiveDate::from_ymd_opt(2026, 5, 29).expect("from");
        let to = NaiveDate::from_ymd_opt(2026, 6, 1).expect("to");
        let b = historical_request_body("1333", "NSE_EQ", "EQUITY", from, to);
        assert_eq!(b["securityId"], "1333", "securityId is a STRING");
        assert_eq!(b["exchangeSegment"], "NSE_EQ");
        assert_eq!(b["instrument"], "EQUITY");
        assert_eq!(b["oi"], true, "open-interest requested for prev-OI cache");
        assert_eq!(b["fromDate"], "2026-05-29");
        assert_eq!(b["toDate"], "2026-06-01");
    }

    #[test]
    fn parse_last_daily_candle_happy_path() {
        let body = r#"{"open":[100.5],"high":[105.0],"low":[99.0],"close":[104.25],"volume":[12345],"timestamp":[1748563200]}"#;
        let c = parse_last_daily_candle(body).expect("parse");
        assert_eq!(c.open, 100.5);
        assert_eq!(c.close, 104.25);
        assert_eq!(c.volume, 12345);
        assert_eq!(c.utc_epoch_secs, 1_748_563_200);
        // No open_interest array (equity-style) → OI defaults to 0.
        assert_eq!(c.oi, 0, "missing open_interest defaults to 0");
    }

    #[test]
    fn parse_open_interest_for_fno() {
        // F&O daily candle WITH open_interest (oi:true requested).
        let body = r#"{"open":[250.0],"high":[260.0],"low":[245.0],"close":[255.0],"volume":[12345],"open_interest":[987654],"timestamp":[1748563200]}"#;
        let c = parse_last_daily_candle(body).expect("parse");
        assert_eq!(c.oi, 987_654, "F&O open_interest parsed");
    }

    #[test]
    fn parse_open_interest_takes_last_when_multiple() {
        let body = r#"{"open":[1.0,2.0],"high":[1.0,2.0],"low":[1.0,2.0],"close":[1.0,2.0],"volume":[10,20],"open_interest":[111,222],"timestamp":[100,200]}"#;
        let c = parse_last_daily_candle(body).expect("parse");
        assert_eq!(c.oi, 222, "takes the LAST open_interest");
    }

    #[test]
    fn parse_open_interest_length_mismatch_defaults_zero() {
        // OI array shorter than the rest → candle still parses, OI = 0.
        let body = r#"{"open":[1.0,2.0],"high":[1.0,2.0],"low":[1.0,2.0],"close":[1.0,2.0],"volume":[10,20],"open_interest":[111],"timestamp":[100,200]}"#;
        let c = parse_last_daily_candle(body).expect("parse");
        assert_eq!(c.oi, 0, "mismatched OI length → 0, candle still valid");
        assert_eq!(c.close, 2.0, "candle body unaffected");
    }

    #[test]
    fn parse_open_interest_as_float_truncates() {
        let body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"open_interest":[55555.0],"timestamp":[100]}"#;
        let c = parse_last_daily_candle(body).expect("parse");
        assert_eq!(c.oi, 55_555);
    }

    #[test]
    fn parse_takes_last_when_multiple() {
        let body = r#"{"open":[1.0,2.0],"high":[1.0,2.0],"low":[1.0,2.0],"close":[1.0,2.0],"volume":[10,20],"timestamp":[100,200]}"#;
        let c = parse_last_daily_candle(body).expect("parse");
        assert_eq!(c.close, 2.0, "takes the LAST candle");
        assert_eq!(c.utc_epoch_secs, 200);
    }

    #[test]
    fn parse_empty_arrays_returns_none() {
        let body = r#"{"open":[],"high":[],"low":[],"close":[],"volume":[],"timestamp":[]}"#;
        assert!(parse_last_daily_candle(body).is_none());
    }

    #[test]
    fn parse_length_mismatch_returns_none() {
        let body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"timestamp":[100,200]}"#;
        assert!(
            parse_last_daily_candle(body).is_none(),
            "mismatched lengths rejected"
        );
    }

    #[test]
    fn parse_malformed_returns_none_never_panics() {
        assert!(parse_last_daily_candle("not json").is_none());
        assert!(parse_last_daily_candle("{}").is_none());
        assert!(parse_last_daily_candle(r#"{"open":[1.0]}"#).is_none());
    }

    #[test]
    fn parse_volume_as_float_truncates() {
        let body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[12345.0],"timestamp":[100]}"#;
        let c = parse_last_daily_candle(body).expect("parse");
        assert_eq!(c.volume, 12345);
    }

    #[test]
    fn segment_static_maps_known_and_unknown() {
        assert_eq!(segment_static("IDX_I"), "IDX_I");
        assert_eq!(segment_static("NSE_EQ"), "NSE_EQ");
        assert_eq!(segment_static("WEIRD"), "UNKNOWN");
    }

    #[test]
    fn test_stash_universe_and_stashed_universe_roundtrip() {
        // This test owns the UNIVERSE_SNAPSHOT OnceLock.
        assert!(stashed_universe().is_none(), "empty before stash");
        let u = Arc::new(DailyUniverse {
            subscription_targets: Vec::new(),
            fno_contracts: Vec::new(),
        });
        stash_universe(u);
        assert!(stashed_universe().is_some(), "present after stash");
    }
}
