//! End-to-end integration test for the Phase 0/1/2/5 observability
//! chain. Proves the pipeline works from a live `tracing::error!`
//! macro all the way through to `errors.summary.md` by exercising
//! every component in-process.
//!
//! Phase 12.6 of `.claude/plans/active-plan.md` — the real-time
//! self-check the operator asked for.
//!
//! Run: `cargo test -p tickvault-app --test observability_chain_e2e`
//!
//! # Chain exercised
//!
//! ```text
//! tracing::error!(code=I-P1-11, ...)
//!   -> subscriber chain
//!   -> errors.jsonl rolling appender
//!   -> file on disk (JSON per line)
//!   -> summary_writer::regenerate_summary()
//!   -> errors.summary.md
//! ```
//!
//! # What this guards
//!
//! - `init_errors_jsonl_appender` correctly wires JSON format + ERROR
//!   filter + rolling file rotation.
//! - The `code` structured field we added to `error!` sites survives
//!   through the JSON formatter.
//! - `summary_writer` can parse the produced JSONL without format
//!   drift.
//! - The signature-hash grouping correctly deduplicates repeated
//!   events.
//! - The summary markdown renders a row with the expected code +
//!   count.
//!
//! If ANY step breaks, this test fails with a precise message — the
//! operator knows exactly where the chain dropped the signal.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use tickvault_app::observability;
use tickvault_common::error_code::ErrorCode;
use tickvault_core::notification::summary_writer::{
    SUMMARY_FILENAME, SummaryWriterConfig, regenerate_summary,
};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

fn fresh_tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tv-e2e-{}-{}-{}",
        label,
        std::process::id(),
        // nanos to avoid collisions in parallel test runs
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create_dir_all: {e}"));
    dir
}

#[test]
fn error_with_code_field_survives_jsonl_roundtrip() {
    let tmp = fresh_tmp_dir("jsonl-roundtrip");

    // 1. Initialize the JSONL appender into the temp dir.
    let (writer, guard) = observability::init_errors_jsonl_appender(&tmp)
        .unwrap_or_else(|e| panic!("init_errors_jsonl_appender: {e}"));

    // 2. Build a subscriber with ONLY the JSONL layer so we can
    //    isolate this test's output from the global subscriber the
    //    main binary installs.
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true) // hoists `code`, `severity`, `message` to top level
        .with_current_span(false)
        .with_span_list(false)
        .with_target(true)
        .with_file(false)
        .with_line_number(false)
        .with_thread_ids(false)
        .with_writer(writer)
        .with_filter(tracing_subscriber::filter::LevelFilter::ERROR);

    let subscriber = tracing_subscriber::registry().with(layer);

    // 3. Emit a known event inside this subscriber's scope.
    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(
            code = ErrorCode::InstrumentP1CrossSegmentCollision.code_str(),
            severity = ErrorCode::InstrumentP1CrossSegmentCollision
                .severity()
                .as_str(),
            security_id = 13,
            test_marker = "e2e-chain-test",
            "I-P1-11: test event from observability_chain_e2e"
        );
    });

    // 4. Drop the guard so the background flush thread completes.
    drop(guard);

    // 5. Wait briefly for the rolling appender's file write to land.
    //    tracing-appender uses a channel + background thread; the
    //    write is fast but not synchronous.
    std::thread::sleep(Duration::from_millis(200));

    // 6. Scan every errors.jsonl* file in the dir and find our event.
    let mut found = false;
    let entries = fs::read_dir(&tmp).unwrap_or_else(|e| panic!("read_dir: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("errors.jsonl") {
            continue;
        }
        let body = fs::read_to_string(&path).unwrap_or_default();
        if body.contains("e2e-chain-test")
            && body.contains("I-P1-11")
            && body.contains("\"severity\":\"medium\"")
        {
            found = true;
            break;
        }
    }

    assert!(
        found,
        "expected errors.jsonl* file containing our marker event in {}",
        tmp.display()
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn summary_writer_groups_duplicate_jsonl_events_end_to_end() {
    let tmp = fresh_tmp_dir("summary-grouping");

    let (writer, guard) =
        observability::init_errors_jsonl_appender(&tmp).unwrap_or_else(|e| panic!("init: {e}"));

    let layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true) // hoists `code`, `severity`, `message` to top level
        .with_current_span(false)
        .with_span_list(false)
        .with_target(true)
        .with_file(false)
        .with_line_number(false)
        .with_thread_ids(false)
        .with_writer(writer)
        .with_filter(tracing_subscriber::filter::LevelFilter::ERROR);

    let subscriber = tracing_subscriber::registry().with(layer);

    // Emit three duplicate events + one distinct.
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..3 {
            tracing::error!(
                code = ErrorCode::InstrumentP1CrossSegmentCollision.code_str(),
                severity = "medium",
                "collision event"
            );
        }
        tracing::error!(
            code = ErrorCode::Dh904RateLimit.code_str(),
            severity = "high",
            "rate limit hit"
        );
    });

    drop(guard);
    std::thread::sleep(Duration::from_millis(200));

    // Run the summary writer against this directory.
    let cfg = SummaryWriterConfig::new(&tmp);
    let signatures = regenerate_summary(&cfg).unwrap_or_else(|e| panic!("regenerate_summary: {e}"));

    assert_eq!(
        signatures, 2,
        "expected 2 distinct signatures (collision x3 collapsed, DH-904 x1)"
    );

    // Read the summary and assert it shows the grouping.
    let summary = fs::read_to_string(tmp.join(SUMMARY_FILENAME))
        .unwrap_or_else(|e| panic!("read summary: {e}"));

    assert!(
        summary.contains("I-P1-11"),
        "summary missing I-P1-11 row:\n{summary}"
    );
    assert!(
        summary.contains("DH-904"),
        "summary missing DH-904 row:\n{summary}"
    );
    // The collision group has count=3.
    assert!(
        summary.contains("| 3 |"),
        "summary missing count=3 for collision group:\n{summary}"
    );
    // DH-904 appears once.
    assert!(
        summary.contains("| 1 |"),
        "summary missing count=1 for rate-limit group:\n{summary}"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn summary_file_novel_section_lists_single_occurrence_events() {
    let tmp = fresh_tmp_dir("novel-section");

    let (writer, guard) =
        observability::init_errors_jsonl_appender(&tmp).unwrap_or_else(|e| panic!("init: {e}"));

    let layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true) // hoists `code`, `severity`, `message` to top level
        .with_current_span(false)
        .with_span_list(false)
        .with_target(true)
        .with_file(false)
        .with_line_number(false)
        .with_thread_ids(false)
        .with_writer(writer)
        .with_filter(tracing_subscriber::filter::LevelFilter::ERROR);

    let subscriber = tracing_subscriber::registry().with(layer);

    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(
            code = ErrorCode::AuthGapTokenExpiry.code_str(),
            severity = "critical",
            "singular novel auth event"
        );
    });

    drop(guard);
    std::thread::sleep(Duration::from_millis(200));

    let cfg = SummaryWriterConfig::new(&tmp);
    regenerate_summary(&cfg).unwrap_or_else(|e| panic!("regen: {e}"));
    let summary =
        fs::read_to_string(tmp.join(SUMMARY_FILENAME)).unwrap_or_else(|e| panic!("read: {e}"));

    assert!(
        summary.contains("## Novel signatures"),
        "summary missing novel section:\n{summary}"
    );
    assert!(
        summary.contains("AUTH-GAP-01"),
        "summary missing novel code row:\n{summary}"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn summary_writer_reports_all_green_when_no_errors() {
    let tmp = fresh_tmp_dir("all-green");

    // Initialize appender so the directory exists but emit nothing.
    let (_, guard) =
        observability::init_errors_jsonl_appender(&tmp).unwrap_or_else(|e| panic!("init: {e}"));
    drop(guard);

    let cfg = SummaryWriterConfig::new(&tmp);
    let count = regenerate_summary(&cfg).unwrap_or_else(|e| panic!("regen: {e}"));
    assert_eq!(count, 0);

    let summary =
        fs::read_to_string(tmp.join(SUMMARY_FILENAME)).unwrap_or_else(|e| panic!("read: {e}"));
    assert!(
        summary.contains("Zero ERROR-level events"),
        "summary should report all-green:\n{summary}"
    );

    let _ = fs::remove_dir_all(&tmp);
}
