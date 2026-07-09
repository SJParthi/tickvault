//! Source-scan ratchet — AGGREGATOR-DROP-01 paging wired end-to-end
//! (2026-07-09, PR #2 of the candle-drop paging sequence).
//!
//! The 2026-07-09 audit confirmed the Severity::Critical sealed-candle
//! drop code (`ErrorCode::AggregatorDrop01` — ring + spill + DLQ all
//! failed, the ONLY silent-data-loss path for sealed candles) paged
//! NOBODY: no `error_code_alerts` map entry, and its companion counter
//! `tv_seal_writer_drain_total{kind="dropped"}` was not exported to
//! CloudWatch. This guard pins the full chain in lockstep so a rename /
//! deletion on ANY side (Rust emit, main.rs pre-registration, either
//! terraform file) fails the build:
//!
//! 1. main.rs pre-registers the `kind="dropped"` series at 0 AFTER the
//!    metrics recorder installs (the round-5 feed-stall first-sample
//!    baseline lesson — a pre-install registration resolves to the no-op
//!    recorder; a MISSING registration makes the counter alarm dead on
//!    arrival for a single-episode drop, the dominant shape).
//! 2. `crates/storage/src/seal_writer_loop.rs` still fires
//!    `error!(code = ErrorCode::AggregatorDrop01.code_str(), …)` and the
//!    `kind => "dropped"` counter increment in its PRODUCTION region.
//! 3. `deploy/aws/terraform/error-code-alarms.tf` carries the
//!    aggregator-drop-01 map entry whose filter pattern matches the REAL
//!    emitted code string (built from `ErrorCode::AggregatorDrop01
//!    .code_str()` at test time — a code-string rename breaks this test,
//!    never silently blinds the filter) at ERROR level, with
//!    `ok_recovery = false` (permanent loss — no false-recovery OK page).
//! 4. `deploy/aws/terraform/seal-drop-alarm.tf` extracts the real metric
//!    field, kind-sliced to `dropped`, into the DERIVED
//!    `tv_seal_writer_drain_dropped_total` name, and the alarm reads that
//!    same name (Sum / 300s / threshold 1 / notBreaching / no ok_actions).
//!
//! House precedents: `fast_boot_token_validation_wiring_guard.rs`
//! (main.rs source-order scan from an app integration test),
//! `groww_sidecar_supervisor.rs::
//! test_stall_restart_counter_is_preregistered_after_recorder_install`
//! (post-install registration ordering),
//! `cw_agent_selector_lockstep_guard.rs` (terraform<->Rust lockstep).
//! Runbook: `.claude/rules/project/wave-6-error-codes.md`.

use std::fs;
use std::path::PathBuf;

use tickvault_common::error_code::ErrorCode;

fn read_repo_file(rel_from_app: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_app);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())) // APPROVED: test
}

/// Strip `//`-line-comments (treating `://` URL scheme separators as code,
/// the `http_client_fallback_guard.rs` precedent) so a comment mentioning a
/// needle can never satisfy the scan.
fn strip_line_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Everything strictly before the first top-level `#[cfg(test)]` — the
/// production region (robust to code after `mod tests`).
fn production_region(body: &str) -> &str {
    body.split("#[cfg(test)]").next().unwrap_or(body)
}

/// Whitespace-compacted form so rustfmt / terraform-fmt re-wrapping and
/// alignment can never hide a real site from a contiguous needle.
fn compact(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

const DRAIN_COUNTER: &str = "tv_seal_writer_drain_total";
const DERIVED_METRIC: &str = "tv_seal_writer_drain_dropped_total";

#[test]
fn test_seal_drop_counter_is_preregistered_after_recorder_install() {
    let main_src = strip_line_comments(&read_repo_file("src/main.rs"));
    let install = main_src
        .find("observability::init_metrics(")
        .expect("the metrics recorder install site must exist in main.rs"); // APPROVED: test
    let reg = main_src.find(&format!("\"{DRAIN_COUNTER}\"")).expect(
        "main.rs must pre-register tv_seal_writer_drain_total kind=dropped post-install \
         (the boot-time registration the tv-<env>-seal-writer-dropped pager depends on: \
         the CW agent's delta pipeline drops the series' first sample as its baseline, \
         so a lazily-born dropped-counter loses the session's FIRST drop entirely)",
    ); // APPROVED: test
    assert!(
        install < reg,
        "the seal-drop counter registration in main.rs must come AFTER \
         observability::init_metrics — a pre-install registration resolves to the \
         no-op recorder and registers nothing (the VOID round-4 class)"
    );
    // The registration must target the `dropped` kind specifically and be a
    // 0-increment (a real increment at boot would be a false drop signal).
    let compact_main = compact(&main_src);
    assert!(
        compact_main.contains(&format!(
            "counter!(\"{DRAIN_COUNTER}\",\"kind\"=>\"dropped\").increment(0)"
        )),
        "main.rs must pre-register EXACTLY the kind=\"dropped\" series at 0 — \
         the only kind that feeds the seal-writer-dropped alarm"
    );
}

#[test]
fn test_seal_writer_loop_still_emits_coded_error_and_dropped_counter() {
    let src = read_repo_file("../storage/src/seal_writer_loop.rs");
    let prod = compact(&strip_line_comments(production_region(&src)));
    assert!(
        prod.contains("ErrorCode::AggregatorDrop01.code_str()"),
        "seal_writer_loop.rs production region must still fire \
         error!(code = ErrorCode::AggregatorDrop01.code_str(), …) on truly-dropped \
         seals — the errcode-aggregator-drop-01 log-filter alarm depends on it"
    );
    assert!(
        prod.contains(&format!(
            "counter!(\"{DRAIN_COUNTER}\",\"kind\"=>\"dropped\")"
        )),
        "seal_writer_loop.rs production region must still increment \
         tv_seal_writer_drain_total kind=dropped — the tv-<env>-seal-writer-dropped \
         counter alarm depends on it"
    );
}

#[test]
fn test_errcode_alarm_map_contains_aggregator_drop_01() {
    let tf = read_repo_file("../../deploy/aws/terraform/error-code-alarms.tf");
    // Build the filter needle from the REAL code string so a Rust-side
    // code_str rename breaks THIS test instead of silently blinding the
    // CloudWatch filter. Raw tf bytes carry literal backslash-quote
    // sequences: { $.code = \"AGGREGATOR-DROP-01\" && $.level = \"ERROR\" }
    let code = ErrorCode::AggregatorDrop01.code_str();
    assert_eq!(
        code, "AGGREGATOR-DROP-01",
        "code_str drifted — update the terraform filter pattern in the same PR"
    );
    let pattern_needle = format!("{{ $.code = \\\"{code}\\\" && $.level = \\\"ERROR\\\" }}");
    assert!(
        tf.contains(&pattern_needle),
        "error-code-alarms.tf must carry the aggregator-drop-01 filter pattern \
         matching the emitted code string at ERROR level; expected pattern \
         body: {pattern_needle}"
    );
    // The entry must suppress the false-recovery OK page (permanent loss).
    let entry_start = tf
        .find("\"aggregator-drop-01\"")
        .expect("error-code-alarms.tf must define the aggregator-drop-01 map entry"); // APPROVED: test
    let entry_window = &tf[entry_start..(entry_start + 2_500).min(tf.len())];
    assert!(
        compact(entry_window).contains("ok_recovery=false"),
        "aggregator-drop-01 must set ok_recovery = false — a dropped sealed candle \
         never comes back, so an auto-OK page would be a Rule-11 false recovery"
    );
}

#[test]
fn test_seal_drop_fallback_filter_matches_emitted_series_shape() {
    let tf = read_repo_file("../../deploy/aws/terraform/seal-drop-alarm.tf");
    // Filter pattern: real metric field + the dropped kind-slice (raw tf
    // bytes carry \"dropped\" as backslash-quote sequences).
    let pattern_needle = format!("{{ $.{DRAIN_COUNTER} = * && $.kind = \\\"dropped\\\" }}");
    assert!(
        tf.contains(&pattern_needle),
        "seal-drop-alarm.tf filter must match the real /metrics event shape: \
         the {DRAIN_COUNTER} field AND $.kind = dropped (without the kind clause \
         the busy submitted/flushed series would inflate the Sum); expected \
         pattern body: {pattern_needle}"
    );
    let compact_tf = compact(&tf);
    assert!(
        compact_tf.contains(&format!("value=\"$.{DRAIN_COUNTER}\"")),
        "the metric transformation must extract the per-scrape delta value from \
         the real counter field $.{DRAIN_COUNTER}"
    );
    // The DERIVED name must be used by BOTH the transformation and the alarm
    // (a kind-restricted slice must never reuse the raw counter identity —
    // a future unfiltered extraction would double-count under Sum).
    assert!(
        compact_tf.contains(&format!("name=\"{DERIVED_METRIC}\""))
            && compact_tf.contains(&format!("metric_name=\"{DERIVED_METRIC}\"")),
        "both the metric_transformation name and the alarm metric_name must use \
         the derived {DERIVED_METRIC} identity"
    );
    for needle in [
        "statistic=\"Sum\"",
        "period=300",
        "threshold=1",
        "treat_missing_data=\"notBreaching\"",
        "ok_actions=[]",
    ] {
        assert!(
            compact_tf.contains(needle),
            "seal-drop-alarm.tf alarm shape drifted — missing `{needle}` \
             (Sum of per-scrape deltas, aligned 300s window, a single dropped \
             seal pages, sparse-metric missing data never breaches, and no \
             false-recovery OK page)"
        );
    }
}
