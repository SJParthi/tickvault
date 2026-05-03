//! Ratchet test: pins the wording of `wave-4-shared-preamble.md` Section 8
//! and `per-wave-guarantee-matrix.md` Section 7's "honest 100% claim" to
//! values that are actually proven by file:line evidence in the repo.
//!
//! Triggered by audit findings #2 and #3 in
//! `docs/operator/aws-readiness-audit-2026-05-03.md` which flagged that
//! the §8 prior wording cited "≤600K rescue ring capacity" and
//! "chaos-tested 70h sleep/wake" without locatable code/test evidence.
//!
//! After investigation:
//! - `TICK_BUFFER_CAPACITY = 600_000` exists in
//!   `crates/common/src/constants.rs` and is ratcheted by
//!   `crates/storage/tests/zero_tick_loss_alert_guard.rs`.
//! - The 65h Fri 16:00 → Mon 09:00 IST weekend sleep/wake test
//!   exists at `crates/core/tests/ws_sleep_resilience.rs:93,173`.
//!   The literal "70h" wording was off by 5 hours; the real chaos
//!   test is 65h.
//! - The >65h long-holiday wake (e.g. Fri → Tue across a Mon holiday,
//!   ~92h) is NOT yet pinned and is queued as Wave-6 item W6-2.
//!
//! These tests fail the build if the §8 wording silently regresses
//! back to unproven claims.

use std::fs;
use std::path::PathBuf;

const PREAMBLE_PATH: &str = "../../.claude/rules/project/wave-4-shared-preamble.md";
const MATRIX_PATH: &str = "../../.claude/rules/project/per-wave-guarantee-matrix.md";

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

#[test]
fn section8_does_not_claim_unproven_70h_chaos() {
    for (label, path) in [("preamble", PREAMBLE_PATH), ("matrix", MATRIX_PATH)] {
        let text = read(path);
        // The literal "70h sleep" / "70 hour sleep" wording is the
        // banned phrasing — the actual chaos test simulates 65h.
        for banned in ["chaos-tested 70h sleep", "70h sleep/wake", "70 hour sleep"] {
            assert!(
                !text.contains(banned),
                "{label} ({path}) contains banned unproven phrase {banned:?}. \
                 The real test simulates 65h Fri→Mon weekend; for >65h \
                 holiday-weekend dormant sleep see Wave-6 backlog item W6-2."
            );
        }
    }
}

#[test]
fn section8_keeps_600k_rescue_ring_claim_with_evidence_pointer() {
    for (label, path) in [("preamble", PREAMBLE_PATH), ("matrix", MATRIX_PATH)] {
        let text = read(path);
        // The 600_000-tick ring buffer claim must remain because it IS
        // proven, AND it must cite the constant + ratchet test so
        // future readers can verify the proof in one grep.
        assert!(
            text.contains("600,000-tick ring buffer capacity"),
            "{label} ({path}) must keep the proven 600,000-tick ring \
             buffer capacity claim with the new precise wording."
        );
        assert!(
            text.contains("`TICK_BUFFER_CAPACITY`"),
            "{label} ({path}) must cite the TICK_BUFFER_CAPACITY constant \
             so reviewers can verify the proof."
        );
        assert!(
            text.contains("zero_tick_loss_alert_guard.rs"),
            "{label} ({path}) must cite the ratchet test that pins the \
             600,000 value."
        );
    }
}

#[test]
fn section8_keeps_65h_weekend_chaos_claim_with_evidence_pointer() {
    for (label, path) in [("preamble", PREAMBLE_PATH), ("matrix", MATRIX_PATH)] {
        let text = read(path);
        assert!(
            text.contains("65h Fri 16:00 IST"),
            "{label} ({path}) must cite the 65h weekend sleep/wake \
             chaos test by its actual duration (Fri 16:00 → Mon 09:00 \
             IST), not the legacy 70h claim."
        );
        assert!(
            text.contains("ws_sleep_resilience.rs"),
            "{label} ({path}) must cite the ws_sleep_resilience.rs test \
             file as evidence."
        );
    }
}

#[test]
fn section8_points_at_wave6_backlog_for_holiday_wake() {
    for (label, path) in [("preamble", PREAMBLE_PATH), ("matrix", MATRIX_PATH)] {
        let text = read(path);
        assert!(
            text.contains("Wave-6") || text.contains("W6-2"),
            "{label} ({path}) must point at Wave-6 backlog for the >65h \
             long-weekend holiday wake test that is not yet pinned."
        );
    }
}

#[test]
fn section8_still_rejects_literal_never_promises() {
    // Existing rule (predates this audit): \"WebSocket never disconnects\"
    // and \"QuestDB never fails\" without envelope qualifier are REJECT
    // IN REVIEW. Pin that REJECT clause stays.
    let preamble = read(PREAMBLE_PATH);
    assert!(
        preamble.contains("REJECT IN REVIEW"),
        "wave-4-shared-preamble.md §8 must keep the REJECT IN REVIEW \
         clause for literal 'never' promises without envelope."
    );
}
