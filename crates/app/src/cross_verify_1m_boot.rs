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
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_storage::cross_verify_1m_audit_persistence::{
    CrossVerify1mAuditWriter, CrossVerify1mMismatch, MismatchField, csv_header,
    ensure_cross_verify_1m_audit_table, mismatch_to_csv_line,
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
/// `CrossVerify1mMismatch` per differing field; otherwise count it as
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
) -> (Vec<CrossVerify1mMismatch>, CompareStats) {
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
        let mut push = |field: MismatchField, our_value: f64, dhan_value: f64| {
            // Cold path (post-market, once/day) — owning the segment/symbol
            // Strings per cell is irrelevant to performance.
            out.push(CrossVerify1mMismatch {
                run_ts_ist_nanos,
                trading_date_ist_nanos,
                security_id,
                segment: segment.to_string(),
                symbol: symbol.to_string(),
                minute_ts_ist_nanos: d.minute_ts_ist_nanos,
                field,
                our_value,
                dhan_value,
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
#[must_use]
pub fn our_candles_select_sql(security_id: i64, segment: &str, day_start_ist_nanos: i64) -> String {
    let day_end = day_start_ist_nanos.saturating_add(86_400 * NANOS_PER_SEC);
    format!(
        "SELECT (ts / 1) AS ts_nanos, open, high, low, close, volume \
         FROM candles_1m \
         WHERE security_id = {security_id} AND segment = '{segment}' \
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
    ensure_cross_verify_1m_audit_table(&questdb_config).await;

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

    let intraday_url = format!("{base_url}{INTRADAY_PATH}");
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
    let mut writer = CrossVerify1mAuditWriter::new(&questdb_config);

    let trading_date_ist_nanos = day_start_ist_nanos;
    let to_date = trading_date.succ_opt().unwrap_or(trading_date);
    let mut summary = CrossVerify1mSummary::default();
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

    if writer.flush().is_err() {
        error!(
            code = tickvault_common::error_code::ErrorCode::CrossVerify1m01MismatchFound.code_str(),
            "cross_verify_1m: final audit flush failed — rows may be unpersisted"
        );
    }

    // Degraded if too many symbols failed to fetch (false-OK guard).
    if summary.instruments_checked > 0 {
        let fail_frac = summary.fetch_failures as f64 / summary.instruments_checked as f64;
        summary.degraded = fail_frac > FETCH_DEGRADED_FAIL_FRACTION;
    }

    write_csv_file(csv_dir, trading_date, &csv).await;

    if summary.stats.mismatches > 0 {
        error!(
            code = tickvault_common::error_code::ErrorCode::CrossVerify1m01MismatchFound.code_str(),
            compared = summary.stats.compared,
            mismatches = summary.stats.mismatches,
            missing = summary.stats.missing_ours,
            "cross_verify_1m: OHLCV mismatches found vs Dhan intraday"
        );
    }
    if summary.degraded {
        error!(
            code = tickvault_common::error_code::ErrorCode::CrossVerify1m02FetchDegraded.code_str(),
            fetch_failures = summary.fetch_failures,
            instruments = summary.instruments_checked,
            "cross_verify_1m: intraday fetch degraded — partial coverage"
        );
    }
    info!(
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
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
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
        assert_eq!(m.our_value, 100.0);
        assert_eq!(m.dhan_value, 200.0);
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
}
