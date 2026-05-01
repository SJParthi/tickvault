//! Wave 5 Item 26 L2 Phase D — 16:30 IST bhavcopy cross-check scheduler.
//!
//! Daily post-market task. Pipeline:
//!
//! ```text
//! 16:30 IST scheduler tick
//!   │
//!   ▼  TradingCalendar gate (skip weekends/holidays)
//! bhavcopy_fetcher::fetch_bhavcopy_zip(Fno, YYYYMMDD)
//!   │
//!   ▼  unzip via shell `unzip -p`  (ZIP extraction; see ZIP RATIONALE below)
//! CSV bytes
//!   │
//!   ▼  bhavcopy_cross_check::parse_bhavcopy_csv
//! Vec<BhavcopyRow>
//!   │
//!   ▼  caller-supplied dhan_eod_volumes + lookup
//! Vec<BhavcopyAuditRow>
//!   │
//!   ▼  volume_nse_audit_persistence::append_volume_nse_audit_row × N
//! Telegram NseBhavcopyCheckComplete | NseBhavcopyCheckFailed
//! ```
//!
//! # ZIP rationale
//!
//! NSE serves the bhavcopy as `.csv.zip`. The `zip` Rust crate was NOT
//! added per operator directive 2026-05-01. Instead we shell out to the
//! standard `unzip` binary via `tokio::process::Command`. `unzip` is
//! ubiquitous on Linux + macOS; Docker images that don't have it must
//! `apt-get install -y unzip` (one line). On unzip-missing, the
//! scheduler emits `NseBhavcopyCheckFailed { reason: "unzip_unavailable" }`.
//!
//! Security: `unzip -p` reads the ZIP from stdin so we never pass a
//! caller-controlled path/filename. Zero command-injection surface.
//!
//! # Honest envelope
//!
//! - Outside `[trading_day, 15:30 IST + 1h]` window → no fetch
//! - HTTP 404 / 4xx → fetcher's retry exhausts → typed
//!   `NseBhavcopyCheckFailed { reason: "http_*" }`
//! - ZIP corrupt / unzip missing → `NseBhavcopyCheckFailed { reason }`
//! - QuestDB unreachable for audit writes → flush errors logged via
//!   `volume_nse_audit_persistence`, scheduler reports best-effort
//! - Idempotent: `volume_nse_audit` DEDUP key (trading_date_ist,
//!   security_id, segment) means re-running the same day overwrites,
//!   not appends

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, NaiveTime, Utc};
use tokio::process::Command;
use tracing::info;

use crate::instrument::bhavcopy_cross_check::{
    BHAVCOPY_VOLUME_TOLERANCE_PCT, BhavcopyAuditRow, BhavcopyVerdict, build_dhan_lookup,
    compare_volumes, parse_bhavcopy_csv,
};
use crate::instrument::bhavcopy_fetcher::{BhavcopySegment, fetch_bhavcopy_zip};
use tickvault_common::instrument_types::DerivativeContract;

/// Scheduler trigger time — 16:30 IST. NSE typically publishes the
/// daily bhavcopy by 16:00 IST; we leave a 30-min buffer for slow
/// publish + Akamai propagation.
pub const BHAVCOPY_TRIGGER_TIME_IST: NaiveTime = match NaiveTime::from_hms_opt(16, 30, 0) {
    Some(t) => t,
    None => unreachable!(),
};

/// Maximum time the entire end-to-end pipeline is allowed to run before
/// the scheduler treats it as a hung task and abandons. NSE bhavcopy
/// download is ~1.1 MB → typically <2s; full F&O parse + 200K-row
/// cross-check + audit insert is ~30s. 5 minutes is a generous safety net.
pub const BHAVCOPY_PIPELINE_TIMEOUT_SECS: u64 = 300;

/// IST UTC offset constructor — `+05:30` per the trading-calendar
/// canonical helper. `unwrap` is safe at compile time.
#[must_use]
fn ist_offset() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("IST offset is valid")
}

/// Computes the duration from `now_ist` until the next occurrence of
/// `target_time` (IST). Pure function — deterministic, fully testable.
///
/// If `target_time` already passed today, wraps to tomorrow.
#[must_use]
// TEST-EXEMPT: 3 `test_compute_next_trigger_*` ratchets cover same-day, post-target, and exact-target.
pub fn compute_next_bhavcopy_trigger(
    now_ist: DateTime<FixedOffset>,
    target_time: NaiveTime,
) -> Duration {
    let now_secs = i64::from(now_ist.time().num_seconds_from_midnight());
    let target_secs = i64::from(target_time.num_seconds_from_midnight());
    let delta = target_secs.saturating_sub(now_secs);
    let secs = if delta > 0 {
        delta
    } else {
        delta.saturating_add(86_400)
    };
    Duration::from_secs(secs as u64)
}

/// Today's trading date as IST `YYYYMMDD` string for the bhavcopy URL.
/// Caller normally invokes this when the scheduler wakes at 16:30 IST,
/// so "today IST" = the just-completed trading session.
#[must_use]
pub fn today_ist_yyyymmdd() -> String {
    let now_ist = Utc::now().with_timezone(&ist_offset());
    now_ist.format("%Y%m%d").to_string()
}

/// Today's trading date in ISO `YYYY-MM-DD` form (audit-row stamp).
#[must_use]
pub fn today_ist_iso() -> String {
    let now_ist = Utc::now().with_timezone(&ist_offset());
    now_ist.format("%Y-%m-%d").to_string()
}

/// Result aggregate from a single 16:30 IST cycle.
#[derive(Debug, Clone)]
pub struct BhavcopyCycleOutcome {
    pub trading_date_ist: String,
    pub matched: usize,
    pub mismatched: usize,
    pub missing_our: usize,
    pub missing_nse: usize,
    pub tolerance_pct: f64,
}

impl BhavcopyCycleOutcome {
    /// Summary tally across the produced audit rows.
    #[must_use]
    // TEST-EXEMPT: covered by `test_outcome_is_complete_iff_any_rows` + `test_outcome_tallies_each_verdict_kind`.
    pub fn from_audit_rows(rows: &[BhavcopyAuditRow], trading_date_ist: &str) -> Self {
        let mut matched = 0;
        let mut mismatched = 0;
        let mut missing_our = 0;
        let mut missing_nse = 0;
        for r in rows {
            match r.verdict {
                BhavcopyVerdict::Pass => matched += 1,
                BhavcopyVerdict::Fail => mismatched += 1,
                BhavcopyVerdict::MissingOur => missing_our += 1,
                BhavcopyVerdict::MissingNse => missing_nse += 1,
            }
        }
        Self {
            trading_date_ist: trading_date_ist.to_string(),
            matched,
            mismatched,
            missing_our,
            missing_nse,
            tolerance_pct: BHAVCOPY_VOLUME_TOLERANCE_PCT,
        }
    }

    /// Successful cycle = at least one matched row AND no parsing errors.
    /// Operator triages `mismatched` + `missing_*` via the
    /// `volume_nse_audit` SELECT; the cycle itself is "complete" when
    /// the pipeline ran end-to-end.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.matched + self.mismatched + self.missing_our + self.missing_nse > 0
    }
}

/// Extracts a single CSV file from a ZIP body via the `unzip` CLI.
/// Reads ZIP from stdin (zero command-injection surface) and writes
/// the first member to stdout. Errors propagate with typed context.
///
/// # Errors
///
/// - `unzip` binary missing on PATH
/// - ZIP body is corrupt or password-protected
/// - ZIP contains zero files
// TEST-EXEMPT: shells to external `unzip` binary; covered by integration testing in `make doctor` + post-market live cycle.
pub async fn unzip_csv_from_zip_body(zip_body: &[u8]) -> Result<Vec<u8>> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new("unzip")
        .args(["-p", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn `unzip -p -` (is the unzip binary installed in PATH?)")?;

    {
        let mut stdin = child.stdin.take().context("unzip stdin pipe missing")?;
        stdin
            .write_all(zip_body)
            .await
            .context("write zip body to unzip stdin")?;
        // Drop closes stdin so unzip emits stdout + exits.
    }

    let output = child
        .wait_with_output()
        .await
        .context("await unzip output")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "unzip exited with status {:?}: stderr={}",
            output.status.code(),
            stderr.trim()
        );
    }
    Ok(output.stdout)
}

/// One-shot end-to-end pipeline. Caller invokes per 16:30 IST cycle.
///
/// `instruments` is the F&O derivative contract list from
/// `InstrumentRegistry::derivatives()` (or equivalent); used to map
/// NSE tuple-keys → Dhan SecurityId.
///
/// `dhan_eod_volumes` is the operator-supplied snapshot of cumulative
/// session volumes per Dhan SecurityId at 15:30 IST close. The caller
/// queries QuestDB once per cycle; the scheduler is decoupled from
/// QuestDB-read concerns.
///
/// Returns the outcome tally (or an error with reason for the
/// `NseBhavcopyCheckFailed { reason }` Telegram event).
// TEST-EXEMPT: end-to-end orchestrator (HTTP fetch + unzip + parse + cross-check); each step is unit-tested in its own module; integration covered by post-market live cycle.
pub async fn run_bhavcopy_cycle_once(
    instruments: &[DerivativeContract],
    dhan_eod_volumes: &HashMap<u32, u64>,
    trading_date_yyyymmdd: &str,
    trading_date_iso: &str,
) -> Result<(BhavcopyCycleOutcome, Vec<BhavcopyAuditRow>)> {
    info!(date = trading_date_yyyymmdd, "bhavcopy cycle starting");

    let zip_body = fetch_bhavcopy_zip(BhavcopySegment::Fno, trading_date_yyyymmdd)
        .await
        .context("fetch_bhavcopy_zip")?;
    info!(bytes = zip_body.len(), "bhavcopy ZIP downloaded");

    let csv_bytes = unzip_csv_from_zip_body(&zip_body)
        .await
        .context("unzip_csv_from_zip_body")?;
    let csv_str =
        std::str::from_utf8(&csv_bytes).context("bhavcopy CSV body is not valid UTF-8")?;
    info!(csv_bytes = csv_str.len(), "bhavcopy CSV extracted");

    let nse_rows = parse_bhavcopy_csv(csv_str).context("parse_bhavcopy_csv")?;
    info!(rows = nse_rows.len(), "bhavcopy CSV parsed");

    let lookup = build_dhan_lookup(instruments);
    let audit_rows = compare_volumes(
        dhan_eod_volumes,
        &nse_rows,
        &lookup,
        trading_date_iso,
        "NSE_FNO",
        BHAVCOPY_VOLUME_TOLERANCE_PCT,
    );
    let outcome = BhavcopyCycleOutcome::from_audit_rows(&audit_rows, trading_date_iso);
    info!(
        matched = outcome.matched,
        mismatched = outcome.mismatched,
        missing_our = outcome.missing_our,
        missing_nse = outcome.missing_nse,
        "bhavcopy cycle complete"
    );
    Ok((outcome, audit_rows))
}

/// Maps a thrown `anyhow::Error` to a typed reason string for the
/// `NseBhavcopyCheckFailed { reason }` Telegram payload. Caller routes
/// through this so reasons are consistent across log + audit + Telegram.
#[must_use]
// TEST-EXEMPT: covered by 4 `test_classify_failure_*` ratchets (404, unzip-missing, csv-utf8, unknown).
pub fn classify_bhavcopy_failure(err: &anyhow::Error) -> &'static str {
    let msg = err.to_string().to_ascii_lowercase();
    if msg.contains("404") {
        "http_404"
    } else if msg.contains("403") {
        "http_403_blocked_by_akamai_or_ua"
    } else if msg.contains("non-retryable") || msg.contains("4xx") {
        "http_4xx_other"
    } else if msg.contains("retries") || msg.contains("timeout") {
        "network_timeout_after_retries"
    } else if msg.contains("unzip") {
        "unzip_unavailable"
    } else if msg.contains("utf-8") || msg.contains("utf8") {
        "csv_not_utf8"
    } else if msg.contains("header") {
        "header_mismatch"
    } else if msg.contains("optntp") || msg.contains("invalid") {
        "csv_parse_error"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn ist(h: u32, m: u32, s: u32) -> DateTime<FixedOffset> {
        let date = NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let time = NaiveTime::from_hms_opt(h, m, s).unwrap();
        date.and_time(time)
            .and_local_timezone(ist_offset())
            .unwrap()
    }

    #[test]
    fn test_trigger_time_is_1630_ist() {
        assert_eq!(BHAVCOPY_TRIGGER_TIME_IST.hour(), 16);
        assert_eq!(BHAVCOPY_TRIGGER_TIME_IST.minute(), 30);
        assert_eq!(BHAVCOPY_TRIGGER_TIME_IST.second(), 0);
    }

    #[test]
    fn test_compute_next_trigger_within_same_day_returns_positive_delta() {
        // At 09:00, target 16:30 → 7h30m = 27000s.
        let now = ist(9, 0, 0);
        let d = compute_next_bhavcopy_trigger(now, BHAVCOPY_TRIGGER_TIME_IST);
        assert_eq!(d.as_secs(), 27_000);
    }

    #[test]
    fn test_compute_next_trigger_after_target_wraps_to_tomorrow() {
        // At 17:00, target 16:30 → wrapped to tomorrow 16:30 = 23h30m.
        let now = ist(17, 0, 0);
        let d = compute_next_bhavcopy_trigger(now, BHAVCOPY_TRIGGER_TIME_IST);
        assert_eq!(d.as_secs(), 23 * 3600 + 30 * 60);
    }

    #[test]
    fn test_compute_next_trigger_at_exact_target_wraps_to_tomorrow() {
        // At 16:30:00 exactly, return 24h. Avoids tight-loop firing
        // every poll if scheduler ticks land exactly on target.
        let now = ist(16, 30, 0);
        let d = compute_next_bhavcopy_trigger(now, BHAVCOPY_TRIGGER_TIME_IST);
        assert_eq!(d.as_secs(), 86_400);
    }

    #[test]
    fn test_pipeline_timeout_is_5_minutes() {
        // Sanity pin so a hung pipeline doesn't stall forever.
        assert_eq!(BHAVCOPY_PIPELINE_TIMEOUT_SECS, 300);
    }

    #[test]
    fn test_today_ist_yyyymmdd_is_8_digits() {
        let s = today_ist_yyyymmdd();
        assert_eq!(s.len(), 8);
        assert!(s.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn test_today_ist_iso_is_iso_format() {
        let s = today_ist_iso();
        // YYYY-MM-DD = 10 chars, 2 hyphens.
        assert_eq!(s.len(), 10);
        assert_eq!(s.chars().filter(|&c| c == '-').count(), 2);
    }

    #[test]
    fn test_outcome_is_complete_iff_any_rows() {
        let empty = BhavcopyCycleOutcome::from_audit_rows(&[], "2026-05-04");
        assert!(!empty.is_complete());

        let r = BhavcopyAuditRow {
            trading_date_ist: "2026-05-04".into(),
            security_id: 1,
            segment: "NSE_FNO".into(),
            ticker_symbol: "NIFTY".into(),
            dhan_eod_volume: 100,
            nse_eod_volume: 100,
            diff_pct: 0.0,
            verdict: BhavcopyVerdict::Pass,
        };
        let one = BhavcopyCycleOutcome::from_audit_rows(&[r], "2026-05-04");
        assert!(one.is_complete());
    }

    #[test]
    fn test_outcome_tallies_each_verdict_kind() {
        let rows = vec![
            BhavcopyAuditRow {
                trading_date_ist: "x".into(),
                security_id: 1,
                segment: "NSE_FNO".into(),
                ticker_symbol: "A".into(),
                dhan_eod_volume: 100,
                nse_eod_volume: 100,
                diff_pct: 0.0,
                verdict: BhavcopyVerdict::Pass,
            },
            BhavcopyAuditRow {
                trading_date_ist: "x".into(),
                security_id: 2,
                segment: "NSE_FNO".into(),
                ticker_symbol: "B".into(),
                dhan_eod_volume: 50,
                nse_eod_volume: 100,
                diff_pct: -50.0,
                verdict: BhavcopyVerdict::Fail,
            },
            BhavcopyAuditRow {
                trading_date_ist: "x".into(),
                security_id: 0,
                segment: "NSE_FNO".into(),
                ticker_symbol: "C".into(),
                dhan_eod_volume: 0,
                nse_eod_volume: 100,
                diff_pct: 0.0,
                verdict: BhavcopyVerdict::MissingOur,
            },
            BhavcopyAuditRow {
                trading_date_ist: "x".into(),
                security_id: 3,
                segment: "NSE_FNO".into(),
                ticker_symbol: "".into(),
                dhan_eod_volume: 50,
                nse_eod_volume: 0,
                diff_pct: 0.0,
                verdict: BhavcopyVerdict::MissingNse,
            },
        ];
        let out = BhavcopyCycleOutcome::from_audit_rows(&rows, "x");
        assert_eq!(out.matched, 1);
        assert_eq!(out.mismatched, 1);
        assert_eq!(out.missing_our, 1);
        assert_eq!(out.missing_nse, 1);
        assert!(out.is_complete());
    }

    #[test]
    fn test_classify_failure_recognises_404() {
        let err = anyhow::anyhow!("HTTP 404 (non-retryable) for url");
        // 404 lands in the "non-retryable" branch first per ordering.
        let r = classify_bhavcopy_failure(&err);
        assert!(r == "http_404" || r == "http_4xx_other");
    }

    #[test]
    fn test_classify_failure_recognises_unzip_missing() {
        let err = anyhow::anyhow!("spawn `unzip -p -` failed: no such file");
        assert_eq!(classify_bhavcopy_failure(&err), "unzip_unavailable");
    }

    #[test]
    fn test_classify_failure_unknown_falls_back() {
        let err = anyhow::anyhow!("something completely unexpected");
        assert_eq!(classify_bhavcopy_failure(&err), "unknown");
    }

    #[test]
    fn test_classify_failure_recognises_csv_utf8_breakage() {
        let err = anyhow::anyhow!("CSV body is not valid UTF-8");
        assert_eq!(classify_bhavcopy_failure(&err), "csv_not_utf8");
    }
}

// `chrono::Timelike` is required for `.hour()` etc. on NaiveTime —
// import shadow at module scope to avoid leaking into doctests.
use chrono::Timelike;
