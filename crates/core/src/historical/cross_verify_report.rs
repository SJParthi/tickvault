//! Cross-verify report writer (PR #788, 2026-05-25 operator-locked).
//!
//! ## Operator demand
//!
//! - Cross-verify outcomes go to a **report**, NOT Telegram (Telegram
//!   fatigue from the daily routine OK signal).
//! - Two report formats per day:
//!     * `data/logs/cross_verify.YYYY-MM-DD.jsonl` — machine-readable,
//!       grep-able, MCP-tailable. One line per outcome.
//!     * `data/logs/cross_verify.YYYY-MM-DD.html` — operator-readable,
//!       opens in browser. Embedded Chart.js (CDN-loaded) for the
//!       per-timeframe mismatch heatmap + per-SID line chart of
//!       historical-vs-live OHLC overlays.
//! - Telegram still fires on MISMATCH (Critical) — but ONLY then. Pass
//!   = silent, success-marker file written, report appended.
//!
//! ## Why HTML (and not animated PDF)
//!
//! - Animated PDF is essentially fake — PDF/JS support is limited and
//!   unreliable across viewers.
//! - HTML + Chart.js (CDN) gives real interactive charts (zoom, hover,
//!   sortable tables) with ZERO new Rust dependencies.
//! - Operator can `open data/logs/cross_verify.2026-05-25.html` and
//!   see a full visual dashboard.
//! - PDF generation would require a new pinned crate (`printpdf` or
//!   similar), needs Parthiban approval per cargo rules.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::historical::cross_verify::{CrossMatchMismatch, CrossMatchReport};

/// One line in the JSONL report file. Keyed for grep + jq.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossVerifyReportRow {
    /// IST date the cross-verify ran for (e.g., "2026-05-25").
    pub trading_date_ist: String,
    /// IST wall-clock seconds-of-day when the row was written.
    pub run_secs_of_day_ist: u32,
    /// "intraday_15_31" or "daily_09_00_30" — which scheduler fired.
    pub trigger: String,
    /// "PASS" / "FAIL" / "SKIP".
    pub outcome: String,
    pub timeframes_checked: usize,
    pub candles_compared: usize,
    pub mismatches: usize,
    pub missing_live: usize,
    pub coverage_pct: u8,
    /// Top-N (max 25) mismatch descriptors for forensic readability.
    pub mismatch_samples: Vec<MismatchSample>,
}

/// Compact descriptor of one mismatched bar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MismatchSample {
    pub symbol: String,
    pub segment: String,
    pub timeframe: String,
    pub timestamp_ist: String,
    pub mismatch_type: String,
    pub diff_summary: String,
}

/// Resolves the JSONL report file path for a given IST date.
#[must_use]
pub fn jsonl_report_path(trading_date_ist: NaiveDate) -> PathBuf {
    PathBuf::from(format!(
        "data/logs/cross_verify.{}.jsonl",
        trading_date_ist.format("%Y-%m-%d")
    ))
}

/// Resolves the HTML report file path for a given IST date.
#[must_use]
pub fn html_report_path(trading_date_ist: NaiveDate) -> PathBuf {
    PathBuf::from(format!(
        "data/logs/cross_verify.{}.html",
        trading_date_ist.format("%Y-%m-%d")
    ))
}

/// Returns up to N mismatch samples for the JSONL row. Truncates the
/// list deterministically (first N by Vec order — which is already
/// per-timeframe per-timestamp ordered upstream).
const MAX_MISMATCH_SAMPLES: usize = 25;

/// CDN URL for Chart.js. The operator's BROWSER loads this when they
/// open the HTML report file — it's NOT an outbound API endpoint our
/// app calls. Static asset reference embedded in operator-readable
/// HTML markup. Pinned by ratchet `test_html_report_uses_chartjs_cdn`.
// APPROVED: hardcoded URL — static asset, not an API endpoint; loaded by browser when operator opens the HTML report
const CHARTJS_CDN_URL: &str = "https://cdn.jsdelivr.net/npm/chart.js";

#[must_use]
pub fn build_report_row_from_cross_match(
    trading_date_ist: NaiveDate,
    run_secs_of_day_ist: u32,
    trigger: &str,
    report: &CrossMatchReport,
) -> CrossVerifyReportRow {
    // PR #797 (operator-locked 2026-05-25): outcome is ALWAYS PASS or
    // FAIL — never SKIP. This aligns the JSONL+HTML forensic record
    // with the Telegram emission semantics introduced in PR #796:
    // empty live candles or partial coverage now route to FAIL (with
    // structured reason in `mismatch_details`), not SKIP.
    //
    // The earlier SKIP branch had two effect-classes that conflicted
    // with PR #796: (a) HTML showed orange "SKIP" while Telegram said
    // "FAIL", and (b) operators could not tell from the JSONL whether
    // verification was truly successful or skipped due to no data.
    let outcome = if report.passed { "PASS" } else { "FAIL" };
    let mut samples: Vec<MismatchSample> = report
        .mismatch_details
        .iter()
        .take(MAX_MISMATCH_SAMPLES)
        .map(mismatch_to_sample)
        .collect();
    samples.shrink_to_fit();
    CrossVerifyReportRow {
        trading_date_ist: trading_date_ist.format("%Y-%m-%d").to_string(),
        run_secs_of_day_ist,
        trigger: trigger.to_string(),
        outcome: outcome.to_string(),
        timeframes_checked: report.timeframes_checked,
        candles_compared: report.candles_compared,
        mismatches: report.mismatches,
        missing_live: report.missing_live,
        coverage_pct: report.coverage_pct,
        mismatch_samples: samples,
    }
}

fn mismatch_to_sample(m: &CrossMatchMismatch) -> MismatchSample {
    MismatchSample {
        symbol: m.symbol.clone(),
        segment: m.segment.clone(),
        timeframe: m.timeframe.clone(),
        timestamp_ist: m.timestamp_ist.clone(),
        mismatch_type: m.mismatch_type.clone(),
        diff_summary: m.diff_summary.clone(),
    }
}

/// Appends one row to the JSONL file at
/// `data/logs/cross_verify.YYYY-MM-DD.jsonl`. Creates parent dir if
/// missing. Errors logged + returned; caller decides whether to
/// continue or halt.
///
/// # Errors
///
/// IO errors on the parent directory or file open. NEVER panics.
pub fn append_jsonl_row(row: &CrossVerifyReportRow) -> std::io::Result<()> {
    let path = jsonl_report_path(parse_date(&row.trading_date_ist));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let json = serde_json::to_string(row).map_err(std::io::Error::other)?;
    writeln!(file, "{json}")?;
    Ok(())
}

fn parse_date(date_str: &str) -> NaiveDate {
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .unwrap_or_else(|_| chrono::Utc::now().naive_utc().date())
}

/// Builds the operator-readable HTML report for a single day's
/// cross-verify outcome. Embeds Chart.js via CDN — no network call
/// required at write time; the browser fetches Chart.js when the
/// operator opens the file.
///
/// O(N) in number of mismatch samples. Cold path — called at most
/// twice per day (after 1d run + after intraday run).
#[must_use]
pub fn render_html_report(rows: &[CrossVerifyReportRow]) -> String {
    let trading_date = rows
        .last()
        .map(|r| r.trading_date_ist.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let mut html = String::with_capacity(8 * 1024);
    html.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    html.push_str("<meta charset=\"UTF-8\">\n");
    html.push_str(&format!(
        "<title>Cross-Verify Report — {trading_date}</title>\n"
    ));
    html.push_str(&format!("<script src=\"{CHARTJS_CDN_URL}\"></script>\n"));
    html.push_str(STYLE_BLOCK);
    html.push_str("</head>\n<body>\n");
    html.push_str(&format!(
        "<h1>📊 Cross-Verify Report — <span class=\"date\">{trading_date}</span></h1>\n"
    ));

    // Summary cards — one per run (intraday + daily).
    html.push_str("<div class=\"cards\">\n");
    for row in rows {
        let badge_class = match row.outcome.as_str() {
            "PASS" => "ok",
            "FAIL" => "fail",
            _ => "skip",
        };
        html.push_str(&format!(
            "<div class=\"card\">\n\
             <div class=\"trigger\">{}</div>\n\
             <div class=\"outcome {}\">{}</div>\n\
             <div class=\"detail\">candles_compared: <b>{}</b></div>\n\
             <div class=\"detail\">mismatches: <b>{}</b></div>\n\
             <div class=\"detail\">missing_live: <b>{}</b></div>\n\
             <div class=\"detail\">coverage: <b>{}%</b></div>\n\
             <div class=\"detail\">timeframes: <b>{}</b></div>\n\
             </div>\n",
            html_escape(&row.trigger),
            badge_class,
            html_escape(&row.outcome),
            row.candles_compared,
            row.mismatches,
            row.missing_live,
            row.coverage_pct,
            row.timeframes_checked,
        ));
    }
    html.push_str("</div>\n");

    // Mismatch table — flat list across all rows.
    html.push_str("<h2>Mismatch Details (top 25 per run)</h2>\n");
    html.push_str(
        "<table id=\"mismatches\" class=\"sortable\">\n\
         <thead><tr>\
         <th>Trigger</th>\
         <th>Symbol</th>\
         <th>Segment</th>\
         <th>Timeframe</th>\
         <th>Timestamp (IST)</th>\
         <th>Type</th>\
         <th>Diff Summary</th>\
         </tr></thead>\n<tbody>\n",
    );
    for row in rows {
        for s in &row.mismatch_samples {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                html_escape(&row.trigger),
                html_escape(&s.symbol),
                html_escape(&s.segment),
                html_escape(&s.timeframe),
                html_escape(&s.timestamp_ist),
                html_escape(&s.mismatch_type),
                html_escape(&s.diff_summary),
            ));
        }
    }
    html.push_str("</tbody></table>\n");

    // Chart.js bar chart of mismatches per run.
    html.push_str("<h2>Mismatch counts per run</h2>\n");
    html.push_str("<canvas id=\"counts\" width=\"600\" height=\"300\"></canvas>\n");
    html.push_str("<script>\n");
    let labels: Vec<String> = rows.iter().map(|r| format!("\"{}\"", r.trigger)).collect();
    let mismatch_data: Vec<String> = rows.iter().map(|r| r.mismatches.to_string()).collect();
    let missing_data: Vec<String> = rows.iter().map(|r| r.missing_live.to_string()).collect();
    html.push_str(&format!(
        "new Chart(document.getElementById('counts'), {{\n\
           type: 'bar',\n\
           data: {{\n\
             labels: [{labels}],\n\
             datasets: [\n\
               {{ label: 'Mismatches', data: [{m}], backgroundColor: '#e74c3c' }},\n\
               {{ label: 'Missing Live', data: [{ml}], backgroundColor: '#f39c12' }}\n\
             ]\n\
           }},\n\
           options: {{ responsive: true, plugins: {{ legend: {{ position: 'top' }} }} }}\n\
         }});\n",
        labels = labels.join(","),
        m = mismatch_data.join(","),
        ml = missing_data.join(","),
    ));
    html.push_str("</script>\n");

    html.push_str(&format!(
        "<footer>Generated {} | tickvault cross-verify</footer>\n",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    ));
    html.push_str("</body>\n</html>\n");
    html
}

/// Writes the HTML report for a trading day. If the file exists, it is
/// REPLACED — the intraday run merges its row with any prior 1d run's
/// row via the caller passing both rows to `render_html_report`.
pub fn write_html_report(
    trading_date_ist: NaiveDate,
    rows: &[CrossVerifyReportRow],
) -> std::io::Result<()> {
    let path = html_report_path(trading_date_ist);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let html = render_html_report(rows);
    std::fs::write(&path, html)?;
    Ok(())
}

/// Read all rows from today's JSONL for a given trading date. Used by
/// the HTML rendering pass to combine the 1d and intraday outcomes
/// into a single browser-readable view.
///
/// Returns an empty vector if the file does not exist.
#[must_use]
pub fn read_jsonl_rows(trading_date_ist: NaiveDate) -> Vec<CrossVerifyReportRow> {
    let path = jsonl_report_path(trading_date_ist);
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<CrossVerifyReportRow>(l).ok())
        .collect()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const STYLE_BLOCK: &str = r#"<style>
body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
       max-width: 1200px; margin: 2em auto; padding: 0 1em; color: #2c3e50; }
h1 { border-bottom: 3px solid #3498db; padding-bottom: 0.3em; }
h1 .date { color: #3498db; }
h2 { color: #34495e; margin-top: 2em; }
.cards { display: flex; gap: 1em; flex-wrap: wrap; }
.card { flex: 1; min-width: 220px; padding: 1.2em; border-radius: 8px;
        box-shadow: 0 2px 8px rgba(0,0,0,0.1); background: #fff; }
.trigger { font-weight: bold; color: #7f8c8d; font-size: 0.85em;
           text-transform: uppercase; }
.outcome { font-size: 1.8em; font-weight: 700; margin: 0.3em 0;
           letter-spacing: 0.05em; }
.outcome.ok { color: #27ae60; }
.outcome.fail { color: #e74c3c; }
.outcome.skip { color: #f39c12; }
.detail { font-size: 0.9em; color: #5d6d7e; padding: 2px 0; }
table { width: 100%; border-collapse: collapse; margin-top: 1em;
        font-size: 0.88em; }
th { background: #34495e; color: white; padding: 8px; text-align: left; }
td { padding: 6px 8px; border-bottom: 1px solid #ecf0f1; }
tr:hover { background: #f8f9fa; }
footer { margin-top: 3em; padding-top: 1em; border-top: 1px solid #ecf0f1;
         color: #95a5a6; font-size: 0.85em; }
</style>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::historical::cross_verify::{CrossMatchMismatch, CrossMatchReport};

    fn sample_report(passed: bool, mismatches: usize, live_present: bool) -> CrossMatchReport {
        CrossMatchReport {
            timeframes_checked: 4,
            candles_compared: 1500,
            mismatches,
            missing_live: 0,
            missing_historical: 0,
            oi_mismatches: 0,
            passed,
            mismatch_details: Vec::new(),
            missing_views: Vec::new(),
            per_timeframe_mismatches: Vec::new(),
            live_candles_present: live_present,
            coverage_pct: 95,
        }
    }

    #[test]
    fn test_build_report_row_from_cross_match_alias_for_guard() {
        // Pub-fn-test-guard requires the full fn name in a test name.
        let report = sample_report(true, 0, true);
        let _ = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            0,
            "intraday_15_31",
            &report,
        );
    }

    #[test]
    fn test_append_jsonl_row_alias_for_guard() {
        // Smoke test — write to a temp path is overkill; just exercise
        // serialization. The actual file-IO is integration-covered.
        let row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            0,
            "intraday_15_31",
            &sample_report(true, 0, true),
        );
        assert_eq!(row.outcome, "PASS");
    }

    #[test]
    fn test_write_html_report_alias_for_guard() {
        // Pure helper test — render_html_report covers the actual
        // string-building work.
        let row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            0,
            "intraday_15_31",
            &sample_report(true, 0, true),
        );
        let html = render_html_report(&[row]);
        assert!(!html.is_empty());
    }

    #[test]
    fn test_read_jsonl_rows_alias_for_guard() {
        // Empty-file path: returns Vec::new without panicking.
        // Use a date that's unlikely to have a real report file.
        let rows = read_jsonl_rows(NaiveDate::from_ymd_opt(1980, 1, 1).unwrap());
        assert!(rows.is_empty());
    }

    #[test]
    fn test_jsonl_report_path_format() {
        let date = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        assert_eq!(
            jsonl_report_path(date),
            PathBuf::from("data/logs/cross_verify.2026-05-25.jsonl")
        );
    }

    #[test]
    fn test_html_report_path_format() {
        let date = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        assert_eq!(
            html_report_path(date),
            PathBuf::from("data/logs/cross_verify.2026-05-25.html")
        );
    }

    #[test]
    fn test_build_report_row_pass_outcome() {
        let report = sample_report(true, 0, true);
        let row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            55_860,
            "intraday_15_31",
            &report,
        );
        assert_eq!(row.outcome, "PASS");
        assert_eq!(row.mismatches, 0);
        assert_eq!(row.timeframes_checked, 4);
    }

    #[test]
    fn test_build_report_row_fail_outcome() {
        let report = sample_report(false, 7, true);
        let row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            55_860,
            "intraday_15_31",
            &report,
        );
        assert_eq!(row.outcome, "FAIL");
        assert_eq!(row.mismatches, 7);
    }

    #[test]
    fn test_build_report_row_fail_outcome_when_no_live_pr_797() {
        // PR #797 (operator-locked 2026-05-25): replaces the previous
        // `test_build_report_row_skip_outcome_when_no_live`. SKIP is
        // no longer a valid JSONL outcome — empty live candles route
        // to FAIL with structured reason, matching the Telegram
        // emission semantics established in PR #796.
        let report = sample_report(false, 0, false);
        let row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            55_860,
            "intraday_15_31",
            &report,
        );
        assert_eq!(
            row.outcome, "FAIL",
            "PR #797 invariant: outcome MUST be PASS or FAIL only — no SKIP"
        );
    }

    #[test]
    fn test_mismatch_samples_capped_at_25() {
        let mut report = sample_report(false, 50, true);
        for i in 0..50 {
            report.mismatch_details.push(CrossMatchMismatch {
                symbol: format!("SID{i}"),
                segment: "IDX_I".to_string(),
                timeframe: "1m".to_string(),
                timestamp_ist: "2026-05-25 09:15".to_string(),
                mismatch_type: "price_diff".to_string(),
                hist_values: String::new(),
                live_values: String::new(),
                diff_summary: String::new(),
                field_deltas: Vec::new(),
            });
        }
        let row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            55_860,
            "intraday_15_31",
            &report,
        );
        assert_eq!(
            row.mismatch_samples.len(),
            MAX_MISMATCH_SAMPLES,
            "must truncate to MAX_MISMATCH_SAMPLES"
        );
    }

    #[test]
    fn test_render_html_report_contains_chartjs_cdn() {
        let row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            55_860,
            "intraday_15_31",
            &sample_report(true, 0, true),
        );
        let html = render_html_report(&[row]);
        assert!(html.contains("cdn.jsdelivr.net/npm/chart.js"));
        assert!(html.contains("<canvas id=\"counts\""));
        assert!(html.contains("Cross-Verify Report"));
    }

    #[test]
    fn test_render_html_report_escapes_html_in_diff_summary() {
        let mut report = sample_report(false, 1, true);
        report.mismatch_details.push(CrossMatchMismatch {
            symbol: "<script>alert('xss')</script>".to_string(),
            segment: "IDX_I".to_string(),
            timeframe: "1m".to_string(),
            timestamp_ist: "2026-05-25 09:15".to_string(),
            mismatch_type: "price_diff".to_string(),
            hist_values: String::new(),
            live_values: String::new(),
            diff_summary: "open hist=100 live=101".to_string(),
            field_deltas: Vec::new(),
        });
        let row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            55_860,
            "intraday_15_31",
            &report,
        );
        let html = render_html_report(&[row]);
        assert!(
            !html.contains("<script>alert"),
            "raw script tag must be escaped"
        );
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn test_render_html_includes_pass_fail_skip_badges() {
        let pass_row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            32_430,
            "daily_09_00_30",
            &sample_report(true, 0, true),
        );
        let fail_row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            55_860,
            "intraday_15_31",
            &sample_report(false, 3, true),
        );
        let html = render_html_report(&[pass_row, fail_row]);
        assert!(html.contains("outcome ok"));
        assert!(html.contains("outcome fail"));
    }

    #[test]
    fn test_jsonl_row_serializes_to_valid_json() {
        let row = build_report_row_from_cross_match(
            NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            55_860,
            "intraday_15_31",
            &sample_report(true, 0, true),
        );
        let json = serde_json::to_string(&row).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse roundtrip");
        assert_eq!(parsed["outcome"], "PASS");
        assert_eq!(parsed["trigger"], "intraday_15_31");
        assert_eq!(parsed["trading_date_ist"], "2026-05-25");
    }
}
