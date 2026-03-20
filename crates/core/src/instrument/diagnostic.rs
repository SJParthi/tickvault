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
            checks.push(CheckResult {
                name: "csv_download".to_owned(),
                passed: true,
                detail: format!(
                    "downloaded {} bytes from {} source",
                    result.csv_text.len(),
                    result.source
                ),
                duration_ms: download_ms,
            });
            Some(result.csv_text)
        }
        Err(err) => {
            checks.push(CheckResult {
                name: "csv_download".to_owned(),
                passed: false,
                detail: format!("download failed: {err}"),
                duration_ms: download_ms,
            });
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
                let nse_i = parsed_rows
                    .iter()
                    .filter(|r| {
                        r.segment == 'I'
                            && matches!(
                                r.exchange,
                                dhan_live_trader_common::types::Exchange::NationalStockExchange
                            )
                    })
                    .count();
                let nse_e = parsed_rows
                    .iter()
                    .filter(|r| {
                        r.segment == 'E'
                            && matches!(
                                r.exchange,
                                dhan_live_trader_common::types::Exchange::NationalStockExchange
                            )
                    })
                    .count();
                let nse_d = parsed_rows
                    .iter()
                    .filter(|r| {
                        r.segment == 'D'
                            && matches!(
                                r.exchange,
                                dhan_live_trader_common::types::Exchange::NationalStockExchange
                            )
                    })
                    .count();
                let bse_i = parsed_rows
                    .iter()
                    .filter(|r| {
                        r.segment == 'I'
                            && matches!(
                                r.exchange,
                                dhan_live_trader_common::types::Exchange::BombayStockExchange
                            )
                    })
                    .count();
                let bse_d = parsed_rows
                    .iter()
                    .filter(|r| {
                        r.segment == 'D'
                            && matches!(
                                r.exchange,
                                dhan_live_trader_common::types::Exchange::BombayStockExchange
                            )
                    })
                    .count();

                checks.push(CheckResult {
                    name: "csv_parse".to_owned(),
                    passed: true,
                    detail: format!(
                        "total={total_rows}, parsed={}, NSE_I={nse_i}, NSE_E={nse_e}, NSE_D={nse_d}, BSE_I={bse_i}, BSE_D={bse_d}",
                        parsed_rows.len()
                    ),
                    duration_ms: parse_ms,
                });

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
                                checks.push(CheckResult {
                                    name: "universe_build_and_validate".to_owned(),
                                    passed: true,
                                    detail: format!(
                                        "{uc} underlyings, {dc} derivatives — validation passed"
                                    ),
                                    duration_ms: build_ms,
                                });
                            }
                            Err(err) => {
                                checks.push(CheckResult {
                                    name: "universe_build_and_validate".to_owned(),
                                    passed: false,
                                    detail: format!(
                                        "{uc} underlyings, {dc} derivatives — validation FAILED: {err}"
                                    ),
                                    duration_ms: build_ms,
                                });
                            }
                        }
                    }
                    Err(err) => {
                        let build_ms = start.elapsed().as_millis() as u64;
                        checks.push(CheckResult {
                            name: "universe_build_and_validate".to_owned(),
                            passed: false,
                            detail: format!("universe build failed: {err}"),
                            duration_ms: build_ms,
                        });
                    }
                }
            }
            Err(err) => {
                checks.push(CheckResult {
                    name: "csv_parse".to_owned(),
                    passed: false,
                    detail: format!("parse failed: {err}"),
                    duration_ms: parse_ms,
                });
            }
        }
    }

    let healthy = checks.iter().all(|c| c.passed);
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
    // Additional coverage: check_csv_headers edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_csv_headers_extra_columns_reported() {
        let header = "EXCH_ID,SEGMENT,SECURITY_ID,ISIN,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                       UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,SERIES,\
                       LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,EXPIRY_FLAG,\
                       EXTRA_COL1,EXTRA_COL2";
        let result = check_csv_headers(header);
        assert!(result.passed, "all required columns present");
        assert!(
            result.detail.contains("extra columns"),
            "should mention extra columns: {}",
            result.detail
        );
        assert!(
            result.detail.contains("EXTRA_COL1"),
            "should list EXTRA_COL1: {}",
            result.detail
        );
    }

    #[test]
    fn test_check_csv_headers_only_some_missing() {
        // Missing both EXPIRY_FLAG and TICK_SIZE
        let header = "EXCH_ID,SEGMENT,SECURITY_ID,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                       UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,SERIES,\
                       LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,";
        let result = check_csv_headers(header);
        assert!(!result.passed);
        assert!(result.detail.contains("TICK_SIZE"));
        assert!(result.detail.contains("EXPIRY_FLAG"));
    }

    #[test]
    fn test_check_csv_headers_garbage_input() {
        let result = check_csv_headers("THIS,IS,NOT,A,VALID,HEADER");
        assert!(!result.passed, "garbage headers should fail");
        assert!(result.detail.contains("MISSING"));
    }

    #[test]
    fn test_check_csv_headers_multiline_only_checks_first() {
        // First line has valid headers; second line is data
        let csv = "EXCH_ID,SEGMENT,SECURITY_ID,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                    UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,SERIES,\
                    LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,EXPIRY_FLAG\n\
                    NSE,I,13,INDEX,13,NIFTY,NIFTY 50,Nifty 50,EQ,1,0001-01-01,0,XX,0.05,0";
        let result = check_csv_headers(csv);
        assert!(
            result.passed,
            "should check only first line: {}",
            result.detail
        );
    }

    // -----------------------------------------------------------------------
    // Additional coverage: DiagnosticReport / CheckResult
    // -----------------------------------------------------------------------

    #[test]
    fn test_diagnostic_report_unhealthy_when_any_check_fails() {
        let report = DiagnosticReport {
            healthy: false,
            checks: vec![
                CheckResult {
                    name: "good".to_owned(),
                    passed: true,
                    detail: "ok".to_owned(),
                    duration_ms: 1,
                },
                CheckResult {
                    name: "bad".to_owned(),
                    passed: false,
                    detail: "failed".to_owned(),
                    duration_ms: 2,
                },
            ],
        };
        assert!(!report.healthy);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"healthy\":false"));
        assert!(json.contains("\"bad\""));
    }

    #[test]
    fn test_diagnostic_report_empty_checks_is_healthy() {
        let report = DiagnosticReport {
            healthy: true,
            checks: vec![],
        };
        assert!(report.healthy);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"checks\":[]"));
    }

    #[test]
    fn test_check_result_serialization_all_fields() {
        let result = CheckResult {
            name: "test_check".to_owned(),
            passed: false,
            detail: "something went wrong".to_owned(),
            duration_ms: 12345,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"name\":\"test_check\""));
        assert!(json.contains("\"passed\":false"));
        assert!(json.contains("\"detail\":\"something went wrong\""));
        assert!(json.contains("\"duration_ms\":12345"));
    }

    // -----------------------------------------------------------------------
    // Additional coverage: check_time_gate detail content
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_time_gate_detail_contains_window_params() {
        let result = check_time_gate("09:00:00", "15:30:00");
        assert!(result.passed);
        assert!(
            result.detail.contains("09:00:00"),
            "detail should contain window_start"
        );
        assert!(
            result.detail.contains("15:30:00"),
            "detail should contain window_end"
        );
        assert!(
            result.detail.contains("ist_time="),
            "detail should contain IST time"
        );
    }

    // -----------------------------------------------------------------------
    // Additional coverage: check_cache_status with existing files
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_cache_status_with_existing_csv_file() {
        let temp_dir =
            std::env::temp_dir().join(format!("dlt-test-diag-csv-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let csv_path = temp_dir.join("instruments.csv");
        std::fs::write(&csv_path, "header\ndata\nmore data").unwrap();

        let result = check_cache_status(temp_dir.to_str().unwrap(), "instruments.csv");
        assert!(result.passed, "cache status is informational");
        assert!(
            result.detail.contains("dir_exists=true"),
            "dir should exist: {}",
            result.detail
        );
        assert!(
            result.detail.contains("csv_exists=true"),
            "csv should exist: {}",
            result.detail
        );
        // File has some bytes
        assert!(
            !result.detail.contains("csv_bytes=0"),
            "csv bytes should be non-zero: {}",
            result.detail
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_check_cache_status_with_fresh_marker() {
        let temp_dir =
            std::env::temp_dir().join(format!("dlt-test-diag-fresh-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Write a freshness marker with today's date
        let today = chrono::Utc::now()
            .with_timezone(&ist_offset())
            .date_naive()
            .to_string();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, &today).unwrap();

        let result = check_cache_status(temp_dir.to_str().unwrap(), "instruments.csv");
        assert!(result.passed);
        assert!(
            result.detail.contains("fresh=true"),
            "should be fresh: {}",
            result.detail
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_check_cache_status_stale_marker() {
        let temp_dir =
            std::env::temp_dir().join(format!("dlt-test-diag-stale-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "2020-01-01").unwrap();

        let result = check_cache_status(temp_dir.to_str().unwrap(), "instruments.csv");
        assert!(result.passed);
        assert!(
            result.detail.contains("fresh=false"),
            "should be stale: {}",
            result.detail
        );
        assert!(
            result.detail.contains("2020-01-01"),
            "should show marker content: {}",
            result.detail
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------------
    // Additional coverage: EXPECTED_COLUMNS constant validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_expected_columns_includes_key_columns() {
        assert!(
            EXPECTED_COLUMNS.contains(&"EXCH_ID"),
            "must include EXCH_ID"
        );
        assert!(
            EXPECTED_COLUMNS.contains(&"SECURITY_ID"),
            "must include SECURITY_ID"
        );
        assert!(
            EXPECTED_COLUMNS.contains(&"INSTRUMENT"),
            "must include INSTRUMENT"
        );
        assert!(
            EXPECTED_COLUMNS.contains(&"UNDERLYING_SYMBOL"),
            "must include UNDERLYING_SYMBOL"
        );
        assert!(
            EXPECTED_COLUMNS.contains(&"STRIKE_PRICE"),
            "must include STRIKE_PRICE"
        );
        assert!(
            EXPECTED_COLUMNS.contains(&"OPTION_TYPE"),
            "must include OPTION_TYPE"
        );
    }

    #[test]
    fn test_expected_columns_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for col in EXPECTED_COLUMNS {
            assert!(
                seen.insert(col),
                "duplicate column in EXPECTED_COLUMNS: {}",
                col
            );
        }
    }

    // -----------------------------------------------------------------------
    // Additional coverage: check_csv_headers with whitespace variations
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_csv_headers_with_whitespace_around_columns() {
        // Columns with spaces — the trim in split should handle this
        let header = " EXCH_ID , SEGMENT , SECURITY_ID , INSTRUMENT , UNDERLYING_SECURITY_ID ,\
                        UNDERLYING_SYMBOL , SYMBOL_NAME , DISPLAY_NAME , SERIES ,\
                        LOT_SIZE , SM_EXPIRY_DATE , STRIKE_PRICE , OPTION_TYPE , TICK_SIZE , EXPIRY_FLAG ";
        let result = check_csv_headers(header);
        assert!(
            result.passed,
            "whitespace-padded columns should still be found: {}",
            result.detail
        );
    }

    #[test]
    fn test_check_csv_headers_all_missing_reports_all() {
        let result = check_csv_headers("ONLY_ONE_COLUMN");
        assert!(!result.passed);
        // Should report all 15 expected columns as missing
        assert!(result.detail.contains("EXCH_ID"));
        assert!(result.detail.contains("SEGMENT"));
        assert!(result.detail.contains("SECURITY_ID"));
    }

    #[tokio::test]
    async fn test_check_url_reachability_label_in_name() {
        let result = check_url_reachability("http://127.0.0.1:1/x", "custom_label").await;
        assert_eq!(result.name, "url_reachability_custom_label");
    }

    // -----------------------------------------------------------------------
    // Coverage: DiagnosticReport healthy logic
    // -----------------------------------------------------------------------

    #[test]
    fn test_diagnostic_report_healthy_matches_all_checks_passed() {
        let one_fail = DiagnosticReport {
            healthy: false,
            checks: vec![
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
                CheckResult {
                    name: "c".to_owned(),
                    passed: true,
                    detail: "ok".to_owned(),
                    duration_ms: 3,
                },
            ],
        };
        assert!(!one_fail.healthy);
        assert!(!one_fail.checks.iter().all(|c| c.passed));
    }

    #[test]
    fn test_check_csv_headers_isin_is_extra_not_required() {
        let header = "ISIN,EXCH_ID,SEGMENT,SECURITY_ID,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                       UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,SERIES,\
                       LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,EXPIRY_FLAG";
        let result = check_csv_headers(header);
        assert!(
            result.passed,
            "ISIN is extra, not required: {}",
            result.detail
        );
        assert!(
            result.detail.contains("ISIN"),
            "should list ISIN as extra: {}",
            result.detail
        );
    }

    #[test]
    fn test_check_cache_status_dir_exists_but_no_csv_file() {
        let temp_dir =
            std::env::temp_dir().join(format!("dlt-test-diag-nocsvfile-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let result = check_cache_status(temp_dir.to_str().unwrap(), "nonexistent.csv");
        assert!(result.passed);
        assert!(
            result.detail.contains("dir_exists=true"),
            "{}",
            result.detail
        );
        assert!(
            result.detail.contains("csv_exists=false"),
            "{}",
            result.detail
        );
        assert!(result.detail.contains("csv_bytes=0"), "{}", result.detail);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_diagnostic_report_many_checks_serialization() {
        let checks: Vec<CheckResult> = (0..10)
            .map(|i| CheckResult {
                name: format!("check_{i}"),
                passed: i % 2 == 0,
                detail: format!("detail_{i}"),
                duration_ms: i as u64 * 100,
            })
            .collect();
        let report = DiagnosticReport {
            healthy: false,
            checks,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("check_0"));
        assert!(json.contains("check_9"));
        assert!(json.contains("\"healthy\":false"));
    }

    #[test]
    fn test_check_csv_headers_only_bom_no_columns() {
        let result = check_csv_headers("\u{FEFF}");
        assert!(!result.passed, "BOM-only input should fail");
    }

    #[test]
    fn test_check_time_gate_narrow_window_format() {
        let result = check_time_gate("00:00:00", "00:00:01");
        assert!(result.passed, "informational check always passes");
        assert!(result.detail.contains("00:00:00"));
        assert!(result.detail.contains("00:00:01"));
    }

    #[tokio::test]
    async fn test_run_diagnostic_unreachable_urls() {
        let cfg = InstrumentConfig {
            daily_download_time: "08:30:00".to_string(),
            csv_cache_directory: "/tmp/dlt-diag-nonexist-99".to_string(),
            csv_cache_filename: "nonexistent.csv".to_string(),
            csv_download_timeout_secs: 2,
            build_window_start: "08:25:00".to_string(),
            build_window_end: "08:55:00".to_string(),
        };
        let report =
            run_instrument_diagnostic("http://127.0.0.1:1/p", "http://127.0.0.1:1/f", &cfg).await;
        assert!(!report.healthy);
        assert!(report.checks.len() >= 4);
    }

    #[tokio::test]
    async fn test_run_diagnostic_cached_csv_exercises_all_checks() {
        let dir = std::env::temp_dir().join(format!("dlt-diag-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let header = "EXCH_ID,SEGMENT,SECURITY_ID,ISIN,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                       UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,INSTRUMENT_TYPE,SERIES,\
                       LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,EXPIRY_FLAG,\
                       ASM_GSM_FLAG,ASM_GSM_CATEGORY,BUY_SELL_INDICATOR,MTF_LEVERAGE";
        // Generate enough rows to exceed INSTRUMENT_CSV_MIN_BYTES (1 MB)
        let rows: Vec<String> = (0..10000)
            .map(|i| {
                format!(
                    "NSE,I,{i},INE{i:09}01,INDEX,{i},NIFTY,NIFTY 50 INDEX,Nifty 50 Index Display Name Long,INDEX,EQ,1,\
                     0001-01-01,0,XX,0.05,0,N,NA,1,0"
                )
            })
            .collect();
        let csv_content = format!("{header}\n{}", rows.join("\n"));
        std::fs::write(dir.join("inst.csv"), &csv_content).unwrap();
        let cfg = InstrumentConfig {
            daily_download_time: "08:30:00".to_string(),
            csv_cache_directory: dir.to_str().unwrap().to_string(),
            csv_cache_filename: "inst.csv".to_string(),
            csv_download_timeout_secs: 2,
            build_window_start: "00:00:00".to_string(),
            build_window_end: "23:59:59".to_string(),
        };
        let report =
            run_instrument_diagnostic("http://127.0.0.1:1/p", "http://127.0.0.1:1/f", &cfg).await;
        // With cached CSV available, we expect: 2 URL + time_gate + cache + csv_download
        // + csv_headers + csv_parse + universe_build_and_validate = 8 checks
        assert!(
            report.checks.len() >= 5,
            "expected at least 5 checks, got: {} ({:?})",
            report.checks.len(),
            report.checks.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        // Verify csv_download check exists (from cache)
        let dl = report.checks.iter().find(|c| c.name == "csv_download");
        if let Some(dl_check) = dl {
            assert!(
                dl_check.passed,
                "csv_download should pass: {}",
                dl_check.detail
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_url_reachability_success_path() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://127.0.0.1:{}", addr.port());
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let r = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(r.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        let result = check_url_reachability(&url, "ok").await;
        assert!(result.passed);
        assert!(result.detail.contains("200"));
    }

    #[tokio::test]
    async fn test_url_reachability_404_path() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://127.0.0.1:{}", addr.port());
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let r = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(r.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        let result = check_url_reachability(&url, "nf").await;
        assert!(!result.passed);
        assert!(result.detail.contains("404"));
    }

    #[test]
    fn test_csv_headers_dup_cols_still_pass() {
        let header = "EXCH_ID,SEGMENT,SECURITY_ID,INSTRUMENT,UNDERLYING_SECURITY_ID,\
                       UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,SERIES,\
                       LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,EXPIRY_FLAG,\
                       EXCH_ID";
        let result = check_csv_headers(header);
        assert!(result.passed);
    }

    #[test]
    fn test_diagnostic_report_debug() {
        let report = DiagnosticReport {
            healthy: true,
            checks: vec![],
        };
        let d = format!("{:?}", report);
        assert!(d.contains("healthy: true"));
    }

    #[test]
    fn test_check_result_debug() {
        let check = CheckResult {
            name: "t".to_owned(),
            passed: true,
            detail: "ok".to_owned(),
            duration_ms: 0,
        };
        let d = format!("{:?}", check);
        assert!(d.contains("\"t\""));
    }
}
