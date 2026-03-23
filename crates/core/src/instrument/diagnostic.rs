//! Instrument system diagnostic — end-to-end health check.
//!
//! Downloads the CSV, validates headers, parses rows, builds the universe,
//! runs validation, and reports detailed status for each step.
//! Reuses existing functions — no new HTTP or parsing logic.

use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::Serialize;
use tracing::info;

use dhan_live_trader_common::config::InstrumentConfig;
use dhan_live_trader_common::constants::*;
use dhan_live_trader_common::trading_calendar::ist_offset;

use super::csv_downloader::download_instrument_csv;
use super::csv_parser::parse_instrument_csv;
use super::instrument_loader::{is_instrument_fresh, is_within_build_window};
use super::universe_builder::build_fno_universe_from_csv;
use super::validation::validate_fno_universe;

// ---------------------------------------------------------------------------
// Report Types
// ---------------------------------------------------------------------------

/// Complete diagnostic report for the instrument system.
#[derive(Debug, Serialize)]
pub struct DiagnosticReport {
    /// Overall pass/fail.
    pub healthy: bool,
    /// Individual check results.
    pub checks: Vec<CheckResult>,
}

/// Result of a single diagnostic check.
#[derive(Debug, Serialize)]
pub struct CheckResult {
    /// Check name (e.g., "url_reachability", "csv_headers").
    pub name: String,
    /// Whether this check passed.
    pub passed: bool,
    /// Human-readable detail.
    pub detail: String,
    /// Duration of this check in milliseconds.
    pub duration_ms: u64,
}

/// All expected CSV column names for header validation.
const EXPECTED_COLUMNS: &[&str] = &[
    CSV_COLUMN_EXCH_ID,
    CSV_COLUMN_SEGMENT,
    CSV_COLUMN_SECURITY_ID,
    CSV_COLUMN_INSTRUMENT,
    CSV_COLUMN_UNDERLYING_SECURITY_ID,
    CSV_COLUMN_UNDERLYING_SYMBOL,
    CSV_COLUMN_SYMBOL_NAME,
    CSV_COLUMN_DISPLAY_NAME,
    CSV_COLUMN_SERIES,
    CSV_COLUMN_LOT_SIZE,
    CSV_COLUMN_EXPIRY_DATE,
    CSV_COLUMN_STRIKE_PRICE,
    CSV_COLUMN_OPTION_TYPE,
    CSV_COLUMN_TICK_SIZE,
    CSV_COLUMN_EXPIRY_FLAG,
];

// ---------------------------------------------------------------------------
// Pure Helper Functions (extracted for testability)
// ---------------------------------------------------------------------------

/// Segment breakdown counts from parsed instrument CSV rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentCounts {
    /// NSE Index instruments.
    pub nse_i: usize,
    /// NSE Equity instruments.
    pub nse_e: usize,
    /// NSE Derivative instruments.
    pub nse_d: usize,
    /// BSE Index instruments.
    pub bse_i: usize,
    /// BSE Derivative instruments.
    pub bse_d: usize,
}

/// Counts instruments by exchange + segment from parsed CSV rows.
fn count_segments(rows: &[super::csv_parser::ParsedInstrumentRow]) -> SegmentCounts {
    use dhan_live_trader_common::types::Exchange;

    let mut counts = SegmentCounts {
        nse_i: 0,
        nse_e: 0,
        nse_d: 0,
        bse_i: 0,
        bse_d: 0,
    };

    for r in rows {
        match (&r.exchange, r.segment) {
            (Exchange::NationalStockExchange, 'I') => {
                counts.nse_i = counts.nse_i.saturating_add(1);
            }
            (Exchange::NationalStockExchange, 'E') => {
                counts.nse_e = counts.nse_e.saturating_add(1);
            }
            (Exchange::NationalStockExchange, 'D') => {
                counts.nse_d = counts.nse_d.saturating_add(1);
            }
            (Exchange::BombayStockExchange, 'I') => {
                counts.bse_i = counts.bse_i.saturating_add(1);
            }
            (Exchange::BombayStockExchange, 'D') => {
                counts.bse_d = counts.bse_d.saturating_add(1);
            }
            _ => {} // BSE_E and others: not tracked in diagnostic
        }
    }

    counts
}

/// Builds the CSV parse check result detail string.
fn build_csv_parse_detail(
    total_rows: usize,
    parsed_count: usize,
    counts: &SegmentCounts,
) -> String {
    format!(
        "total={total_rows}, parsed={parsed_count}, NSE_I={}, NSE_E={}, NSE_D={}, BSE_I={}, BSE_D={}",
        counts.nse_i, counts.nse_e, counts.nse_d, counts.bse_i, counts.bse_d
    )
}

/// Builds a CheckResult for a successful CSV parse.
fn build_csv_parse_check(
    total_rows: usize,
    parsed_count: usize,
    counts: &SegmentCounts,
    duration_ms: u64,
) -> CheckResult {
    CheckResult {
        name: "csv_parse".to_owned(),
        passed: true,
        detail: build_csv_parse_detail(total_rows, parsed_count, counts),
        duration_ms,
    }
}

/// Builds a CheckResult for a failed CSV parse.
fn build_csv_parse_error(err_msg: &str, duration_ms: u64) -> CheckResult {
    CheckResult {
        name: "csv_parse".to_owned(),
        passed: false,
        detail: format!("parse failed: {err_msg}"),
        duration_ms,
    }
}

/// Builds a CheckResult for a successful CSV download.
fn build_download_success(bytes_len: usize, source: &str, duration_ms: u64) -> CheckResult {
    CheckResult {
        name: "csv_download".to_owned(),
        passed: true,
        detail: format!("downloaded {bytes_len} bytes from {source} source"),
        duration_ms,
    }
}

/// Builds a CheckResult for a failed CSV download.
fn build_download_error(err_msg: &str, duration_ms: u64) -> CheckResult {
    CheckResult {
        name: "csv_download".to_owned(),
        passed: false,
        detail: format!("download failed: {err_msg}"),
        duration_ms,
    }
}

/// Builds a CheckResult for a successful universe build + validation.
fn build_universe_success(underlyings: usize, derivatives: usize, duration_ms: u64) -> CheckResult {
    CheckResult {
        name: "universe_build_and_validate".to_owned(),
        passed: true,
        detail: format!("{underlyings} underlyings, {derivatives} derivatives — validation passed"),
        duration_ms,
    }
}

/// Builds a CheckResult for a universe build that passed but validation failed.
fn build_universe_validation_error(
    underlyings: usize,
    derivatives: usize,
    err_msg: &str,
    duration_ms: u64,
) -> CheckResult {
    CheckResult {
        name: "universe_build_and_validate".to_owned(),
        passed: false,
        detail: format!(
            "{underlyings} underlyings, {derivatives} derivatives — validation FAILED: {err_msg}"
        ),
        duration_ms,
    }
}

/// Builds a CheckResult for a failed universe build.
fn build_universe_build_error(err_msg: &str, duration_ms: u64) -> CheckResult {
    CheckResult {
        name: "universe_build_and_validate".to_owned(),
        passed: false,
        detail: format!("universe build failed: {err_msg}"),
        duration_ms,
    }
}

/// Determines overall health from check results.
fn determine_healthy(checks: &[CheckResult]) -> bool {
    checks.iter().all(|c| c.passed)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run all diagnostic checks and return a structured report.
pub async fn run_instrument_diagnostic(
    primary_url: &str,
    fallback_url: &str,
    instrument_config: &InstrumentConfig,
) -> DiagnosticReport {
    let mut checks = Vec::with_capacity(8);

    // Check 1: URL reachability (primary)
    checks.push(check_url_reachability(primary_url, "primary").await);

    // Check 2: URL reachability (fallback)
    checks.push(check_url_reachability(fallback_url, "fallback").await);

    // Check 3: Time gate status
    checks.push(check_time_gate(
        &instrument_config.build_window_start,
        &instrument_config.build_window_end,
    ));

    // Check 4: Cache status
    checks.push(check_cache_status(
        &instrument_config.csv_cache_directory,
        &instrument_config.csv_cache_filename,
    ));

    // Check 5: CSV download
    let start = Instant::now();
    let download_result = download_instrument_csv(
        primary_url,
        fallback_url,
        &instrument_config.csv_cache_directory,
        &instrument_config.csv_cache_filename,
        instrument_config.csv_download_timeout_secs,
    )
    .await;
    let download_ms = start.elapsed().as_millis() as u64;

    let csv_text = match download_result {
        Ok(result) => {
            checks.push(build_download_success(
                result.csv_text.len(),
                &result.source,
                download_ms,
            ));
            Some(result.csv_text)
        }
        Err(err) => {
            checks.push(build_download_error(&err.to_string(), download_ms));
            None
        }
    };

    // Checks 6-8 depend on successful download
    if let Some(csv_text) = csv_text {
        // Check 6: CSV header validation
        checks.push(check_csv_headers(&csv_text));

        // Check 7: CSV parse
        let start = Instant::now();
        let parse_result = parse_instrument_csv(&csv_text);
        let parse_ms = start.elapsed().as_millis() as u64;

        match parse_result {
            Ok((total_rows, parsed_rows)) => {
                let counts = count_segments(&parsed_rows);

                checks.push(build_csv_parse_check(
                    total_rows,
                    parsed_rows.len(),
                    &counts,
                    parse_ms,
                ));

                // Check 8: Universe build + validation
                let start = Instant::now();
                match build_fno_universe_from_csv(&csv_text, "diagnostic") {
                    Ok(universe) => {
                        let build_ms = start.elapsed().as_millis() as u64;
                        let uc = universe.underlyings.len();
                        let dc = universe.derivative_contracts.len();

                        // Run validation
                        match validate_fno_universe(&universe) {
                            Ok(()) => {
                                checks.push(build_universe_success(uc, dc, build_ms));
                            }
                            Err(err) => {
                                checks.push(build_universe_validation_error(
                                    uc,
                                    dc,
                                    &err.to_string(),
                                    build_ms,
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        let build_ms = start.elapsed().as_millis() as u64;
                        checks.push(build_universe_build_error(&err.to_string(), build_ms));
                    }
                }
            }
            Err(err) => {
                checks.push(build_csv_parse_error(&err.to_string(), parse_ms));
            }
        }
    }

    let healthy = determine_healthy(&checks);
    info!(
        healthy,
        checks = checks.len(),
        "instrument diagnostic complete"
    );

    DiagnosticReport { healthy, checks }
}

// ---------------------------------------------------------------------------
// Individual Checks
// ---------------------------------------------------------------------------

/// Check if a URL is reachable via HEAD request.
async fn check_url_reachability(url: &str, label: &str) -> CheckResult {
    let start = Instant::now();

    let client = match Client::builder()
        .timeout(Duration::from_secs(INSTRUMENT_CSV_DOWNLOAD_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            return CheckResult {
                name: format!("url_reachability_{label}"),
                passed: false,
                detail: format!("failed to build HTTP client: {err}"),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    match client.head(url).send().await {
        Ok(response) => {
            let status = response.status();
            let content_length = response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown");

            CheckResult {
                name: format!("url_reachability_{label}"),
                passed: status.is_success(),
                detail: format!("HTTP {status}, content-length={content_length}, url={url}"),
                duration_ms: start.elapsed().as_millis() as u64,
            }
        }
        Err(err) => CheckResult {
            name: format!("url_reachability_{label}"),
            passed: false,
            detail: format!("request failed: {err}, url={url}"),
            duration_ms: start.elapsed().as_millis() as u64,
        },
    }
}

/// Check time gate status.
fn check_time_gate(window_start: &str, window_end: &str) -> CheckResult {
    let start = Instant::now();
    let is_open = is_within_build_window(window_start, window_end);

    let now_ist_str = {
        let offset = ist_offset();
        chrono::Utc::now()
            .with_timezone(&offset)
            .format("%H:%M:%S")
            .to_string()
    };

    CheckResult {
        name: "time_gate".to_owned(),
        passed: true, // Time gate is informational, not a pass/fail
        detail: format!(
            "ist_time={now_ist_str}, window=[{window_start}, {window_end}), gate_open={is_open}"
        ),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

/// Check cache directory and freshness marker status.
fn check_cache_status(cache_dir: &str, cache_filename: &str) -> CheckResult {
    let start = Instant::now();
    let cache_path = Path::new(cache_dir).join(cache_filename);
    let marker_path = Path::new(cache_dir).join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);

    let dir_exists = Path::new(cache_dir).is_dir();
    let file_exists = cache_path.is_file();
    let file_size = if file_exists {
        std::fs::metadata(&cache_path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let is_fresh = is_instrument_fresh(cache_dir);
    let marker_content = std::fs::read_to_string(&marker_path).unwrap_or_default();

    CheckResult {
        name: "cache_status".to_owned(),
        passed: true, // Cache is informational
        detail: format!(
            "dir_exists={dir_exists}, csv_exists={file_exists}, csv_bytes={file_size}, \
             fresh={is_fresh}, marker={}, cache_dir={cache_dir}",
            marker_content.trim()
        ),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

/// Check CSV headers against expected column names.
fn check_csv_headers(csv_text: &str) -> CheckResult {
    let start = Instant::now();

    // Strip BOM if present (same as csv_parser.rs)
    let csv_text = csv_text.strip_prefix('\u{FEFF}').unwrap_or(csv_text);

    // Read just the first line as headers
    let first_line = csv_text.lines().next().unwrap_or("");
    let actual_columns: Vec<&str> = first_line.split(',').map(|s| s.trim()).collect();

    let mut missing: Vec<&str> = Vec::new();
    for expected in EXPECTED_COLUMNS {
        if !actual_columns.contains(expected) {
            missing.push(expected);
        }
    }

    let extra: Vec<&&str> = actual_columns
        .iter()
        .filter(|col| !col.is_empty() && !EXPECTED_COLUMNS.contains(col))
        .collect();

    if missing.is_empty() {
        CheckResult {
            name: "csv_headers".to_owned(),
            passed: true,
            detail: format!(
                "all {} expected columns present, {} extra columns: [{}]",
                EXPECTED_COLUMNS.len(),
                extra.len(),
                extra.iter().map(|s| **s).collect::<Vec<_>>().join(", ")
            ),
            duration_ms: start.elapsed().as_millis() as u64,
        }
    } else {
        CheckResult {
            name: "csv_headers".to_owned(),
            passed: false,
            detail: format!(
                "MISSING columns: [{}], extra columns: [{}], actual header: {first_line}",
                missing.join(", "),
                extra.iter().map(|s| **s).collect::<Vec<_>>().join(", ")
            ),
            duration_ms: start.elapsed().as_millis() as u64,
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
    fn test_check_csv_headers_all_present() {
        let header = "EXCH_ID,SEGMENT,SECURITY_ID,ISIN,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                       UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,INSTRUMENT_TYPE,SERIES,\
                       LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,EXPIRY_FLAG,";
        let csv = format!(
            "{header}\nNSE,I,13,ISIN,IDX_I,13,NIFTY,NIFTY 50,Nifty 50,INDEX,EQ,1,0001-01-01,0,XX,0.05,0,"
        );
        let result = check_csv_headers(&csv);
        assert!(result.passed, "all columns present: {}", result.detail);
    }

    #[test]
    fn test_check_csv_headers_missing_column() {
        // Missing EXPIRY_FLAG
        let header = "EXCH_ID,SEGMENT,SECURITY_ID,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                       UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,SERIES,\
                       LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,";
        let result = check_csv_headers(header);
        assert!(!result.passed, "missing column should fail");
        assert!(
            result.detail.contains("EXPIRY_FLAG"),
            "should report missing EXPIRY_FLAG: {}",
            result.detail
        );
    }

    #[test]
    fn test_check_csv_headers_with_bom() {
        let header = "\u{FEFF}EXCH_ID,SEGMENT,SECURITY_ID,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                       UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,SERIES,\
                       LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,EXPIRY_FLAG,";
        let result = check_csv_headers(header);
        assert!(result.passed, "BOM should be stripped: {}", result.detail);
    }

    #[test]
    fn test_check_time_gate_always_passes() {
        let result = check_time_gate("08:25:00", "08:55:00");
        assert!(result.passed, "time gate is informational");
        assert!(result.detail.contains("gate_open="));
    }

    #[test]
    fn test_check_cache_status_nonexistent_dir() {
        let result = check_cache_status("/tmp/dlt-nonexistent-diag-99999", "test.csv");
        assert!(result.passed, "cache status is informational");
        assert!(
            result.detail.contains("dir_exists=false"),
            "should report dir missing: {}",
            result.detail
        );
    }

    #[test]
    fn test_diagnostic_report_serialization() {
        let report = DiagnosticReport {
            healthy: true,
            checks: vec![CheckResult {
                name: "test".to_owned(),
                passed: true,
                detail: "ok".to_owned(),
                duration_ms: 42,
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"healthy\":true"));
        assert!(json.contains("\"name\":\"test\""));
    }

    #[test]
    fn test_expected_columns_count() {
        assert_eq!(
            EXPECTED_COLUMNS.len(),
            15,
            "should have 15 expected columns"
        );
    }

    #[test]
    fn test_check_csv_headers_empty_input() {
        let result = check_csv_headers("");
        assert!(!result.passed, "empty input should have missing columns");
    }

    #[tokio::test]
    async fn test_check_url_reachability_unreachable() {
        let result = check_url_reachability("http://127.0.0.1:1/nonexistent", "test").await;
        assert!(!result.passed, "unreachable URL should fail");
        assert!(result.detail.contains("request failed"));
    }

    // -----------------------------------------------------------------------
    // count_segments tests
    // -----------------------------------------------------------------------

    fn make_row(
        exchange: dhan_live_trader_common::types::Exchange,
        segment: char,
    ) -> super::super::csv_parser::ParsedInstrumentRow {
        super::super::csv_parser::ParsedInstrumentRow {
            exchange,
            segment,
            security_id: 1,
            instrument: String::new(),
            underlying_security_id: 0,
            underlying_symbol: String::new(),
            symbol_name: String::new(),
            display_name: String::new(),
            series: String::new(),
            lot_size: 1,
            expiry_date: None,
            strike_price: 0.0,
            option_type: None,
            tick_size: 0.05,
            expiry_flag: String::new(),
        }
    }

    #[test]
    fn test_count_segments_empty() {
        let counts = count_segments(&[]);
        assert_eq!(
            counts,
            SegmentCounts {
                nse_i: 0,
                nse_e: 0,
                nse_d: 0,
                bse_i: 0,
                bse_d: 0,
            }
        );
    }

    #[test]
    fn test_count_segments_nse_only() {
        use dhan_live_trader_common::types::Exchange;

        let rows = vec![
            make_row(Exchange::NationalStockExchange, 'I'),
            make_row(Exchange::NationalStockExchange, 'I'),
            make_row(Exchange::NationalStockExchange, 'E'),
            make_row(Exchange::NationalStockExchange, 'D'),
            make_row(Exchange::NationalStockExchange, 'D'),
            make_row(Exchange::NationalStockExchange, 'D'),
        ];
        let counts = count_segments(&rows);
        assert_eq!(counts.nse_i, 2);
        assert_eq!(counts.nse_e, 1);
        assert_eq!(counts.nse_d, 3);
        assert_eq!(counts.bse_i, 0);
        assert_eq!(counts.bse_d, 0);
    }

    #[test]
    fn test_count_segments_bse_only() {
        use dhan_live_trader_common::types::Exchange;

        let rows = vec![
            make_row(Exchange::BombayStockExchange, 'I'),
            make_row(Exchange::BombayStockExchange, 'D'),
            make_row(Exchange::BombayStockExchange, 'D'),
        ];
        let counts = count_segments(&rows);
        assert_eq!(counts.nse_i, 0);
        assert_eq!(counts.nse_e, 0);
        assert_eq!(counts.nse_d, 0);
        assert_eq!(counts.bse_i, 1);
        assert_eq!(counts.bse_d, 2);
    }

    #[test]
    fn test_count_segments_mixed() {
        use dhan_live_trader_common::types::Exchange;

        let rows = vec![
            make_row(Exchange::NationalStockExchange, 'I'),
            make_row(Exchange::BombayStockExchange, 'I'),
            make_row(Exchange::NationalStockExchange, 'E'),
            make_row(Exchange::NationalStockExchange, 'D'),
            make_row(Exchange::BombayStockExchange, 'D'),
        ];
        let counts = count_segments(&rows);
        assert_eq!(counts.nse_i, 1);
        assert_eq!(counts.nse_e, 1);
        assert_eq!(counts.nse_d, 1);
        assert_eq!(counts.bse_i, 1);
        assert_eq!(counts.bse_d, 1);
    }

    #[test]
    fn test_count_segments_unknown_segment_ignored() {
        use dhan_live_trader_common::types::Exchange;

        let rows = vec![
            make_row(Exchange::NationalStockExchange, 'X'), // unknown segment
            make_row(Exchange::BombayStockExchange, 'E'),   // BSE_E not tracked
        ];
        let counts = count_segments(&rows);
        assert_eq!(
            counts,
            SegmentCounts {
                nse_i: 0,
                nse_e: 0,
                nse_d: 0,
                bse_i: 0,
                bse_d: 0,
            }
        );
    }

    // -----------------------------------------------------------------------
    // build_csv_parse_detail tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_csv_parse_detail() {
        let counts = SegmentCounts {
            nse_i: 10,
            nse_e: 2000,
            nse_d: 5000,
            bse_i: 5,
            bse_d: 100,
        };
        let detail = build_csv_parse_detail(10000, 7115, &counts);
        assert!(detail.contains("total=10000"));
        assert!(detail.contains("parsed=7115"));
        assert!(detail.contains("NSE_I=10"));
        assert!(detail.contains("NSE_E=2000"));
        assert!(detail.contains("NSE_D=5000"));
        assert!(detail.contains("BSE_I=5"));
        assert!(detail.contains("BSE_D=100"));
    }

    #[test]
    fn test_build_csv_parse_detail_zeros() {
        let counts = SegmentCounts {
            nse_i: 0,
            nse_e: 0,
            nse_d: 0,
            bse_i: 0,
            bse_d: 0,
        };
        let detail = build_csv_parse_detail(0, 0, &counts);
        assert!(detail.contains("total=0"));
        assert!(detail.contains("parsed=0"));
    }

    // -----------------------------------------------------------------------
    // build_csv_parse_check tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_csv_parse_check() {
        let counts = SegmentCounts {
            nse_i: 5,
            nse_e: 100,
            nse_d: 500,
            bse_i: 3,
            bse_d: 50,
        };
        let result = build_csv_parse_check(1000, 658, &counts, 42);
        assert!(result.passed);
        assert_eq!(result.name, "csv_parse");
        assert_eq!(result.duration_ms, 42);
        assert!(result.detail.contains("parsed=658"));
    }

    // -----------------------------------------------------------------------
    // build_csv_parse_error tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_csv_parse_error() {
        let result = build_csv_parse_error("column SECURITY_ID not found", 15);
        assert!(!result.passed);
        assert_eq!(result.name, "csv_parse");
        assert!(result.detail.contains("parse failed"));
        assert!(result.detail.contains("column SECURITY_ID not found"));
        assert_eq!(result.duration_ms, 15);
    }

    // -----------------------------------------------------------------------
    // build_download_success / build_download_error tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_download_success() {
        let result = build_download_success(5_000_000, "primary", 250);
        assert!(result.passed);
        assert_eq!(result.name, "csv_download");
        assert!(result.detail.contains("5000000 bytes"));
        assert!(result.detail.contains("primary source"));
        assert_eq!(result.duration_ms, 250);
    }

    #[test]
    fn test_build_download_error() {
        let result = build_download_error("connection refused", 5000);
        assert!(!result.passed);
        assert_eq!(result.name, "csv_download");
        assert!(result.detail.contains("download failed"));
        assert!(result.detail.contains("connection refused"));
        assert_eq!(result.duration_ms, 5000);
    }

    // -----------------------------------------------------------------------
    // build_universe_success / error tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_universe_success() {
        let result = build_universe_success(25, 5000, 100);
        assert!(result.passed);
        assert_eq!(result.name, "universe_build_and_validate");
        assert!(result.detail.contains("25 underlyings"));
        assert!(result.detail.contains("5000 derivatives"));
        assert!(result.detail.contains("validation passed"));
        assert_eq!(result.duration_ms, 100);
    }

    #[test]
    fn test_build_universe_validation_error() {
        let result = build_universe_validation_error(25, 50, "too few derivatives", 100);
        assert!(!result.passed);
        assert_eq!(result.name, "universe_build_and_validate");
        assert!(result.detail.contains("25 underlyings"));
        assert!(result.detail.contains("50 derivatives"));
        assert!(result.detail.contains("validation FAILED"));
        assert!(result.detail.contains("too few derivatives"));
    }

    #[test]
    fn test_build_universe_build_error() {
        let result = build_universe_build_error("CSV parse failed", 50);
        assert!(!result.passed);
        assert_eq!(result.name, "universe_build_and_validate");
        assert!(result.detail.contains("universe build failed"));
        assert!(result.detail.contains("CSV parse failed"));
        assert_eq!(result.duration_ms, 50);
    }

    // -----------------------------------------------------------------------
    // determine_healthy tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_determine_healthy_all_passed() {
        let checks = vec![
            CheckResult {
                name: "a".to_owned(),
                passed: true,
                detail: "ok".to_owned(),
                duration_ms: 1,
            },
            CheckResult {
                name: "b".to_owned(),
                passed: true,
                detail: "ok".to_owned(),
                duration_ms: 2,
            },
        ];
        assert!(determine_healthy(&checks));
    }

    #[test]
    fn test_determine_healthy_one_failed() {
        let checks = vec![
            CheckResult {
                name: "a".to_owned(),
                passed: true,
                detail: "ok".to_owned(),
                duration_ms: 1,
            },
            CheckResult {
                name: "b".to_owned(),
                passed: false,
                detail: "fail".to_owned(),
                duration_ms: 2,
            },
        ];
        assert!(!determine_healthy(&checks));
    }

    #[test]
    fn test_determine_healthy_empty() {
        assert!(determine_healthy(&[]), "empty checks = vacuously healthy");
    }

    #[test]
    fn test_determine_healthy_all_failed() {
        let checks = vec![
            CheckResult {
                name: "a".to_owned(),
                passed: false,
                detail: "fail".to_owned(),
                duration_ms: 1,
            },
            CheckResult {
                name: "b".to_owned(),
                passed: false,
                detail: "fail".to_owned(),
                duration_ms: 2,
            },
        ];
        assert!(!determine_healthy(&checks));
    }

    // -----------------------------------------------------------------------
    // check_csv_headers additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_csv_headers_extra_columns_reported() {
        let header = "EXCH_ID,SEGMENT,SECURITY_ID,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                       UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,SERIES,\
                       LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,EXPIRY_FLAG,\
                       EXTRA_COL1,EXTRA_COL2";
        let result = check_csv_headers(header);
        assert!(result.passed);
        assert!(
            result.detail.contains("extra columns"),
            "detail: {}",
            result.detail
        );
        assert!(result.detail.contains("EXTRA_COL1"));
        assert!(result.detail.contains("EXTRA_COL2"));
    }

    #[test]
    fn test_check_csv_headers_only_extra_columns() {
        let header = "EXTRA1,EXTRA2,EXTRA3";
        let result = check_csv_headers(header);
        assert!(!result.passed, "missing all expected columns should fail");
        assert!(result.detail.contains("MISSING"));
    }

    #[test]
    fn test_check_csv_headers_whitespace_trimmed() {
        // Columns with spaces around names
        let header = " EXCH_ID , SEGMENT , SECURITY_ID , INSTRUMENT , UNDERLYING_SECURITY_ID ,\
                        UNDERLYING_SYMBOL , SYMBOL_NAME , DISPLAY_NAME , SERIES ,\
                        LOT_SIZE , SM_EXPIRY_DATE , STRIKE_PRICE , OPTION_TYPE , TICK_SIZE , EXPIRY_FLAG ";
        let result = check_csv_headers(header);
        assert!(
            result.passed,
            "whitespace should be trimmed: {}",
            result.detail
        );
    }

    // -----------------------------------------------------------------------
    // check_time_gate additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_time_gate_wide_window() {
        let result = check_time_gate("00:00:00", "23:59:59");
        assert!(result.passed);
        assert!(result.detail.contains("gate_open="));
        assert!(result.detail.contains("ist_time="));
    }

    #[test]
    fn test_check_time_gate_narrow_window() {
        let result = check_time_gate("08:25:00", "08:26:00");
        assert!(result.passed);
        assert_eq!(result.name, "time_gate");
    }

    // -----------------------------------------------------------------------
    // check_cache_status additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_cache_status_with_tmp_dir() {
        let result = check_cache_status("/tmp", "nonexistent_file.csv");
        assert!(result.passed);
        assert!(result.detail.contains("dir_exists=true"));
        assert!(result.detail.contains("csv_exists=false"));
    }

    #[test]
    fn test_check_cache_status_reports_fresh_and_marker() {
        let result = check_cache_status("/tmp/dlt-nonexistent-99999", "test.csv");
        assert!(result.detail.contains("fresh="));
        assert!(result.detail.contains("marker="));
        assert!(result.detail.contains("cache_dir="));
    }

    #[test]
    fn test_check_cache_status_with_actual_csv_file() {
        let temp_dir = std::env::temp_dir().join(format!(
            "dlt-diag-cache-status-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&temp_dir);
        let csv_filename = "test-instruments.csv";
        let csv_path = temp_dir.join(csv_filename);
        std::fs::write(&csv_path, "HEADER,ROW\ndata,here\n").unwrap();

        let result = check_cache_status(temp_dir.to_str().unwrap(), csv_filename);
        assert!(result.passed);
        assert!(
            result.detail.contains("dir_exists=true"),
            "detail: {}",
            result.detail
        );
        assert!(
            result.detail.contains("csv_exists=true"),
            "detail: {}",
            result.detail
        );
        // csv_bytes should be non-zero
        assert!(
            !result.detail.contains("csv_bytes=0"),
            "csv_bytes should be non-zero when file exists: {}",
            result.detail
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_check_cache_status_with_fresh_marker() {
        use dhan_live_trader_common::constants::INSTRUMENT_FRESHNESS_MARKER_FILENAME;
        let temp_dir = std::env::temp_dir().join(format!(
            "dlt-diag-fresh-marker-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&temp_dir);

        // Write today's marker
        let today = chrono::Utc::now()
            .with_timezone(&dhan_live_trader_common::trading_calendar::ist_offset())
            .date_naive()
            .to_string();
        std::fs::write(temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME), &today).unwrap();

        let result = check_cache_status(temp_dir.to_str().unwrap(), "missing.csv");
        assert!(result.passed);
        assert!(
            result.detail.contains("fresh=true"),
            "should be fresh when marker matches today: {}",
            result.detail
        );
        assert!(
            result.detail.contains(&today),
            "marker content should appear: {}",
            result.detail
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------------
    // DiagnosticReport / CheckResult additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_diagnostic_report_unhealthy() {
        let report = DiagnosticReport {
            healthy: false,
            checks: vec![
                CheckResult {
                    name: "url_reachability_primary".to_owned(),
                    passed: true,
                    detail: "HTTP 200".to_owned(),
                    duration_ms: 100,
                },
                CheckResult {
                    name: "csv_download".to_owned(),
                    passed: false,
                    detail: "download failed: timeout".to_owned(),
                    duration_ms: 5000,
                },
            ],
        };
        assert!(!report.healthy);
        assert_eq!(report.checks.len(), 2);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"healthy\":false"));
    }

    #[test]
    fn test_check_result_serialization() {
        let check = CheckResult {
            name: "test_check".to_owned(),
            passed: false,
            detail: "something went wrong".to_owned(),
            duration_ms: 999,
        };
        let json = serde_json::to_string(&check).unwrap();
        assert!(json.contains("\"name\":\"test_check\""));
        assert!(json.contains("\"passed\":false"));
        assert!(json.contains("\"duration_ms\":999"));
    }

    #[test]
    fn test_segment_counts_debug() {
        let counts = SegmentCounts {
            nse_i: 1,
            nse_e: 2,
            nse_d: 3,
            bse_i: 4,
            bse_d: 5,
        };
        let debug_str = format!("{counts:?}");
        assert!(debug_str.contains("nse_i: 1"));
        assert!(debug_str.contains("bse_d: 5"));
    }

    #[test]
    fn test_segment_counts_clone() {
        let counts = SegmentCounts {
            nse_i: 10,
            nse_e: 20,
            nse_d: 30,
            bse_i: 40,
            bse_d: 50,
        };
        let cloned = counts.clone();
        assert_eq!(counts, cloned);
    }
}
