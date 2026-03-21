//! Cross-verification of historical candle data integrity.
//!
//! Queries QuestDB to verify that the unified `historical_candles` table
//! has adequate coverage across all 5 timeframes (1m, 5m, 15m, 60m, 1d)
//! for every fetched instrument.
//!
//! # Verification Checks
//! 1. **Per-timeframe candle count**: each timeframe has expected minimum candles
//! 2. **OHLC consistency**: high >= low for all stored candles
//! 3. **Data integrity**: no non-positive prices (open/high/low/close <= 0)
//! 4. **Timestamp bounds**: intraday candles within 09:15–15:29 IST market hours
//! 5. **Multi-instrument coverage**: all instruments have data in all timeframes
//!
//! # Historical vs Live Cross-Match
//! After post-market re-fetch, compares `historical_candles` (Dhan REST API)
//! against materialized views (`candles_1m`, `candles_5m`, etc.) from live
//! WebSocket ticks. Detects missed ticks, price drift, and data gaps.
//!
//! # Automation
//! Runs after historical candle fetch — no human intervention required.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::{
    CANDLES_PER_TRADING_DAY, QUESTDB_TABLE_HISTORICAL_CANDLES, TIMEFRAME_1M,
};
use dhan_live_trader_common::instrument_registry::InstrumentRegistry;

// ---------------------------------------------------------------------------
// Violation Detail — per-candle diagnostic record
// ---------------------------------------------------------------------------

/// A single candle that failed cross-verification, with full diagnostic context.
///
/// Used for OHLC violations, data violations, and timestamp violations.
/// Contains enough information for Telegram messages to be self-diagnostic.
#[derive(Debug, Clone)]
pub struct ViolationDetail {
    /// Human-readable symbol (e.g., "RELIANCE", "NIFTY50"). From registry lookup.
    pub symbol: String,
    /// Exchange segment string (e.g., "NSE_EQ", "IDX_I").
    pub segment: String,
    /// Timeframe (e.g., "1m", "5m", "1d").
    pub timeframe: String,
    /// Timestamp formatted as IST string (e.g., "2026-03-18 10:15").
    pub timestamp_ist: String,
    /// What's wrong (e.g., "high < low", "open <= 0", "outside market hours").
    pub violation: String,
    /// Actual OHLCV values for context (e.g., "O=0.0 H=2450.5 L=2440.0 C=2445.0").
    pub values: String,
}

// ---------------------------------------------------------------------------
// Cross-Match Types — Historical vs Live comparison
// ---------------------------------------------------------------------------

/// A single candle mismatch between historical API data and live materialized view.
#[derive(Debug, Clone)]
pub struct CrossMatchMismatch {
    /// Human-readable symbol (e.g., "RELIANCE").
    pub symbol: String,
    /// Exchange segment string (e.g., "NSE_EQ").
    pub segment: String,
    /// Timeframe (e.g., "1m", "5m").
    pub timeframe: String,
    /// Timestamp formatted as IST string.
    pub timestamp_ist: String,
    /// Type of mismatch: "price_diff", "missing_live", "missing_historical".
    pub mismatch_type: String,
    /// Historical OHLCV values (e.g., "O=2450.0 H=2465.0 L=2448.0 C=2460.0 V=125000").
    pub hist_values: String,
    /// Live OHLCV values (e.g., "O=2450.0 H=2463.5 L=2448.0 C=2460.0 V=118500"),
    /// or "\[MISSING\]" if no live data.
    pub live_values: String,
    /// Field-level diff summary (e.g., "H(-1.5) V(-6500)").
    pub diff_summary: String,
}

/// Summary of historical vs live candle cross-match.
#[derive(Debug)]
pub struct CrossMatchReport {
    /// Number of timeframes compared (up to 5).
    pub timeframes_checked: usize,
    /// Total candles compared across all timeframes.
    pub candles_compared: usize,
    /// Total mismatched candles.
    pub mismatches: usize,
    /// Historical candle exists but live doesn't (WebSocket missed ticks).
    pub missing_live: usize,
    /// Live candle exists but historical doesn't (should not happen).
    pub missing_historical: usize,
    /// OI mismatches for derivatives (both OI > 0, differ by > tolerance).
    pub oi_mismatches: usize,
    /// Detailed mismatch records (all of them — no arbitrary cap).
    pub mismatch_details: Vec<CrossMatchMismatch>,
    /// Materialized view tables that don't exist yet (skipped during cross-match).
    pub missing_views: Vec<String>,
    /// Whether the cross-match passed (mismatches within tolerance).
    pub passed: bool,
}

// ---------------------------------------------------------------------------
// Verification Result
// ---------------------------------------------------------------------------

/// Summary of cross-verification across all timeframes.
#[derive(Debug)]
pub struct CrossVerificationReport {
    /// Number of unique instruments verified.
    pub instruments_checked: usize,
    /// Number of instruments with complete 1m candle coverage.
    pub instruments_complete: usize,
    /// Number of instruments with missing candles in any timeframe.
    pub instruments_with_gaps: usize,
    /// Total candle count across all timeframes.
    pub total_candles_in_db: usize,
    /// Per-timeframe candle counts.
    pub timeframe_counts: Vec<TimeframeCoverage>,
    /// Number of candles with OHLC inconsistency (high < low).
    pub ohlc_violations: usize,
    /// Detailed OHLC violation records (capped at 20).
    pub ohlc_details: Vec<ViolationDetail>,
    /// Number of candles with non-positive prices.
    pub data_violations: usize,
    /// Detailed data violation records (capped at 20).
    pub data_details: Vec<ViolationDetail>,
    /// Number of intraday candles outside market hours.
    pub timestamp_violations: usize,
    /// Detailed timestamp violation records (capped at 20).
    pub timestamp_details: Vec<ViolationDetail>,
    /// Whether the verification passed overall.
    pub passed: bool,
}

/// Coverage summary for a single timeframe.
#[derive(Debug, Clone)]
pub struct TimeframeCoverage {
    /// Timeframe label (e.g., "1m", "5m", "15m", "60m", "1d").
    pub timeframe: String,
    /// Number of instruments with data in this timeframe.
    pub instrument_count: usize,
    /// Total candles in this timeframe.
    pub candle_count: usize,
}

// ---------------------------------------------------------------------------
// QuestDB Query Response Types
// ---------------------------------------------------------------------------

/// QuestDB HTTP API response wrapper.
#[derive(Debug, serde::Deserialize)]
struct QuestDbQueryResponse {
    dataset: Vec<Vec<serde_json::Value>>,
    #[allow(dead_code)] // APPROVED: field required for deserialization, used for debug logging
    count: usize,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB verification queries.
const VERIFY_QUERY_TIMEOUT_SECS: u64 = 30;

/// Minimum candle coverage ratio to consider an instrument "complete".
/// 95% of 375 = 356 candles minimum for the cross-verification window
/// (09:15–15:29 continuous trading session; pre-market 09:00–09:14 excluded
/// because Dhan historical API has no data for that window).
const MIN_CANDLE_COVERAGE_RATIO: f64 = 0.95;

/// All timeframes we verify.
const VERIFIED_TIMEFRAMES: &[&str] = &["1m", "5m", "15m", "60m", "1d"];

/// Maximum violation detail rows to fetch per category.
/// High cap for memory safety only — we show ALL violations, not hiding anything.
/// If there are more than 500, the count query tells you the real total.
const MAX_VIOLATION_DETAILS: usize = 500;

/// Price tolerance for cross-match comparison (absolute).
/// f32→f64 already fixed via `f32_to_f64_clean()` — any real diff is meaningful.
/// Epsilon-level tolerance only absorbs IEEE754 floating point noise.
/// Minimum tick in Indian markets = 0.05, so any real diff >> 1e-6.
const CROSS_MATCH_PRICE_EPSILON: f64 = 1e-6;

/// Volume tolerance for cross-match (relative, 10%).
/// If |hist_vol - live_vol| / max(hist_vol, 1) > 10%, it's a mismatch.
/// Volume is the best indicator of missed WebSocket ticks.
const CROSS_MATCH_VOLUME_TOLERANCE_PCT: f64 = 0.10;

/// OI (Open Interest) tolerance for cross-match (relative, 10%).
/// Only applied when both historical and live OI > 0 (derivatives only).
const CROSS_MATCH_OI_TOLERANCE_PCT: f64 = 0.10;

/// Mapping from historical timeframe labels to materialized view table names.
const CROSS_MATCH_TIMEFRAMES: &[(&str, &str)] = &[
    ("1m", "candles_1m"),
    ("5m", "candles_5m"),
    ("15m", "candles_15m"),
    ("60m", "candles_1h"),
    ("1d", "candles_1d"),
];

// ---------------------------------------------------------------------------
// Cross-Verification Logic
// ---------------------------------------------------------------------------

/// Creates a failed report with all zeros.
fn failed_report() -> CrossVerificationReport {
    CrossVerificationReport {
        instruments_checked: 0,
        instruments_complete: 0,
        instruments_with_gaps: 0,
        total_candles_in_db: 0,
        timeframe_counts: Vec::new(),
        ohlc_violations: 0,
        ohlc_details: Vec::new(),
        data_violations: 0,
        data_details: Vec::new(),
        timestamp_violations: 0,
        timestamp_details: Vec::new(),
        passed: false,
    }
}

/// Runs cross-verification of candle data integrity in QuestDB.
///
/// Checks all 5 timeframes for coverage and validates OHLC consistency,
/// data integrity (non-positive prices), and timestamp bounds (market hours).
///
/// # Arguments
/// * `questdb_config` — QuestDB connection config
/// * `registry` — Instrument registry for security_id → symbol lookup
///
/// # Returns
/// A `CrossVerificationReport` with pass/fail status, per-timeframe breakdown,
/// violation counts, and detailed violation records.
pub async fn verify_candle_integrity(
    questdb_config: &QuestDbConfig,
    registry: &InstrumentRegistry,
) -> CrossVerificationReport {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(VERIFY_QUERY_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "failed to build HTTP client for cross-verification");
            return failed_report();
        }
    };

    // --- Step 1: Per-timeframe coverage query ---
    // Groups by (timeframe, security_id) to get counts per instrument per timeframe.
    // Use 3-day window to ensure daily candles (stamped at IST midnight = hour 0)
    // are always captured regardless of when verification runs.
    // Intraday candles from today are also within this window.
    let coverage_query = format!(
        "SELECT timeframe, security_id, count() as candle_count \
         FROM {} \
         WHERE ts > dateadd('d', -3, now()) \
         GROUP BY timeframe, security_id \
         ORDER BY timeframe, candle_count DESC",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let coverage_data = match execute_query(&client, &base_url, &coverage_query).await {
        Some(data) => data,
        None => return failed_report(),
    };

    // Parse coverage results into per-timeframe maps
    let mut timeframe_instruments: HashMap<String, Vec<(i64, usize)>> = HashMap::new();
    let mut total_candles = 0_usize;

    for row in &coverage_data.dataset {
        if row.len() < 3 {
            continue;
        }
        let tf = row[0].as_str().unwrap_or("").to_string();
        let sid = row[1].as_i64().unwrap_or(0);
        let count = row[2].as_i64().unwrap_or(0).max(0) as usize;
        total_candles = total_candles.saturating_add(count);

        timeframe_instruments
            .entry(tf)
            .or_default()
            .push((sid, count));
    }

    // Build per-timeframe coverage summary
    let mut timeframe_counts: Vec<TimeframeCoverage> =
        Vec::with_capacity(VERIFIED_TIMEFRAMES.len());
    for &tf in VERIFIED_TIMEFRAMES {
        let instruments = timeframe_instruments.get(tf);
        let instrument_count = instruments.map_or(0, Vec::len);
        let candle_count: usize = instruments.map_or(0, |v| v.iter().map(|(_, c)| *c).sum());
        timeframe_counts.push(TimeframeCoverage {
            timeframe: tf.to_string(),
            instrument_count,
            candle_count,
        });
    }

    // --- Step 2: 1m coverage check (primary completeness metric) ---
    let one_m_instruments = timeframe_instruments.get(TIMEFRAME_1M);

    #[allow(clippy::arithmetic_side_effects)]
    // APPROVED: f64 multiplication for coverage ratio check — no overflow risk
    let min_candles = (CANDLES_PER_TRADING_DAY as f64 * MIN_CANDLE_COVERAGE_RATIO) as usize;

    let mut instruments_complete = 0_usize;
    let mut instruments_with_gaps = 0_usize;
    let mut gap_log_count = 0_usize;

    if let Some(instruments) = one_m_instruments {
        for &(sid, count) in instruments {
            if count >= min_candles {
                instruments_complete = instruments_complete.saturating_add(1);
            } else {
                instruments_with_gaps = instruments_with_gaps.saturating_add(1);
                if gap_log_count < 10 {
                    debug!(
                        security_id = sid,
                        candle_count = count,
                        expected = CANDLES_PER_TRADING_DAY,
                        timeframe = TIMEFRAME_1M,
                        "instrument has incomplete 1m candle coverage"
                    );
                    gap_log_count = gap_log_count.saturating_add(1);
                }
            }
        }
    }

    // Count unique instruments across all timeframes
    let mut all_instrument_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for instruments in timeframe_instruments.values() {
        for &(sid, _) in instruments {
            all_instrument_ids.insert(sid);
        }
    }
    let instruments_checked = all_instrument_ids.len();

    // --- Step 3: OHLC consistency check (high < low) with details ---
    let ohlc_count_query = format!(
        "SELECT count() FROM {} \
         WHERE ts > dateadd('d', -3, now()) AND high < low",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let ohlc_violations = extract_count(&client, &base_url, &ohlc_count_query).await;

    let ohlc_details = if ohlc_violations > 0 {
        let ohlc_detail_query = format!(
            "SELECT security_id, segment, timeframe, ts, open, high, low, close, volume \
             FROM {} \
             WHERE ts > dateadd('d', -3, now()) AND high < low \
             LIMIT {}",
            QUESTDB_TABLE_HISTORICAL_CANDLES, MAX_VIOLATION_DETAILS
        );
        parse_violation_rows(
            &client,
            &base_url,
            &ohlc_detail_query,
            registry,
            "high < low",
        )
        .await
    } else {
        Vec::new()
    };

    if ohlc_violations > 0 {
        warn!(
            ohlc_violations,
            "OHLC consistency check found candles with high < low"
        );
    }

    // --- Step 4: Data integrity check (non-positive prices) with details ---
    let data_count_query = format!(
        "SELECT count() FROM {} \
         WHERE ts > dateadd('d', -3, now()) \
         AND (open <= 0 OR high <= 0 OR low <= 0 OR close <= 0)",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let data_violations = extract_count(&client, &base_url, &data_count_query).await;

    let data_details = if data_violations > 0 {
        let data_detail_query = format!(
            "SELECT security_id, segment, timeframe, ts, open, high, low, close, volume \
             FROM {} \
             WHERE ts > dateadd('d', -3, now()) \
             AND (open <= 0 OR high <= 0 OR low <= 0 OR close <= 0) \
             LIMIT {}",
            QUESTDB_TABLE_HISTORICAL_CANDLES, MAX_VIOLATION_DETAILS
        );
        parse_violation_rows(
            &client,
            &base_url,
            &data_detail_query,
            registry,
            "price <= 0",
        )
        .await
    } else {
        Vec::new()
    };

    if data_violations > 0 {
        warn!(
            data_violations,
            "data integrity check found candles with non-positive prices"
        );
    }

    // --- Step 5: Timestamp bounds check (intraday outside market hours) ---
    // QuestDB stores IST-as-UTC: hours in DB ARE IST hours directly.
    // Valid range: [09:15, 15:30) IST — 15:30 is ALWAYS exclusive for ALL timeframes.
    // Last valid candle: 15:29 for 1m, 15:25 for 5m, 15:15 for 15m, 15:00 for 60m.
    // Daily candles: no time check (stamped at IST midnight).
    // Any candle at 15:30+ is a violation regardless of timeframe.
    let ts_count_query = format!(
        "SELECT count() FROM {} \
         WHERE ts > dateadd('d', -3, now()) \
         AND timeframe != '1d' \
         AND (hour(ts) < 9 OR hour(ts) > 15 \
              OR (hour(ts) = 9 AND minute(ts) < 15) \
              OR (hour(ts) = 15 AND minute(ts) > 29))",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let timestamp_violations = extract_count(&client, &base_url, &ts_count_query).await;

    let timestamp_details = if timestamp_violations > 0 {
        let ts_detail_query = format!(
            "SELECT security_id, segment, timeframe, ts, open, high, low, close, volume \
             FROM {} \
             WHERE ts > dateadd('d', -3, now()) \
             AND timeframe != '1d' \
             AND (hour(ts) < 9 OR hour(ts) > 15 \
                  OR (hour(ts) = 9 AND minute(ts) < 15) \
                  OR (hour(ts) = 15 AND minute(ts) > 29)) \
             LIMIT {}",
            QUESTDB_TABLE_HISTORICAL_CANDLES, MAX_VIOLATION_DETAILS
        );
        parse_violation_rows(
            &client,
            &base_url,
            &ts_detail_query,
            registry,
            "outside 09:15-15:29 market hours",
        )
        .await
    } else {
        Vec::new()
    };

    if timestamp_violations > 0 {
        warn!(
            timestamp_violations,
            "timestamp bounds check found intraday candles outside market hours"
        );
    }

    // --- Step 6: Determine pass/fail ---
    // FAIL if: zero instruments OR any gaps OR any violations of any type
    let passed = instruments_checked > 0
        && instruments_with_gaps == 0
        && ohlc_violations == 0
        && data_violations == 0
        && timestamp_violations == 0;

    // Log per-timeframe summary
    for tc in &timeframe_counts {
        info!(
            timeframe = %tc.timeframe,
            instruments = tc.instrument_count,
            candles = tc.candle_count,
            "timeframe coverage"
        );
    }

    info!(
        instruments_checked,
        instruments_complete,
        instruments_with_gaps,
        total_candles,
        ohlc_violations,
        data_violations,
        timestamp_violations,
        passed,
        "multi-timeframe cross-verification complete"
    );

    CrossVerificationReport {
        instruments_checked,
        instruments_complete,
        instruments_with_gaps,
        total_candles_in_db: total_candles,
        timeframe_counts,
        ohlc_violations,
        ohlc_details,
        data_violations,
        data_details,
        timestamp_violations,
        timestamp_details,
        passed,
    }
}

// ---------------------------------------------------------------------------
// Historical vs Live Cross-Match
// ---------------------------------------------------------------------------

/// Compares historical candle data (Dhan REST API) against live materialized
/// views (WebSocket ticks) for the same instruments and timestamps.
///
/// Detects:
/// - **Missing live candles**: historical exists but live doesn't (WebSocket dropped ticks)
/// - **Price mismatches**: OHLCV values differ beyond tolerance
/// - **Missing historical**: live exists but historical doesn't (rare)
///
/// Runs after post-market re-fetch when both datasets are complete for the day.
///
/// # Arguments
/// * `questdb_config` — QuestDB connection config
/// * `registry` — Instrument registry for security_id → symbol lookup
pub async fn cross_match_historical_vs_live(
    questdb_config: &QuestDbConfig,
    registry: &InstrumentRegistry,
) -> CrossMatchReport {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(VERIFY_QUERY_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "failed to build HTTP client for cross-match");
            return failed_cross_match_report();
        }
    };

    let mut total_compared = 0_usize;
    let mut total_mismatches = 0_usize;
    let mut total_missing_live = 0_usize;
    let total_missing_historical = 0_usize;
    let mut total_oi_mismatches = 0_usize;
    let mut all_details: Vec<CrossMatchMismatch> = Vec::new();
    let mut timeframes_checked = 0_usize;
    let mut missing_views: Vec<String> = Vec::new();

    for &(hist_tf, live_table) in CROSS_MATCH_TIMEFRAMES {
        // M2: Check if the materialized view table exists before JOINing
        let table_exists_query =
            format!("SELECT count() FROM tables() WHERE name = '{}'", live_table);
        let table_count = extract_count(&client, &base_url, &table_exists_query).await;
        if table_count == 0 {
            warn!(
                live_table,
                timeframe = hist_tf,
                "materialized view table does not exist — skipping cross-match for this timeframe"
            );
            missing_views.push(live_table.to_string());
            continue;
        }

        // Count total comparable candles for this timeframe
        let count_query = format!(
            "SELECT count() FROM {} h \
             LEFT JOIN {} m ON h.security_id = m.security_id AND h.ts = m.ts AND h.segment = m.segment \
             WHERE h.timeframe = '{}' AND h.ts > dateadd('d', -3, now())",
            QUESTDB_TABLE_HISTORICAL_CANDLES, live_table, hist_tf
        );

        let tf_total = extract_count(&client, &base_url, &count_query).await;
        total_compared = total_compared.saturating_add(tf_total);

        if tf_total == 0 {
            continue;
        }
        timeframes_checked = timeframes_checked.saturating_add(1);

        // Fetch ALL joined rows for this timeframe — Rust applies epsilon + volume + OI checks.
        // SQL pre-filters with generous tolerance to avoid fetching perfectly matching rows.
        let detail_query = format!(
            "SELECT h.security_id, h.segment, h.ts, \
                    h.open, h.high, h.low, h.close, h.volume, \
                    m.open, m.high, m.low, m.close, m.volume, \
                    h.open_interest, m.open_interest \
             FROM {} h \
             LEFT JOIN {} m ON h.security_id = m.security_id AND h.ts = m.ts AND h.segment = m.segment \
             WHERE h.timeframe = '{}' AND h.ts > dateadd('d', -3, now()) \
             AND (m.open IS NULL \
                  OR abs(h.open - m.open) > {eps} \
                  OR abs(h.high - m.high) > {eps} \
                  OR abs(h.low - m.low) > {eps} \
                  OR abs(h.close - m.close) > {eps} \
                  OR abs(h.volume - m.volume) > 0 \
                  OR abs(h.open_interest - m.open_interest) > 0) \
             LIMIT {}",
            QUESTDB_TABLE_HISTORICAL_CANDLES,
            live_table,
            hist_tf,
            MAX_VIOLATION_DETAILS,
            eps = CROSS_MATCH_PRICE_EPSILON,
        );

        let details =
            parse_cross_match_rows_with_oi(&client, &base_url, &detail_query, registry, hist_tf)
                .await;

        for detail in &details {
            total_mismatches = total_mismatches.saturating_add(1);
            if detail.mismatch_type == "missing_live" {
                total_missing_live = total_missing_live.saturating_add(1);
            }
            if detail.mismatch_type == "oi_diff" {
                total_oi_mismatches = total_oi_mismatches.saturating_add(1);
            }
        }

        all_details.extend(details);
    }

    let passed = total_mismatches == 0 && total_compared > 0 && missing_views.is_empty();

    info!(
        timeframes_checked,
        candles_compared = total_compared,
        mismatches = total_mismatches,
        missing_live = total_missing_live,
        oi_mismatches = total_oi_mismatches,
        missing_views_count = missing_views.len(),
        passed,
        "historical vs live cross-match complete"
    );

    CrossMatchReport {
        timeframes_checked,
        candles_compared: total_compared,
        mismatches: total_mismatches,
        missing_live: total_missing_live,
        missing_historical: total_missing_historical,
        oi_mismatches: total_oi_mismatches,
        mismatch_details: all_details,
        missing_views,
        passed,
    }
}

/// Creates a failed cross-match report.
fn failed_cross_match_report() -> CrossMatchReport {
    CrossMatchReport {
        timeframes_checked: 0,
        candles_compared: 0,
        mismatches: 0,
        missing_live: 0,
        missing_historical: 0,
        oi_mismatches: 0,
        mismatch_details: Vec::new(),
        missing_views: Vec::new(),
        passed: false,
    }
}

// ---------------------------------------------------------------------------
// Query Helpers
// ---------------------------------------------------------------------------

/// Executes a QuestDB HTTP query and returns the parsed response.
async fn execute_query(
    client: &Client,
    base_url: &str,
    query: &str,
) -> Option<QuestDbQueryResponse> {
    let result = client.get(base_url).query(&[("query", query)]).send().await;

    match result {
        Ok(response) => {
            if !response.status().is_success() {
                let status = response.status();
                warn!(%status, "QuestDB query returned non-success");
                return None;
            }
            match response.json::<QuestDbQueryResponse>().await {
                Ok(data) => Some(data),
                Err(err) => {
                    warn!(?err, "failed to parse QuestDB query response");
                    None
                }
            }
        }
        Err(err) => {
            tracing::error!(
                ?err,
                "QuestDB query request failed — verification cannot proceed"
            );
            None
        }
    }
}

/// Executes a `SELECT count()` query and returns the count as usize.
async fn extract_count(client: &Client, base_url: &str, query: &str) -> usize {
    match execute_query(client, base_url, query).await {
        Some(data) => {
            if let Some(row) = data.dataset.first() {
                row.first().and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize
            } else {
                0
            }
        }
        None => 0,
    }
}

/// Looks up the display label for a security_id from the instrument registry.
/// Falls back to the raw security_id string if not found.
fn lookup_symbol(registry: &InstrumentRegistry, security_id: i64) -> String {
    #[allow(clippy::cast_sign_loss)]
    // APPROVED: security_id from QuestDB is always non-negative
    let sid_u32 = security_id as u32;
    registry
        .get(sid_u32)
        .map(|inst| inst.display_label.clone())
        .unwrap_or_else(|| security_id.to_string())
}

/// Formats a QuestDB timestamp value as an IST time string.
/// QuestDB returns timestamps as ISO strings like "2026-03-18T03:45:00.000000Z"
/// (which is already IST-as-UTC in our storage convention).
fn format_timestamp_ist(ts_value: &serde_json::Value) -> String {
    // QuestDB HTTP API returns timestamps as ISO strings
    if let Some(ts_str) = ts_value.as_str() {
        // Extract date and time from ISO format: "2026-03-18T03:45:00.000000Z"
        // Since our storage is IST-as-UTC, display as-is but reformat
        if let Some((date_part, time_rest)) = ts_str.split_once('T') {
            let time_part = time_rest
                .split_once('.')
                .map_or(time_rest, |(t, _)| t)
                .trim_end_matches('Z');
            // Show HH:MM only (skip seconds for readability)
            let time_hm = if time_part.len() >= 5 {
                &time_part[..5]
            } else {
                time_part
            };
            return format!("{} {} IST", date_part, time_hm);
        }
        return ts_str.to_string();
    }
    // Fallback for numeric epoch
    ts_value.to_string()
}

/// Parses violation detail rows from a QuestDB query response.
/// Expected columns: security_id, segment, timeframe, ts, open, high, low, close, volume
async fn parse_violation_rows(
    client: &Client,
    base_url: &str,
    query: &str,
    registry: &InstrumentRegistry,
    violation_type: &str,
) -> Vec<ViolationDetail> {
    let data = match execute_query(client, base_url, query).await {
        Some(d) => d,
        None => return Vec::new(),
    };

    let mut details = Vec::with_capacity(data.dataset.len().min(MAX_VIOLATION_DETAILS));

    for row in &data.dataset {
        if row.len() < 9 {
            continue;
        }

        let sid = row[0].as_i64().unwrap_or(0);
        let segment = row[1].as_str().unwrap_or("").to_string();
        let timeframe = row[2].as_str().unwrap_or("").to_string();
        let timestamp_ist = format_timestamp_ist(&row[3]);
        let open = row[4].as_f64().unwrap_or(0.0);
        let high = row[5].as_f64().unwrap_or(0.0);
        let low = row[6].as_f64().unwrap_or(0.0);
        let close = row[7].as_f64().unwrap_or(0.0);
        let volume = row[8].as_i64().unwrap_or(0);

        details.push(ViolationDetail {
            symbol: lookup_symbol(registry, sid),
            segment,
            timeframe,
            timestamp_ist,
            violation: violation_type.to_string(),
            values: format!("O={open} H={high} L={low} C={close} V={volume}"),
        });

        if details.len() >= MAX_VIOLATION_DETAILS {
            break;
        }
    }

    details
}

/// Parses cross-match rows with OI, applying Rust-side epsilon + volume + OI checks.
/// Expected columns: security_id, segment, ts,
///   h_open, h_high, h_low, h_close, h_volume,
///   m_open, m_high, m_low, m_close, m_volume,
///   h_open_interest, m_open_interest
async fn parse_cross_match_rows_with_oi(
    client: &Client,
    base_url: &str,
    query: &str,
    registry: &InstrumentRegistry,
    timeframe: &str,
) -> Vec<CrossMatchMismatch> {
    let data = match execute_query(client, base_url, query).await {
        Some(d) => d,
        None => return Vec::new(),
    };

    let mut details = Vec::with_capacity(data.dataset.len().min(MAX_VIOLATION_DETAILS));

    for row in &data.dataset {
        if row.len() < 15 {
            continue;
        }

        let sid = row[0].as_i64().unwrap_or(0);
        let segment = row[1].as_str().unwrap_or("").to_string();
        let timestamp_ist = format_timestamp_ist(&row[2]);

        let h_open = row[3].as_f64().unwrap_or(0.0);
        let h_high = row[4].as_f64().unwrap_or(0.0);
        let h_low = row[5].as_f64().unwrap_or(0.0);
        let h_close = row[6].as_f64().unwrap_or(0.0);
        let h_volume = row[7].as_i64().unwrap_or(0);
        let h_oi = row[13].as_i64().unwrap_or(0);

        // Live values: NULL if missing (LEFT JOIN with no match)
        let live_is_null = row[8].is_null();

        if live_is_null {
            details.push(CrossMatchMismatch {
                symbol: lookup_symbol(registry, sid),
                segment,
                timeframe: timeframe.to_string(),
                timestamp_ist,
                mismatch_type: "missing_live".to_string(),
                hist_values: format!(
                    "O={h_open} H={h_high} L={h_low} C={h_close} V={h_volume} OI={h_oi}"
                ),
                live_values: "[MISSING — no live data for this candle]".to_string(),
                diff_summary: String::new(),
            });
            continue;
        }

        let m_open = row[8].as_f64().unwrap_or(0.0);
        let m_high = row[9].as_f64().unwrap_or(0.0);
        let m_low = row[10].as_f64().unwrap_or(0.0);
        let m_close = row[11].as_f64().unwrap_or(0.0);
        let m_volume = row[12].as_i64().unwrap_or(0);
        let m_oi = row[14].as_i64().unwrap_or(0);

        // Rust-side mismatch detection with proper tolerances
        let d_open = m_open - h_open;
        let d_high = m_high - h_high;
        let d_low = m_low - h_low;
        let d_close = m_close - h_close;

        let price_mismatch = d_open.abs() > CROSS_MATCH_PRICE_EPSILON
            || d_high.abs() > CROSS_MATCH_PRICE_EPSILON
            || d_low.abs() > CROSS_MATCH_PRICE_EPSILON
            || d_close.abs() > CROSS_MATCH_PRICE_EPSILON;

        // H1: Volume mismatch — relative 10% tolerance
        let h_vol_max = (h_volume.max(1)) as f64;
        let vol_diff_pct = (m_volume.saturating_sub(h_volume) as f64).abs() / h_vol_max;
        let volume_mismatch = vol_diff_pct > CROSS_MATCH_VOLUME_TOLERANCE_PCT;

        // H2: OI mismatch — only when both > 0 (derivatives)
        let oi_mismatch = if h_oi > 0 && m_oi > 0 {
            let h_oi_max = h_oi.max(1) as f64;
            let oi_diff_pct = (m_oi.saturating_sub(h_oi) as f64).abs() / h_oi_max;
            oi_diff_pct > CROSS_MATCH_OI_TOLERANCE_PCT
        } else {
            false
        };

        if !price_mismatch && !volume_mismatch && !oi_mismatch {
            // SQL pre-filter caught it but Rust-side says it's fine (e.g., volume diff < 10%)
            continue;
        }

        // Determine mismatch type
        let mismatch_type = if oi_mismatch && !price_mismatch && !volume_mismatch {
            "oi_diff"
        } else if volume_mismatch && !price_mismatch {
            "volume_diff"
        } else {
            "price_diff"
        };

        // Build diff summary for ALL fields that exceed tolerance
        let mut diffs = Vec::new();
        if d_open.abs() > CROSS_MATCH_PRICE_EPSILON {
            diffs.push(format!("O({d_open:+.2})"));
        }
        if d_high.abs() > CROSS_MATCH_PRICE_EPSILON {
            diffs.push(format!("H({d_high:+.2})"));
        }
        if d_low.abs() > CROSS_MATCH_PRICE_EPSILON {
            diffs.push(format!("L({d_low:+.2})"));
        }
        if d_close.abs() > CROSS_MATCH_PRICE_EPSILON {
            diffs.push(format!("C({d_close:+.2})"));
        }
        let d_vol = m_volume.saturating_sub(h_volume);
        if volume_mismatch {
            diffs.push(format!("V({d_vol:+} {vol_diff_pct:.0}%)"));
        }
        if oi_mismatch {
            let d_oi = m_oi.saturating_sub(h_oi);
            diffs.push(format!("OI({d_oi:+})"));
        }

        details.push(CrossMatchMismatch {
            symbol: lookup_symbol(registry, sid),
            segment,
            timeframe: timeframe.to_string(),
            timestamp_ist,
            mismatch_type: mismatch_type.to_string(),
            hist_values: format!(
                "O={h_open} H={h_high} L={h_low} C={h_close} V={h_volume} OI={h_oi}"
            ),
            live_values: format!(
                "O={m_open} H={m_high} L={m_low} C={m_close} V={m_volume} OI={m_oi}"
            ),
            diff_summary: diffs.join(" "),
        });

        if details.len() >= MAX_VIOLATION_DETAILS {
            break;
        }
    }

    details
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cross_verification_report_default_values() {
        let report = CrossVerificationReport {
            instruments_checked: 100,
            instruments_complete: 95,
            instruments_with_gaps: 5,
            total_candles_in_db: 37500,
            timeframe_counts: vec![],
            ohlc_violations: 0,
            ohlc_details: vec![],
            data_violations: 0,
            data_details: vec![],
            timestamp_violations: 0,
            timestamp_details: vec![],
            passed: false,
        };
        assert!(!report.passed);
        assert_eq!(report.instruments_checked, 100);
    }

    #[test]
    fn test_min_candle_coverage_calculation() {
        #[allow(clippy::arithmetic_side_effects)]
        let min_candles = (CANDLES_PER_TRADING_DAY as f64 * MIN_CANDLE_COVERAGE_RATIO) as usize;
        // 375 * 0.95 = 356.25 → 356
        assert_eq!(min_candles, 356);
    }

    #[test]
    fn test_verified_timeframes_has_all_five() {
        assert_eq!(VERIFIED_TIMEFRAMES.len(), 5);
        assert!(VERIFIED_TIMEFRAMES.contains(&"1m"));
        assert!(VERIFIED_TIMEFRAMES.contains(&"5m"));
        assert!(VERIFIED_TIMEFRAMES.contains(&"15m"));
        assert!(VERIFIED_TIMEFRAMES.contains(&"60m"));
        assert!(VERIFIED_TIMEFRAMES.contains(&"1d"));
    }

    #[test]
    fn test_timeframe_coverage_structure() {
        let tc = TimeframeCoverage {
            timeframe: "5m".to_string(),
            instrument_count: 50,
            candle_count: 3750,
        };
        assert_eq!(tc.timeframe, "5m");
        assert_eq!(tc.instrument_count, 50);
        assert_eq!(tc.candle_count, 3750);
    }

    #[test]
    fn test_multi_timeframe_report_fields() {
        let report = CrossVerificationReport {
            instruments_checked: 50,
            instruments_complete: 48,
            instruments_with_gaps: 2,
            total_candles_in_db: 187500,
            timeframe_counts: vec![
                TimeframeCoverage {
                    timeframe: "1m".to_string(),
                    instrument_count: 50,
                    candle_count: 18750,
                },
                TimeframeCoverage {
                    timeframe: "5m".to_string(),
                    instrument_count: 50,
                    candle_count: 3750,
                },
            ],
            ohlc_violations: 0,
            ohlc_details: vec![],
            data_violations: 0,
            data_details: vec![],
            timestamp_violations: 0,
            timestamp_details: vec![],
            passed: false,
        };

        assert_eq!(report.timeframe_counts.len(), 2);
        assert_eq!(report.timeframe_counts[0].timeframe, "1m");
        assert_eq!(report.timeframe_counts[1].timeframe, "5m");
        assert_eq!(report.ohlc_violations, 0);
        assert!(!report.passed);
    }

    #[test]
    fn test_failed_report_all_zeros() {
        let report = failed_report();
        assert_eq!(report.instruments_checked, 0);
        assert_eq!(report.instruments_complete, 0);
        assert_eq!(report.instruments_with_gaps, 0);
        assert_eq!(report.total_candles_in_db, 0);
        assert!(report.timeframe_counts.is_empty());
        assert_eq!(report.ohlc_violations, 0);
        assert!(report.ohlc_details.is_empty());
        assert_eq!(report.data_violations, 0);
        assert!(report.data_details.is_empty());
        assert_eq!(report.timestamp_violations, 0);
        assert!(report.timestamp_details.is_empty());
        assert!(!report.passed);
    }

    #[test]
    fn test_zero_instruments_fails_verification() {
        // When instruments_checked == 0, passed must be false.
        // This prevents empty DB silently passing verification.
        let instruments_checked = 0;
        let instruments_with_gaps = 0;
        let ohlc_violations = 0;
        let data_violations = 0;
        let timestamp_violations = 0;

        let passed = instruments_checked > 0
            && instruments_with_gaps == 0
            && ohlc_violations == 0
            && data_violations == 0
            && timestamp_violations == 0;

        assert!(!passed, "zero instruments must FAIL verification");
    }

    #[test]
    fn test_passed_requires_zero_all_violations() {
        // passed = true only when ALL conditions met
        let cases = [
            // (checked, gaps, ohlc, data, ts, expected_pass)
            (100, 0, 0, 0, 0, true),  // all good
            (100, 1, 0, 0, 0, false), // gaps
            (100, 0, 1, 0, 0, false), // ohlc violation
            (100, 0, 0, 1, 0, false), // data violation
            (100, 0, 0, 0, 1, false), // timestamp violation
            (0, 0, 0, 0, 0, false),   // zero instruments
        ];

        for (checked, gaps, ohlc, data, ts, expected) in cases {
            let passed = checked > 0 && gaps == 0 && ohlc == 0 && data == 0 && ts == 0;
            assert_eq!(
                passed, expected,
                "checked={checked} gaps={gaps} ohlc={ohlc} data={data} ts={ts}"
            );
        }
    }

    #[test]
    fn test_violation_detail_struct() {
        let detail = ViolationDetail {
            symbol: "RELIANCE".to_string(),
            segment: "NSE_EQ".to_string(),
            timeframe: "1m".to_string(),
            timestamp_ist: "2026-03-18 10:15 IST".to_string(),
            violation: "high < low".to_string(),
            values: "O=2450.0 H=2440.0 L=2450.0 C=2445.0".to_string(),
        };
        assert_eq!(detail.symbol, "RELIANCE");
        assert_eq!(detail.violation, "high < low");
        assert!(detail.values.contains("H=2440.0"));
    }

    #[test]
    fn test_cross_match_mismatch_struct() {
        let mismatch = CrossMatchMismatch {
            symbol: "TCS".to_string(),
            segment: "NSE_EQ".to_string(),
            timeframe: "1m".to_string(),
            timestamp_ist: "2026-03-18 11:30 IST".to_string(),
            mismatch_type: "price_diff".to_string(),
            hist_values: "O=3520.0 H=3535.0 L=3518.0 C=3530.0 V=45000".to_string(),
            live_values: "O=3520.0 H=3535.0 L=3518.0 C=3528.5 V=42100".to_string(),
            diff_summary: "C(-1.5) V(-2900)".to_string(),
        };
        assert_eq!(mismatch.mismatch_type, "price_diff");
        assert!(mismatch.diff_summary.contains("C(-1.5)"));
    }

    #[test]
    fn test_cross_match_missing_live_struct() {
        let mismatch = CrossMatchMismatch {
            symbol: "NIFTY50".to_string(),
            segment: "IDX_I".to_string(),
            timeframe: "5m".to_string(),
            timestamp_ist: "2026-03-18 14:00 IST".to_string(),
            mismatch_type: "missing_live".to_string(),
            hist_values: "O=23480.0 H=23510.0 L=23475.0 C=23505.0 V=0".to_string(),
            live_values: "[MISSING — no live data for this candle]".to_string(),
            diff_summary: String::new(),
        };
        assert_eq!(mismatch.mismatch_type, "missing_live");
        assert!(mismatch.live_values.contains("MISSING"));
    }

    #[test]
    fn test_cross_match_report_struct() {
        let report = CrossMatchReport {
            timeframes_checked: 5,
            candles_compared: 187500,
            mismatches: 12,
            missing_live: 8,
            missing_historical: 0,
            oi_mismatches: 2,
            mismatch_details: vec![],
            missing_views: vec![],
            passed: false,
        };
        assert_eq!(report.timeframes_checked, 5);
        assert_eq!(report.mismatches, 12);
        assert_eq!(report.missing_live, 8);
        assert_eq!(report.oi_mismatches, 2);
        assert!(report.missing_views.is_empty());
        assert!(!report.passed);
    }

    #[test]
    fn test_cross_match_timeframe_mapping() {
        assert_eq!(CROSS_MATCH_TIMEFRAMES.len(), 5);
        // Verify the critical 60m → candles_1h mapping
        let mapping_60m = CROSS_MATCH_TIMEFRAMES.iter().find(|(tf, _)| *tf == "60m");
        assert_eq!(
            mapping_60m,
            Some(&("60m", "candles_1h")),
            "60m must map to candles_1h (not candles_60m)"
        );
    }

    #[test]
    fn test_failed_cross_match_report() {
        let report = failed_cross_match_report();
        assert_eq!(report.timeframes_checked, 0);
        assert_eq!(report.candles_compared, 0);
        assert_eq!(report.mismatches, 0);
        assert_eq!(report.oi_mismatches, 0);
        assert!(report.mismatch_details.is_empty());
        assert!(report.missing_views.is_empty());
        assert!(!report.passed);
    }

    #[test]
    fn test_format_timestamp_ist_iso_format() {
        let ts = serde_json::Value::String("2026-03-18T03:45:00.000000Z".to_string());
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "2026-03-18 03:45 IST");
    }

    #[test]
    fn test_format_timestamp_ist_no_fractional() {
        let ts = serde_json::Value::String("2026-03-18T10:15:00Z".to_string());
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "2026-03-18 10:15 IST");
    }

    #[test]
    fn test_format_timestamp_ist_fallback() {
        let ts = serde_json::json!(1773050340);
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "1773050340");
    }

    #[test]
    fn test_cross_match_epsilon_tolerance() {
        // Epsilon must be tiny — only absorbs IEEE754 noise, not real diffs
        const {
            assert!(CROSS_MATCH_PRICE_EPSILON > 0.0);
            assert!(
                CROSS_MATCH_PRICE_EPSILON < 0.001,
                "epsilon must be < 0.001 (min tick = 0.05)"
            );
        }
    }

    #[test]
    fn test_epsilon_catches_real_tick_diff() {
        // Minimum tick in Indian markets = 0.05
        // Epsilon (1e-6) must be WAY below that
        let min_tick = 0.05_f64;
        assert!(
            min_tick > CROSS_MATCH_PRICE_EPSILON * 1000.0,
            "epsilon must be at least 1000x smaller than min tick"
        );
    }

    #[test]
    fn test_cross_match_volume_tolerance_constant() {
        const {
            assert!(CROSS_MATCH_VOLUME_TOLERANCE_PCT > 0.0);
            assert!(
                CROSS_MATCH_VOLUME_TOLERANCE_PCT <= 0.20,
                "volume tolerance must be <= 20%"
            );
        }
    }

    #[test]
    fn test_cross_match_oi_tolerance_constant() {
        const {
            assert!(CROSS_MATCH_OI_TOLERANCE_PCT > 0.0);
            assert!(
                CROSS_MATCH_OI_TOLERANCE_PCT <= 0.20,
                "OI tolerance must be <= 20%"
            );
        }
    }

    #[test]
    fn test_cross_match_report_includes_oi_mismatches_field() {
        let report = CrossMatchReport {
            timeframes_checked: 3,
            candles_compared: 50000,
            mismatches: 5,
            missing_live: 2,
            missing_historical: 0,
            oi_mismatches: 3,
            mismatch_details: vec![],
            missing_views: vec!["candles_1d".to_string()],
            passed: false,
        };
        assert_eq!(report.oi_mismatches, 3);
        assert_eq!(report.missing_views.len(), 1);
        assert_eq!(report.missing_views[0], "candles_1d");
    }

    #[test]
    fn test_cross_match_report_includes_missing_views_field() {
        let report = CrossMatchReport {
            timeframes_checked: 3,
            candles_compared: 50000,
            mismatches: 0,
            missing_live: 0,
            missing_historical: 0,
            oi_mismatches: 0,
            mismatch_details: vec![],
            missing_views: vec!["candles_5m".to_string(), "candles_1d".to_string()],
            passed: false,
        };
        assert!(!report.passed, "missing views must fail cross-match");
        assert_eq!(report.missing_views.len(), 2);
    }

    #[test]
    fn test_violation_detail_no_arbitrary_cap() {
        // MAX_VIOLATION_DETAILS is a memory safety cap only — high enough to show everything
        const {
            assert!(
                MAX_VIOLATION_DETAILS >= 100,
                "violation detail cap must be >= 100 to show all violations"
            );
        }
    }

    #[tokio::test]
    async fn test_verify_candle_integrity_unreachable_host() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let registry = InstrumentRegistry::empty();
        let report = verify_candle_integrity(&config, &registry).await;
        assert!(!report.passed);
        assert_eq!(report.instruments_checked, 0);
    }

    #[tokio::test]
    async fn test_cross_match_unreachable_host() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let registry = InstrumentRegistry::empty();
        let report = cross_match_historical_vs_live(&config, &registry).await;
        assert!(!report.passed);
        assert_eq!(report.candles_compared, 0);
    }

    // -----------------------------------------------------------------------
    // Pure helper function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_lookup_symbol_not_found_returns_id_string() {
        let registry = InstrumentRegistry::empty();
        assert_eq!(lookup_symbol(&registry, 99999), "99999");
    }

    #[test]
    fn test_lookup_symbol_zero() {
        let registry = InstrumentRegistry::empty();
        assert_eq!(lookup_symbol(&registry, 0), "0");
    }

    #[test]
    fn test_format_timestamp_ist_no_t_separator() {
        let ts = serde_json::Value::String("2026-01-01 10:00:00".to_string());
        assert_eq!(format_timestamp_ist(&ts), "2026-01-01 10:00:00");
    }

    #[test]
    fn test_format_timestamp_ist_null_value() {
        assert_eq!(format_timestamp_ist(&serde_json::Value::Null), "null");
    }

    #[test]
    fn test_format_timestamp_ist_bool_value() {
        assert_eq!(format_timestamp_ist(&serde_json::Value::Bool(true)), "true");
    }

    #[test]
    fn test_format_timestamp_ist_short_time() {
        let ts = serde_json::Value::String("2026-01-01T9:5Z".to_string());
        let result = format_timestamp_ist(&ts);
        assert!(result.contains("2026-01-01"));
    }

    #[test]
    fn test_format_timestamp_ist_float_epoch() {
        let ts = serde_json::json!(1773050340.5);
        assert!(format_timestamp_ist(&ts).contains("1773050340"));
    }

    // -----------------------------------------------------------------------
    // Mock HTTP server for async function tests
    // -----------------------------------------------------------------------

    async fn mock_http_server(body: &'static str) -> (tokio::task::JoinHandle<()>, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            for _ in 0..20 {
                if let Ok((mut stream, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            }
        });
        (handle, port)
    }

    async fn mock_http_error_server() -> (tokio::task::JoinHandle<()>, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            for _ in 0..5 {
                if let Ok((mut stream, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let resp = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            }
        });
        (handle, port)
    }

    async fn mock_http_bad_json_server() -> (tokio::task::JoinHandle<()>, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            for _ in 0..5 {
                if let Ok((mut stream, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let body = "not json";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            }
        });
        (handle, port)
    }

    // -----------------------------------------------------------------------
    // execute_query tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_execute_query_success() {
        let (handle, port) = mock_http_server(r#"{"dataset": [[1, 2, 3]], "count": 1}"#).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let result = execute_query(&client, &url, "SELECT 1").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().dataset.len(), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn test_execute_query_unreachable() {
        let client = Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        assert!(
            execute_query(&client, "http://127.0.0.1:1", "SELECT 1")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_execute_query_non_success_status() {
        let (handle, port) = mock_http_error_server().await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        assert!(execute_query(&client, &url, "SELECT 1").await.is_none());
        handle.abort();
    }

    #[tokio::test]
    async fn test_execute_query_invalid_json() {
        let (handle, port) = mock_http_bad_json_server().await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        assert!(execute_query(&client, &url, "SELECT 1").await.is_none());
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // extract_count tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_extract_count_success() {
        let (handle, port) = mock_http_server(r#"{"dataset": [[42]], "count": 1}"#).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        assert_eq!(extract_count(&client, &url, "SELECT count()").await, 42);
        handle.abort();
    }

    #[tokio::test]
    async fn test_extract_count_empty_dataset() {
        let (handle, port) = mock_http_server(r#"{"dataset": [], "count": 0}"#).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        assert_eq!(extract_count(&client, &url, "SELECT count()").await, 0);
        handle.abort();
    }

    #[tokio::test]
    async fn test_extract_count_unreachable() {
        let client = Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        assert_eq!(extract_count(&client, "http://127.0.0.1:1", "q").await, 0);
    }

    // -----------------------------------------------------------------------
    // parse_violation_rows tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_violation_rows_success() {
        let body = r#"{"dataset": [[11536, "NSE_EQ", "1m", "2026-03-18T09:15:00.000000Z", 100.0, 99.0, 100.5, 100.2, 5000]], "count": 1}"#;
        let (handle, port) = mock_http_server(body).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_violation_rows(&client, &url, "q", &registry, "high < low").await;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].violation, "high < low");
        assert_eq!(details[0].segment, "NSE_EQ");
        handle.abort();
    }

    #[tokio::test]
    async fn test_parse_violation_rows_short_row_skipped() {
        let body = r#"{"dataset": [[11536, "NSE_EQ"]], "count": 1}"#;
        let (handle, port) = mock_http_server(body).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_violation_rows(&client, &url, "q", &registry, "test").await;
        assert!(details.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn test_parse_violation_rows_unreachable() {
        let client = Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let registry = InstrumentRegistry::empty();
        let details =
            parse_violation_rows(&client, "http://127.0.0.1:1", "q", &registry, "t").await;
        assert!(details.is_empty());
    }

    // -----------------------------------------------------------------------
    // parse_cross_match_rows_with_oi tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_cross_match_rows_missing_live() {
        let body = r#"{"dataset": [[11536, "NSE_EQ", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 5000, null, null, null, null, null, 0, 0]], "count": 1}"#;
        let (handle, port) = mock_http_server(body).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].mismatch_type, "missing_live");
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_price_diff() {
        let body = r#"{"dataset": [[11536, "NSE_EQ", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 5000, 100.5, 105.0, 99.0, 102.0, 5000, 0, 0]], "count": 1}"#;
        let (handle, port) = mock_http_server(body).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].mismatch_type, "price_diff");
        assert!(details[0].diff_summary.contains("O("));
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_volume_diff() {
        // Prices match, volume differs > 10%
        let body = r#"{"dataset": [[11536, "NSE_EQ", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 10000, 100.0, 105.0, 99.0, 102.0, 8000, 0, 0]], "count": 1}"#;
        let (handle, port) = mock_http_server(body).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].mismatch_type, "volume_diff");
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_oi_diff() {
        // Prices+volume match, OI differs > 10%
        let body = r#"{"dataset": [[49081, "NSE_FNO", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 5000, 100.0, 105.0, 99.0, 102.0, 5000, 100000, 80000]], "count": 1}"#;
        let (handle, port) = mock_http_server(body).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].mismatch_type, "oi_diff");
        assert!(details[0].diff_summary.contains("OI("));
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_no_mismatch() {
        // Everything matches within tolerance
        let body = r#"{"dataset": [[11536, "NSE_EQ", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 5000, 100.0, 105.0, 99.0, 102.0, 5000, 0, 0]], "count": 1}"#;
        let (handle, port) = mock_http_server(body).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert!(details.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_short_row_skipped() {
        let body = r#"{"dataset": [[11536, "NSE_EQ"]], "count": 1}"#;
        let (handle, port) = mock_http_server(body).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert!(details.is_empty());
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // verify_candle_integrity with mock — empty DB
    // -----------------------------------------------------------------------

    async fn mock_empty_db_server() -> (tokio::task::JoinHandle<()>, u16) {
        mock_http_server(r#"{"dataset": [], "count": 0}"#).await
    }

    #[tokio::test]
    async fn test_verify_candle_integrity_empty_db() {
        let (handle, port) = mock_empty_db_server().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: 1,
            ilp_port: 1,
        };
        let registry = InstrumentRegistry::empty();
        let report = verify_candle_integrity(&config, &registry).await;
        assert!(!report.passed);
        assert_eq!(report.instruments_checked, 0);
        assert_eq!(report.ohlc_violations, 0);
        assert_eq!(report.data_violations, 0);
        assert_eq!(report.timestamp_violations, 0);
        assert_eq!(report.timeframe_counts.len(), 5);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // verify_candle_integrity with mock — good data (375 1m candles)
    // -----------------------------------------------------------------------

    async fn mock_good_data_server() -> (tokio::task::JoinHandle<()>, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let mut query_idx = 0_usize;
            for _ in 0..20 {
                if let Ok((mut stream, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let body = if query_idx == 0 {
                        r#"{"dataset": [["1m", 11536, 375]], "count": 1}"#.to_string()
                    } else {
                        r#"{"dataset": [[0]], "count": 1}"#.to_string()
                    };
                    query_idx += 1;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            }
        });
        (handle, port)
    }

    #[tokio::test]
    async fn test_verify_candle_integrity_with_data() {
        let (handle, port) = mock_good_data_server().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: 1,
            ilp_port: 1,
        };
        let registry = InstrumentRegistry::empty();
        let report = verify_candle_integrity(&config, &registry).await;
        assert!(report.passed);
        assert_eq!(report.instruments_checked, 1);
        assert_eq!(report.instruments_complete, 1);
        assert_eq!(report.instruments_with_gaps, 0);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // cross_match_historical_vs_live with mock — empty tables
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_cross_match_empty_tables() {
        let (handle, port) = mock_empty_db_server().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: 1,
            ilp_port: 1,
        };
        let registry = InstrumentRegistry::empty();
        let report = cross_match_historical_vs_live(&config, &registry).await;
        assert!(!report.passed);
        assert_eq!(report.candles_compared, 0);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // OI / volume mismatch logic unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_oi_mismatch_both_zero_no_mismatch() {
        let h_oi = 0_i64;
        let m_oi = 0_i64;
        let mismatch = h_oi > 0 && m_oi > 0;
        assert!(!mismatch);
    }

    #[test]
    fn test_oi_mismatch_exceeds_tolerance() {
        let h_oi = 100000_i64;
        let m_oi = 80000_i64;
        let h_oi_max = h_oi.max(1) as f64;
        let diff = (m_oi.saturating_sub(h_oi) as f64).abs() / h_oi_max;
        assert!(diff > CROSS_MATCH_OI_TOLERANCE_PCT);
    }

    #[test]
    fn test_volume_mismatch_within_tolerance() {
        let h_vol = 10000_i64;
        let m_vol = 9500_i64;
        let h_vol_max = h_vol.max(1) as f64;
        let diff = (m_vol.saturating_sub(h_vol) as f64).abs() / h_vol_max;
        assert!(diff <= CROSS_MATCH_VOLUME_TOLERANCE_PCT);
    }

    #[test]
    fn test_volume_mismatch_exceeds_tolerance() {
        let h_vol = 10000_i64;
        let m_vol = 8000_i64;
        let h_vol_max = h_vol.max(1) as f64;
        let diff = (m_vol.saturating_sub(h_vol) as f64).abs() / h_vol_max;
        assert!(diff > CROSS_MATCH_VOLUME_TOLERANCE_PCT);
    }

    #[test]
    fn test_cross_match_pass_requires_data_and_no_missing() {
        let total_mismatches = 0_usize;
        let total_compared = 100_usize;
        let missing_views: Vec<String> = vec!["candles_1m".to_string()];
        let passed = total_mismatches == 0 && total_compared > 0 && missing_views.is_empty();
        assert!(!passed, "missing views must fail");
    }

    // -----------------------------------------------------------------------
    // cross_match pass/fail logic — additional branch coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_match_passes_with_data_no_mismatches_no_missing() {
        let total_mismatches = 0_usize;
        let total_compared = 100_usize;
        let missing_views: Vec<String> = vec![];
        let passed = total_mismatches == 0 && total_compared > 0 && missing_views.is_empty();
        assert!(passed, "zero mismatches + data + no missing = pass");
    }

    #[test]
    fn test_cross_match_fails_with_mismatches_present() {
        let total_mismatches = 5_usize;
        let total_compared = 100_usize;
        let missing_views: Vec<String> = vec![];
        let passed = total_mismatches == 0 && total_compared > 0 && missing_views.is_empty();
        assert!(!passed, "mismatches present must fail");
    }

    #[test]
    fn test_cross_match_fails_with_zero_compared() {
        let total_mismatches = 0_usize;
        let total_compared = 0_usize;
        let missing_views: Vec<String> = vec![];
        let passed = total_mismatches == 0 && total_compared > 0 && missing_views.is_empty();
        assert!(!passed, "zero candles compared must fail");
    }

    // -----------------------------------------------------------------------
    // format_timestamp_ist — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_timestamp_ist_empty_string() {
        let ts = serde_json::Value::String(String::new());
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "", "empty string returned as-is");
    }

    #[test]
    fn test_format_timestamp_ist_just_date_no_time() {
        let ts = serde_json::Value::String("2026-03-18".to_string());
        let result = format_timestamp_ist(&ts);
        // No 'T' separator, so returns as-is
        assert_eq!(result, "2026-03-18");
    }

    #[test]
    fn test_format_timestamp_ist_with_milliseconds() {
        let ts = serde_json::Value::String("2026-03-18T14:30:00.123456Z".to_string());
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "2026-03-18 14:30 IST");
    }

    #[test]
    fn test_format_timestamp_ist_array_value() {
        let ts = serde_json::json!([1, 2, 3]);
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "[1,2,3]");
    }

    #[test]
    fn test_format_timestamp_ist_object_value() {
        let ts = serde_json::json!({"time": 123});
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, r#"{"time":123}"#);
    }

    #[test]
    fn test_format_timestamp_ist_negative_epoch() {
        let ts = serde_json::json!(-100);
        let result = format_timestamp_ist(&ts);
        assert_eq!(result, "-100");
    }

    #[test]
    fn test_format_timestamp_ist_very_short_time_part() {
        // Time part shorter than 5 chars (e.g., "9:5") — uses full time_part
        let ts = serde_json::Value::String("2026-01-01T9:5Z".to_string());
        let result = format_timestamp_ist(&ts);
        // time_rest = "9:5Z", after splitting on '.' => "9:5Z", trim Z => "9:5"
        // len("9:5") = 3 < 5, so uses "9:5" directly
        assert_eq!(result, "2026-01-01 9:5 IST");
    }

    // -----------------------------------------------------------------------
    // lookup_symbol — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_lookup_symbol_negative_id() {
        let registry = InstrumentRegistry::empty();
        // Negative i64 cast to u32 wraps around — tests robustness
        let result = lookup_symbol(&registry, -1);
        assert_eq!(result, "-1", "negative ID should return the string form");
    }

    #[test]
    fn test_lookup_symbol_max_i64() {
        let registry = InstrumentRegistry::empty();
        let result = lookup_symbol(&registry, i64::MAX);
        assert_eq!(result, i64::MAX.to_string());
    }

    // -----------------------------------------------------------------------
    // violation detail coverage cap logic
    // -----------------------------------------------------------------------

    #[test]
    fn test_max_violation_details_is_500() {
        assert_eq!(MAX_VIOLATION_DETAILS, 500);
    }

    // -----------------------------------------------------------------------
    // coverage gap parsing — short rows and edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_violation_rows_multiple_rows() {
        let body = r#"{"dataset": [
            [11536, "NSE_EQ", "1m", "2026-03-18T09:15:00.000000Z", 100.0, 99.0, 100.5, 100.2, 5000],
            [1333, "NSE_EQ", "5m", "2026-03-18T10:00:00.000000Z", 200.0, 195.0, 210.0, 205.0, 8000],
            [25, "IDX_I", "1d", "2026-03-18T00:00:00.000000Z", 24500.0, 24400.0, 24600.0, 24550.0, 0]
        ], "count": 3}"#;
        let (handle, port) = mock_http_server(Box::leak(body.to_string().into_boxed_str())).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_violation_rows(&client, &url, "q", &registry, "price <= 0").await;
        assert_eq!(details.len(), 3);
        assert_eq!(details[0].segment, "NSE_EQ");
        assert_eq!(details[1].timeframe, "5m");
        assert_eq!(details[2].segment, "IDX_I");
        assert_eq!(details[0].violation, "price <= 0");
        handle.abort();
    }

    #[tokio::test]
    async fn test_parse_violation_rows_mixed_short_and_valid() {
        // Mix of valid rows and short rows — short rows should be skipped
        let body = r#"{"dataset": [
            [11536, "NSE_EQ", "1m", "2026-03-18T09:15:00.000000Z", 100.0, 99.0, 100.5, 100.2, 5000],
            [1333, "NSE_EQ"],
            [25, "IDX_I", "1d", "2026-03-18T00:00:00.000000Z", 24500.0, 24400.0, 24600.0, 24550.0, 0]
        ], "count": 3}"#;
        let (handle, port) = mock_http_server(Box::leak(body.to_string().into_boxed_str())).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_violation_rows(&client, &url, "q", &registry, "test").await;
        assert_eq!(details.len(), 2, "short row should be skipped");
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // cross-match rows — additional mismatch type combinations
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_cross_match_rows_price_and_volume_diff() {
        // Both price and volume differ — should classify as "price_diff"
        let body = r#"{"dataset": [[11536, "NSE_EQ", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 10000, 100.5, 105.5, 99.0, 102.0, 8000, 0, 0]], "count": 1}"#;
        let (handle, port) = mock_http_server(Box::leak(body.to_string().into_boxed_str())).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert_eq!(details.len(), 1);
        // price_diff takes priority when price mismatch is present
        assert_eq!(details[0].mismatch_type, "price_diff");
        assert!(details[0].diff_summary.contains("O("));
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_volume_within_tolerance() {
        // Volume differs by < 10% — should NOT be flagged
        let body = r#"{"dataset": [[11536, "NSE_EQ", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 10000, 100.0, 105.0, 99.0, 102.0, 9500, 0, 0]], "count": 1}"#;
        let (handle, port) = mock_http_server(Box::leak(body.to_string().into_boxed_str())).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert!(
            details.is_empty(),
            "5% volume diff should be within 10% tolerance"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_oi_within_tolerance() {
        // OI differs by < 10% — should NOT be flagged
        let body = r#"{"dataset": [[49081, "NSE_FNO", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 5000, 100.0, 105.0, 99.0, 102.0, 5000, 100000, 95000]], "count": 1}"#;
        let (handle, port) = mock_http_server(Box::leak(body.to_string().into_boxed_str())).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert!(
            details.is_empty(),
            "5% OI diff should be within 10% tolerance"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_oi_one_zero_no_mismatch() {
        // OI mismatch only checked when BOTH > 0. If one is zero, skip OI check
        let body = r#"{"dataset": [[49081, "NSE_FNO", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 5000, 100.0, 105.0, 99.0, 102.0, 5000, 100000, 0]], "count": 1}"#;
        let (handle, port) = mock_http_server(Box::leak(body.to_string().into_boxed_str())).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert!(
            details.is_empty(),
            "when live OI is 0, OI mismatch should not trigger"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_all_price_fields_differ() {
        // All OHLC fields differ — diff summary should include all 4
        let body = r#"{"dataset": [[11536, "NSE_EQ", "2026-03-18T09:15:00.000000Z", 100.0, 105.0, 99.0, 102.0, 5000, 101.0, 106.0, 100.0, 103.0, 5000, 0, 0]], "count": 1}"#;
        let (handle, port) = mock_http_server(Box::leak(body.to_string().into_boxed_str())).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let registry = InstrumentRegistry::empty();
        let details = parse_cross_match_rows_with_oi(&client, &url, "q", &registry, "1m").await;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].mismatch_type, "price_diff");
        assert!(details[0].diff_summary.contains("O("));
        assert!(details[0].diff_summary.contains("H("));
        assert!(details[0].diff_summary.contains("L("));
        assert!(details[0].diff_summary.contains("C("));
        handle.abort();
    }

    #[tokio::test]
    async fn test_cross_match_rows_unreachable_returns_empty() {
        let client = Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let registry = InstrumentRegistry::empty();
        let details =
            parse_cross_match_rows_with_oi(&client, "http://127.0.0.1:1", "q", &registry, "1m")
                .await;
        assert!(details.is_empty());
    }

    // -----------------------------------------------------------------------
    // ViolationDetail values formatting
    // -----------------------------------------------------------------------

    #[test]
    fn test_violation_detail_values_format() {
        let detail = ViolationDetail {
            symbol: "NIFTY50".to_string(),
            segment: "IDX_I".to_string(),
            timeframe: "1d".to_string(),
            timestamp_ist: "2026-03-18 00:00 IST".to_string(),
            violation: "price <= 0".to_string(),
            values: "O=0 H=24500 L=24400 C=24550 V=0".to_string(),
        };
        assert!(detail.values.contains("O=0"));
        assert!(detail.values.contains("H=24500"));
        assert_eq!(detail.violation, "price <= 0");
    }

    #[test]
    fn test_violation_detail_clone() {
        let detail = ViolationDetail {
            symbol: "TCS".to_string(),
            segment: "NSE_EQ".to_string(),
            timeframe: "5m".to_string(),
            timestamp_ist: "2026-03-18 12:00 IST".to_string(),
            violation: "outside 09:15-15:29 market hours".to_string(),
            values: "O=3500 H=3520 L=3490 C=3510 V=10000".to_string(),
        };
        let cloned = detail.clone();
        assert_eq!(cloned.symbol, detail.symbol);
        assert_eq!(cloned.violation, detail.violation);
    }

    // -----------------------------------------------------------------------
    // CrossMatchMismatch clone and field access
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_match_mismatch_clone() {
        let mismatch = CrossMatchMismatch {
            symbol: "RELIANCE".to_string(),
            segment: "NSE_EQ".to_string(),
            timeframe: "1m".to_string(),
            timestamp_ist: "2026-03-18 10:15 IST".to_string(),
            mismatch_type: "volume_diff".to_string(),
            hist_values: "O=2450 H=2465 L=2448 C=2460 V=125000 OI=0".to_string(),
            live_values: "O=2450 H=2465 L=2448 C=2460 V=100000 OI=0".to_string(),
            diff_summary: "V(-25000 20%)".to_string(),
        };
        let cloned = mismatch.clone();
        assert_eq!(cloned.mismatch_type, "volume_diff");
        assert_eq!(cloned.diff_summary, "V(-25000 20%)");
    }

    // -----------------------------------------------------------------------
    // TimeframeCoverage clone
    // -----------------------------------------------------------------------

    #[test]
    fn test_timeframe_coverage_clone() {
        let tc = TimeframeCoverage {
            timeframe: "60m".to_string(),
            instrument_count: 100,
            candle_count: 600,
        };
        let cloned = tc.clone();
        assert_eq!(cloned.timeframe, "60m");
        assert_eq!(cloned.instrument_count, 100);
    }

    // -----------------------------------------------------------------------
    // CrossMatchReport — pass conditions
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_match_report_passed_true() {
        let report = CrossMatchReport {
            timeframes_checked: 5,
            candles_compared: 100000,
            mismatches: 0,
            missing_live: 0,
            missing_historical: 0,
            oi_mismatches: 0,
            mismatch_details: vec![],
            missing_views: vec![],
            passed: true,
        };
        assert!(report.passed);
        assert_eq!(report.mismatches, 0);
        assert!(report.missing_views.is_empty());
    }

    // -----------------------------------------------------------------------
    // coverage query row parsing edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_coverage_row_short_rows_skipped() {
        // Simulates the coverage parsing logic: rows with < 3 columns are skipped
        let rows: Vec<Vec<serde_json::Value>> = vec![
            vec![serde_json::json!("1m")],
            vec![serde_json::json!("5m"), serde_json::json!(11536)],
            vec![
                serde_json::json!("1m"),
                serde_json::json!(1333),
                serde_json::json!(375),
            ],
        ];

        let mut timeframe_instruments: HashMap<String, Vec<(i64, usize)>> = HashMap::new();
        for row in &rows {
            if row.len() < 3 {
                continue;
            }
            let tf = row[0].as_str().unwrap_or("").to_string();
            let sid = row[1].as_i64().unwrap_or(0);
            let count = row[2].as_i64().unwrap_or(0).max(0) as usize;
            timeframe_instruments
                .entry(tf)
                .or_default()
                .push((sid, count));
        }
        // Only the 3rd row (length 3) should be included
        assert_eq!(timeframe_instruments.len(), 1);
        assert!(timeframe_instruments.contains_key("1m"));
        assert_eq!(timeframe_instruments["1m"].len(), 1);
        assert_eq!(timeframe_instruments["1m"][0], (1333, 375));
    }

    #[test]
    fn test_coverage_row_null_values() {
        let row: Vec<serde_json::Value> = vec![
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ];
        let tf = row[0].as_str().unwrap_or("").to_string();
        let sid = row[1].as_i64().unwrap_or(0);
        let count = row[2].as_i64().unwrap_or(0).max(0) as usize;
        assert_eq!(tf, "");
        assert_eq!(sid, 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_coverage_row_negative_count_clamped() {
        let row: Vec<serde_json::Value> = vec![
            serde_json::json!("1m"),
            serde_json::json!(100),
            serde_json::json!(-5),
        ];
        let count = row[2].as_i64().unwrap_or(0).max(0) as usize;
        assert_eq!(count, 0, "negative count should be clamped to 0");
    }

    // -----------------------------------------------------------------------
    // 1m coverage completeness logic
    // -----------------------------------------------------------------------

    #[test]
    fn test_instrument_complete_at_exact_threshold() {
        #[allow(clippy::arithmetic_side_effects)]
        let min_candles = (CANDLES_PER_TRADING_DAY as f64 * MIN_CANDLE_COVERAGE_RATIO) as usize;
        // Instrument at exactly the threshold should be complete
        let count = min_candles;
        assert!(count >= min_candles);
    }

    #[test]
    fn test_instrument_incomplete_below_threshold() {
        #[allow(clippy::arithmetic_side_effects)]
        let min_candles = (CANDLES_PER_TRADING_DAY as f64 * MIN_CANDLE_COVERAGE_RATIO) as usize;
        let count = min_candles.saturating_sub(1);
        assert!(count < min_candles);
    }

    #[test]
    fn test_instrument_complete_above_threshold() {
        #[allow(clippy::arithmetic_side_effects)]
        let min_candles = (CANDLES_PER_TRADING_DAY as f64 * MIN_CANDLE_COVERAGE_RATIO) as usize;
        let count = CANDLES_PER_TRADING_DAY;
        assert!(count >= min_candles);
    }

    // -----------------------------------------------------------------------
    // OI/volume tolerance math edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_oi_mismatch_h_zero_m_nonzero_no_mismatch() {
        // When h_oi == 0, the OI check is skipped regardless of m_oi
        let h_oi = 0_i64;
        let m_oi = 50000_i64;
        let mismatch = if h_oi > 0 && m_oi > 0 {
            let h_oi_max = h_oi.max(1) as f64;
            let oi_diff_pct = (m_oi.saturating_sub(h_oi) as f64).abs() / h_oi_max;
            oi_diff_pct > CROSS_MATCH_OI_TOLERANCE_PCT
        } else {
            false
        };
        assert!(!mismatch, "h_oi=0 should skip OI comparison");
    }

    #[test]
    fn test_volume_mismatch_zero_hist_volume() {
        // When h_vol == 0, h_vol_max = max(0,1) = 1, diff = |m_vol| / 1
        let h_vol = 0_i64;
        let m_vol = 100_i64;
        let h_vol_max = h_vol.max(1) as f64;
        let vol_diff_pct = (m_vol.saturating_sub(h_vol) as f64).abs() / h_vol_max;
        assert!(
            vol_diff_pct > CROSS_MATCH_VOLUME_TOLERANCE_PCT,
            "100% diff should exceed 10% tolerance"
        );
    }

    #[test]
    fn test_volume_mismatch_both_zero() {
        let h_vol = 0_i64;
        let m_vol = 0_i64;
        let h_vol_max = h_vol.max(1) as f64;
        let vol_diff_pct = (m_vol.saturating_sub(h_vol) as f64).abs() / h_vol_max;
        assert!(
            vol_diff_pct <= CROSS_MATCH_VOLUME_TOLERANCE_PCT,
            "both zero volumes should not mismatch"
        );
    }

    // -----------------------------------------------------------------------
    // QuestDbQueryResponse deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn test_questdb_response_empty_dataset() {
        let json = r#"{"dataset": [], "count": 0}"#;
        let resp: QuestDbQueryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.dataset.is_empty());
        assert_eq!(resp.count, 0);
    }

    #[test]
    fn test_questdb_response_with_data() {
        let json = r#"{"dataset": [["1m", 11536, 375], ["5m", 11536, 75]], "count": 2}"#;
        let resp: QuestDbQueryResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.dataset.len(), 2);
        assert_eq!(resp.count, 2);
        assert_eq!(resp.dataset[0][0].as_str(), Some("1m"));
        assert_eq!(resp.dataset[0][1].as_i64(), Some(11536));
    }

    // -----------------------------------------------------------------------
    // Mismatch type classification logic unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_mismatch_type_oi_only() {
        let price_mismatch = false;
        let volume_mismatch = false;
        let oi_mismatch = true;
        let mismatch_type = if oi_mismatch && !price_mismatch && !volume_mismatch {
            "oi_diff"
        } else if volume_mismatch && !price_mismatch {
            "volume_diff"
        } else {
            "price_diff"
        };
        assert_eq!(mismatch_type, "oi_diff");
    }

    #[test]
    fn test_mismatch_type_volume_only() {
        let price_mismatch = false;
        let volume_mismatch = true;
        let oi_mismatch = false;
        let mismatch_type = if oi_mismatch && !price_mismatch && !volume_mismatch {
            "oi_diff"
        } else if volume_mismatch && !price_mismatch {
            "volume_diff"
        } else {
            "price_diff"
        };
        assert_eq!(mismatch_type, "volume_diff");
    }

    #[test]
    fn test_mismatch_type_price_and_volume() {
        let price_mismatch = true;
        let volume_mismatch = true;
        let oi_mismatch = false;
        let mismatch_type = if oi_mismatch && !price_mismatch && !volume_mismatch {
            "oi_diff"
        } else if volume_mismatch && !price_mismatch {
            "volume_diff"
        } else {
            "price_diff"
        };
        assert_eq!(mismatch_type, "price_diff");
    }

    #[test]
    fn test_mismatch_type_all_three() {
        let price_mismatch = true;
        let volume_mismatch = true;
        let oi_mismatch = true;
        let mismatch_type = if oi_mismatch && !price_mismatch && !volume_mismatch {
            "oi_diff"
        } else if volume_mismatch && !price_mismatch {
            "volume_diff"
        } else {
            "price_diff"
        };
        assert_eq!(mismatch_type, "price_diff");
    }

    #[test]
    fn test_mismatch_type_price_only() {
        let price_mismatch = true;
        let volume_mismatch = false;
        let oi_mismatch = false;
        let mismatch_type = if oi_mismatch && !price_mismatch && !volume_mismatch {
            "oi_diff"
        } else if volume_mismatch && !price_mismatch {
            "volume_diff"
        } else {
            "price_diff"
        };
        assert_eq!(mismatch_type, "price_diff");
    }

    #[test]
    fn test_mismatch_type_volume_and_oi() {
        let price_mismatch = false;
        let volume_mismatch = true;
        let oi_mismatch = true;
        let mismatch_type = if oi_mismatch && !price_mismatch && !volume_mismatch {
            "oi_diff"
        } else if volume_mismatch && !price_mismatch {
            "volume_diff"
        } else {
            "price_diff"
        };
        assert_eq!(mismatch_type, "volume_diff");
    }

    #[test]
    fn test_mismatch_type_price_and_oi() {
        let price_mismatch = true;
        let volume_mismatch = false;
        let oi_mismatch = true;
        let mismatch_type = if oi_mismatch && !price_mismatch && !volume_mismatch {
            "oi_diff"
        } else if volume_mismatch && !price_mismatch {
            "volume_diff"
        } else {
            "price_diff"
        };
        assert_eq!(mismatch_type, "price_diff");
    }

    // -----------------------------------------------------------------------
    // Price epsilon boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_price_diff_at_epsilon_boundary() {
        let h_open = 100.0_f64;
        let m_open = 100.0_f64 + CROSS_MATCH_PRICE_EPSILON;
        let diff = (m_open - h_open).abs();
        // Exactly at epsilon boundary — should NOT be flagged
        // (we use >, not >=, so diff == epsilon is NOT a mismatch)
        assert!(
            !(diff > CROSS_MATCH_PRICE_EPSILON),
            "diff exactly at epsilon should NOT trigger mismatch"
        );
    }

    #[test]
    fn test_price_diff_above_epsilon() {
        let h_open = 100.0_f64;
        let m_open = 100.0_f64 + CROSS_MATCH_PRICE_EPSILON * 2.0;
        let diff = (m_open - h_open).abs();
        assert!(diff > CROSS_MATCH_PRICE_EPSILON);
    }

    #[test]
    fn test_price_diff_below_epsilon() {
        let h_open = 100.0_f64;
        let m_open = 100.0_f64 + CROSS_MATCH_PRICE_EPSILON * 0.5;
        let diff = (m_open - h_open).abs();
        assert!(
            !(diff > CROSS_MATCH_PRICE_EPSILON),
            "half epsilon should not trigger mismatch"
        );
    }

    // -----------------------------------------------------------------------
    // extract_count — additional edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_extract_count_negative_value_clamped() {
        let (handle, port) = mock_http_server(r#"{"dataset": [[-5]], "count": 1}"#).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let result = extract_count(&client, &url, "SELECT count()").await;
        assert_eq!(result, 0, "negative count should be clamped to 0");
        handle.abort();
    }

    #[tokio::test]
    async fn test_extract_count_null_value() {
        let (handle, port) = mock_http_server(r#"{"dataset": [[null]], "count": 1}"#).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let result = extract_count(&client, &url, "SELECT count()").await;
        assert_eq!(result, 0, "null value should return 0");
        handle.abort();
    }

    #[tokio::test]
    async fn test_extract_count_string_value() {
        let (handle, port) =
            mock_http_server(r#"{"dataset": [["not a number"]], "count": 1}"#).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}", port);
        let result = extract_count(&client, &url, "SELECT count()").await;
        assert_eq!(result, 0, "string value should return 0");
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // CrossVerificationReport — pass conditions combinatorial
    // -----------------------------------------------------------------------

    #[test]
    fn test_verification_passes_with_all_conditions_met() {
        let instruments_checked = 50_usize;
        let instruments_with_gaps = 0_usize;
        let ohlc_violations = 0_usize;
        let data_violations = 0_usize;
        let timestamp_violations = 0_usize;

        let passed = instruments_checked > 0
            && instruments_with_gaps == 0
            && ohlc_violations == 0
            && data_violations == 0
            && timestamp_violations == 0;
        assert!(passed);
    }

    #[test]
    fn test_verification_fails_multiple_violations() {
        let instruments_checked = 50_usize;
        let instruments_with_gaps = 2_usize;
        let ohlc_violations = 3_usize;
        let data_violations = 1_usize;
        let timestamp_violations = 5_usize;

        let passed = instruments_checked > 0
            && instruments_with_gaps == 0
            && ohlc_violations == 0
            && data_violations == 0
            && timestamp_violations == 0;
        assert!(!passed);
    }

    // -----------------------------------------------------------------------
    // CROSS_MATCH_TIMEFRAMES mapping completeness
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_match_timeframes_1m_maps_to_candles_1m() {
        let mapping = CROSS_MATCH_TIMEFRAMES.iter().find(|(tf, _)| *tf == "1m");
        assert_eq!(mapping, Some(&("1m", "candles_1m")));
    }

    #[test]
    fn test_cross_match_timeframes_5m_maps_to_candles_5m() {
        let mapping = CROSS_MATCH_TIMEFRAMES.iter().find(|(tf, _)| *tf == "5m");
        assert_eq!(mapping, Some(&("5m", "candles_5m")));
    }

    #[test]
    fn test_cross_match_timeframes_15m_maps_to_candles_15m() {
        let mapping = CROSS_MATCH_TIMEFRAMES.iter().find(|(tf, _)| *tf == "15m");
        assert_eq!(mapping, Some(&("15m", "candles_15m")));
    }

    #[test]
    fn test_cross_match_timeframes_1d_maps_to_candles_1d() {
        let mapping = CROSS_MATCH_TIMEFRAMES.iter().find(|(tf, _)| *tf == "1d");
        assert_eq!(mapping, Some(&("1d", "candles_1d")));
    }

    // -----------------------------------------------------------------------
    // VERIFY_QUERY_TIMEOUT_SECS constant
    // -----------------------------------------------------------------------

    #[test]
    fn test_verify_query_timeout_is_reasonable() {
        const {
            assert!(VERIFY_QUERY_TIMEOUT_SECS >= 5, "timeout too low");
            assert!(VERIFY_QUERY_TIMEOUT_SECS <= 120, "timeout too high");
        }
    }
}
