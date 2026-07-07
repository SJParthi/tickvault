//! Telegram UX Overhaul (2026-07-07) — Lambda house-style ratchet.
//!
//! Build-failing Rust source-scan of the CloudWatch→Telegram Lambda
//! (`deploy/aws/lambda/telegram-webhook/handler.py`), the INDEPENDENT
//! second layer on top of the Python unittest lane (`make lambda-test`
//! in the Repo Guards CI job). The Python tests live next to handler.py
//! and can be edited in the same commit as a regression; this guard
//! pins the judge-contract invariants from `cargo test` on every build:
//!
//! (a) `_house_line` / `_ist_12h` / `_fold_records` exist — the
//!     house-style formatter chain cannot be silently deleted
//! (b) the legacy `Reason: {reason}` raw-threshold line never returns
//!     to the Telegram text path (NewStateReason is print()-forensics
//!     ONLY — CloudWatch Logs, never the operator surface)
//! (c) `parse_mode` never returns to the payload (plain-text mode kills
//!     the unescaped-Markdown silent-400 drop class)
//! (d) ALARM-state records are NEVER routed through the warm-cache OK
//!     suppression (`_should_suppress_ok` is consulted ONLY on the
//!     lone-OK branch — a dropped 🆘 is unacceptable)
//!
//! House pattern: source-order scans (see `episode_edit_wiring_guard.rs`).

use std::fs;

fn handler_src() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/aws/lambda/telegram-webhook/handler.py"
    ))
    .expect("deploy/aws/lambda/telegram-webhook/handler.py must exist")
}

/// Extracts the region of `src` between `start` (inclusive) and the next
/// occurrence of `end` after it (exclusive). Panics loudly on a missing
/// marker so a rename fails the build instead of vacuously passing.
fn region<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let s = src
        .find(start)
        .unwrap_or_else(|| panic!("region start marker missing: {start}"));
    let rest = &src[s..];
    let e = rest
        .find(end)
        .unwrap_or_else(|| panic!("region end marker missing: {end}"));
    &rest[..e]
}

// (a) The house-style formatter chain exists.
#[test]
fn guard_house_line_ist_12h_and_fold_records_present() {
    let src = handler_src();
    for needle in [
        "def _house_line(",
        "def _ist_12h(",
        "def _fold_records(",
        "def _should_suppress_ok(",
        "ALARM_PHRASES",
    ] {
        assert!(
            src.contains(needle),
            "handler.py house-style chain regressed — missing {needle:?}"
        );
    }
    // The fold output is what reaches Telegram (single choke point).
    assert!(
        src.contains("_fold_records(records"),
        "lambda_handler must route records through _fold_records"
    );
}

// (b) Raw CloudWatch NewStateReason never reaches the Telegram text path.
#[test]
fn guard_new_state_reason_never_in_telegram_text() {
    let src = handler_src();
    assert!(
        !src.contains("Reason: {"),
        "the legacy 'Reason: {{reason}}' raw-threshold line must never \
         return to the Telegram text path (house-style contract)"
    );
    // Every executable reference to NewStateReason must live on the
    // print() forensics line (CloudWatch Logs), never in a returned text.
    for (idx, line) in src.lines().enumerate() {
        if !line.contains("NewStateReason") {
            continue;
        }
        let trimmed = line.trim_start();
        let is_comment_or_doc =
            trimmed.starts_with('#') || trimmed.starts_with("(\"") || !line.contains("alarm.get");
        let is_forensics = line.contains("reason=") && src.contains("alarm-forensics ");
        assert!(
            is_comment_or_doc || is_forensics,
            "handler.py line {}: NewStateReason outside the forensics \
             print block: {line:?}",
            idx + 1
        );
    }
}

// (c) Plain-text mode: no markup-parsing field in the Telegram payload.
#[test]
fn guard_parse_mode_absent_from_payload() {
    let src = handler_src();
    // The QUOTED payload-key form is the regression signature (the module
    // docstring may legitimately explain the plain-text decision in prose).
    assert!(
        !src.contains("\"parse_mode\"") && !src.contains("'parse_mode'"),
        "the parse_mode payload key must never return to the Lambda \
         Telegram payload — plain-text mode kills the unescaped-Markdown \
         silent-400 drop class"
    );
    assert!(
        !src.contains("Markdown\""),
        "no Markdown mode literal may reappear in the payload"
    );
}

// (d) ALARM is never routed through the warm-cache OK suppression.
#[test]
fn guard_alarm_never_suppressed_by_warm_cache() {
    let src = handler_src();
    // Exactly ONE call site of _should_suppress_ok (plus its def).
    let def_count = src.matches("def _should_suppress_ok(").count();
    let total = src.matches("_should_suppress_ok(").count();
    assert_eq!(def_count, 1, "warm-cache helper must exist exactly once");
    assert_eq!(
        total - def_count,
        1,
        "exactly ONE _should_suppress_ok call site (the lone-OK branch)"
    );
    // The call site lives in the lone-OK fold branch…
    let lone_ok = region(&src, "# Lone OK flip", "# ALARM / INSUFFICIENT_DATA");
    assert!(
        lone_ok.contains("_should_suppress_ok("),
        "the suppression check must live in the lone-OK branch"
    );
    // …and the ALARM final-state arm never consults the cache to skip.
    let alarm_arm = region(&src, "# ALARM / INSUFFICIENT_DATA", "if lone_ok_phrases:");
    assert!(
        !alarm_arm.contains("_should_suppress_ok"),
        "ALARM records must NEVER be routed through the warm-cache suppression"
    );
    assert!(
        alarm_arm.contains("out.append(_house_line(alarm))"),
        "ALARM records must always emit an individual house line"
    );
}

// Bonus pin: the companion Python test lane exists with the six
// judge-contract test names (the CI `make lambda-test` step runs them).
#[test]
fn guard_python_test_lane_carries_contract_tests() {
    let tests = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/aws/lambda/telegram-webhook/test_handler.py"
    ))
    .expect("test_handler.py must exist");
    for name in [
        "test_house_line_no_raw_threshold_json",
        "test_ok_flip_single_line_recovered",
        "test_alarm_ok_pair_in_batch_folds_to_recovered_only",
        "test_ist_12_hour_timestamp",
        "test_alarm_never_suppressed_by_warm_cache",
        "test_unknown_alarm_name_fallback_still_plain_english",
    ] {
        assert!(
            tests.contains(name),
            "test_handler.py lost the contract test {name:?}"
        );
    }
}
