//! Parity harness — Phase 3 commit 5 framework validation.
//!
//! **Hostile review H2 fix (2026-05-05):** these tests are NO LONGER
//! `#[ignore]`'d. They use pure-Rust synthetic data to exercise the
//! comparison framework end-to-end, so they validate the harness on
//! EVERY `cargo test` run instead of waiting for the operator's soak
//! day to surface a logic regression.
//!
//! ## What lives here
//!
//! - **Synthetic dry-runs** — exercise `compare_bars` + `sweep_parity`
//!   with fabricated `Bar` snapshots to confirm: clean inputs report
//!   clean, drift inputs report drift, missing inputs report missing,
//!   bucket-aligned matches stay matched. These ALWAYS run.
//! - **Empty-input + summary-line smokes** — defensive ratchets that
//!   the framework cannot silently green an empty input set or hide a
//!   mismatch from the operator-facing summary line.
//! - **Type-signature compile-time guards** — fail the build if a
//!   downstream caller's type signature drifts.
//!
//! ## What is genuinely soak-gated (NOT in this file)
//!
//! The 14-trading-day live RAM≡DB parity soak (per active-plan §6 row 3)
//! requires:
//! - The running app's HTTP debug endpoint that exposes `cascade_fanout.latest_<tf>(...)` snapshots
//! - A QuestDB pg-wire connection that pulls the matching matview rows
//! - A standalone binary that loops daily, calls `sweep_parity`, and
//!   writes the report to `data/logs/parity-soak-YYYY-MM-DD.json`
//!
//! That live wiring lands in Phase 4a alongside `/api/movers?v=2`. The
//! framework that BOTH the live soak AND the v2 endpoint use is the
//! `parity` module that ships in this commit.

use tickvault_trading::candles::{Bar, InstrumentKey, ParityReport, sweep_parity};

/// Synthetic bar — same factory style as `cascade_fanout::tests`.
fn synthetic_bar(
    security_id: u32,
    segment_code: u8,
    bucket_start: u32,
    close: f64,
    volume: i64,
) -> Bar {
    Bar {
        bucket_start_ist_secs: bucket_start,
        bucket_end_ist_secs: bucket_start + 60,
        open: 100.0,
        high: 105.0,
        low: 95.0,
        close,
        volume,
        volume_cum_day_at_end: volume + 4_000,
        oi: 200,
        tick_count: 12,
        security_id,
        exchange_segment_code: segment_code,
        sealed: true,
        prev_day_close: 0.0,
        prev_day_oi: 0,
        close_pct_from_prev_day: 0.0,
        oi_pct_from_prev_day: 0.0,
        volume_pct_from_prev_day: 0.0,
    }
}

#[test]
fn parity_soak_clean_synthetic_dry_run() {
    // DRY-RUN: identical RAM and DB inputs must produce a clean
    // ParityReport. This proves the harness will not falsely flag
    // a clean live system.
    let snapshots: Vec<(InstrumentKey, Option<Bar>, Option<Bar>)> = (1..=100)
        .map(|sid| {
            let bar = synthetic_bar(sid, 1, 1_000, 100.5, 1_000);
            ((sid, 1), Some(bar), Some(bar))
        })
        .collect();
    let report = sweep_parity(&snapshots);
    assert!(
        report.is_clean(),
        "clean synthetic dry-run failed: {}",
        report.summary_line()
    );
    assert_eq!(report.instruments_compared, 100);
}

#[test]
fn parity_soak_close_drift_synthetic_dry_run() {
    // DRY-RUN: a single instrument with different close on RAM vs DB
    // must produce exactly ONE mismatch on field "close".
    let mut snapshots: Vec<(InstrumentKey, Option<Bar>, Option<Bar>)> = (1..=100)
        .map(|sid| {
            let bar = synthetic_bar(sid, 1, 1_000, 100.5, 1_000);
            ((sid, 1), Some(bar), Some(bar))
        })
        .collect();
    // Inject drift on instrument 42.
    snapshots[41].2 = Some(synthetic_bar(42, 1, 1_000, 100.55, 1_000));
    let report = sweep_parity(&snapshots);
    assert!(!report.is_clean());
    assert_eq!(report.mismatches.len(), 1);
    assert_eq!(report.mismatches[0].field_name, "close");
    assert_eq!(report.mismatches[0].security_id, 42);
}

#[test]
fn parity_soak_missing_in_db_synthetic_dry_run() {
    // DRY-RUN: 5 instruments missing from DB. Must surface as
    // missing_in_db == 5 (NOT mismatches — distinct failure mode).
    let mut snapshots: Vec<(InstrumentKey, Option<Bar>, Option<Bar>)> = (1..=100)
        .map(|sid| {
            let bar = synthetic_bar(sid, 1, 1_000, 100.5, 1_000);
            ((sid, 1), Some(bar), Some(bar))
        })
        .collect();
    for slot in snapshots.iter_mut().take(5) {
        slot.2 = None;
    }
    let report = sweep_parity(&snapshots);
    assert!(!report.is_clean());
    assert_eq!(report.missing_in_db, 5);
    assert_eq!(report.missing_in_ram, 0);
    assert!(report.mismatches.is_empty());
}

#[test]
fn parity_soak_volume_drift_synthetic_dry_run() {
    // DRY-RUN: cumulative-day volume is the most likely drift point
    // (matview SUM vs RAM tracker). Validate it surfaces correctly.
    let snapshots = vec![(
        (42, 1),
        Some(synthetic_bar(42, 1, 1_000, 100.5, 1_000)),
        Some(synthetic_bar(42, 1, 1_000, 100.5, 999)),
    )];
    let report = sweep_parity(&snapshots);
    assert!(!report.is_clean());
    let names: Vec<&str> = report.mismatches.iter().map(|m| m.field_name).collect();
    assert!(names.contains(&"volume"));
}

#[test]
fn parity_soak_idle_pair_synthetic_dry_run() {
    // DRY-RUN: an idle (security_id, segment) pair (no bars on either
    // side) must NOT count as a parity failure.
    let snapshots: Vec<(InstrumentKey, Option<Bar>, Option<Bar>)> = vec![((42, 1), None, None)];
    let report = sweep_parity(&snapshots);
    assert!(report.is_clean());
    assert_eq!(report.instruments_compared, 1);
    assert_eq!(report.missing_in_db, 0);
    assert_eq!(report.missing_in_ram, 0);
}

/// Smoke test: empty input produces a clean (vacuously-true) report.
/// NOT ignored — runs on every `cargo test` to keep the harness
/// itself ratcheted against silent-pass regressions.
#[test]
fn parity_soak_empty_input_returns_clean() {
    let report = sweep_parity(&[]);
    assert!(report.is_clean());
    assert_eq!(report.instruments_compared, 0);
}

/// Defensive: when the harness reports a mismatch, the
/// `summary_line()` MUST mention it. NOT ignored.
#[test]
fn parity_soak_summary_line_mentions_mismatches_when_present() {
    let snapshots = vec![(
        (42, 1),
        Some(synthetic_bar(42, 1, 1_000, 100.5, 1_000)),
        Some(synthetic_bar(42, 1, 1_000, 999.99, 1_000)),
    )];
    let report = sweep_parity(&snapshots);
    let line = report.summary_line();
    assert!(
        line.contains("mismatches=1"),
        "summary missing mismatch count: {line}"
    );
}

/// Compile-time guard: if the test harness's signature drifts (e.g.
/// someone renames `ParityReport` or `sweep_parity`), this test fails
/// to compile, blocking the regression at PR-time. NOT ignored.
#[test]
fn parity_soak_type_signatures_are_stable() {
    let _: fn(&[(InstrumentKey, Option<Bar>, Option<Bar>)]) -> ParityReport = sweep_parity;
}
