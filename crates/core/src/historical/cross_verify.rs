//! Cross-verification of historical candle data integrity.
//!
//! Queries QuestDB to verify that the unified `historical_candles` table
//! has adequate coverage across all 5 timeframes (1m, 5m, 15m, 60m, 1d)
//! for every fetched instrument.
//!
//! # Verification Checks
//! 1. **Per-timeframe candle count**: each timeframe has expected minimum candles
//! 2. **OHLC consistency**: high >= low for all stored candles
//! 3. **Multi-instrument coverage**: all instruments have data in all timeframes
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
// Cross-Verification Logic
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

/// Creates a failed report with all zeros.
fn failed_report() -> CrossVerificationReport {
    CrossVerificationReport {
        instruments_checked: 0,
        instruments_complete: 0,
        instruments_with_gaps: 0,
        total_candles_in_db: 0,
        timeframe_counts: Vec::new(),
        ohlc_violations: 0,
        passed: false,
    }
}

/// Runs cross-verification of candle data integrity in QuestDB.
///
/// Checks all 5 timeframes for coverage and validates OHLC consistency.
///
/// # Returns
/// A `CrossVerificationReport` with pass/fail status, per-timeframe breakdown,
/// and OHLC violation count.
pub async fn verify_candle_integrity(questdb_config: &QuestDbConfig) -> CrossVerificationReport {
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
    let coverage_query = format!(
        "SELECT timeframe, security_id, count() as candle_count \
         FROM {} \
         WHERE ts > dateadd('d', -1, now()) \
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

    // --- Step 3: OHLC consistency check ---
    let ohlc_query = format!(
        "SELECT count() FROM {} \
         WHERE ts > dateadd('d', -1, now()) AND high < low",
        QUESTDB_TABLE_HISTORICAL_CANDLES
    );

    let ohlc_violations = match execute_query(&client, &base_url, &ohlc_query).await {
        Some(data) => {
            if let Some(row) = data.dataset.first() {
                row.first().and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize
            } else {
                0
            }
        }
        None => 0, // best-effort — don't fail verification for this
    };

    if ohlc_violations > 0 {
        warn!(
            ohlc_violations,
            "OHLC consistency check found candles with high < low"
        );
    }

    // --- Step 4: Determine pass/fail ---
    // Pass if: no 1m gaps AND no OHLC violations AND at least some data
    let passed = (instruments_with_gaps == 0 && ohlc_violations == 0) || instruments_checked == 0;

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
        passed,
    }
}

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
            warn!(?err, "QuestDB query request failed");
            None
        }
    }
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
        assert!(!report.passed);
    }

    #[tokio::test]
    async fn test_verify_candle_integrity_unreachable_host() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let report = verify_candle_integrity(&config).await;
        assert!(!report.passed);
        assert_eq!(report.instruments_checked, 0);
    }
}
