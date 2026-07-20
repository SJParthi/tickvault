//! Ratchet test: pins the wording of `wave-4-shared-preamble.md` Section 8
//! and `per-wave-guarantee-matrix.md` Section 7's "honest 100% claim" to
//! values that are actually proven by file:line evidence in the repo.
//!
//! Triggered by audit findings #2 and #3 in
//! `docs/operator/aws-readiness-audit-2026-05-03.md` which flagged that
//! the §8 prior wording cited "≤600K rescue ring capacity" and
//! "chaos-tested 70h sleep/wake" without locatable code/test evidence.
//!
//! 2026-05-03 (PR #452): rescue ring capacity bumped 600K → 2M (224 MB
//! pre-allocated VecDeque) per operator-spec'd extreme memory pressure
//! handling. Wording in `wave-4-shared-preamble.md` + `per-wave-guarantee-matrix.md`
//! updated to "2,000,000-tick" in tandem.
//!
//! 2026-05-11 (Wave 7-A4): rescue ring capacity bumped 2M → 5M for the
//! old large universe.
//!
//! 2026-05-20: rescue ring rightsized 5M → 100K (~20 MB pre-allocated
//! VecDeque) — the universe narrowed to 4 IDX_I SIDs (~15-20 tps), so
//! a 5M ring buffered ~130 hours for a feed that needs minutes.
//! Wording in both rule files updated to "100,000-tick" in tandem.
//!
//! 2026-07-18 (stage-4 dead-producer sweep): the tick rescue ring +
//! `TICK_BUFFER_CAPACITY` + `zero_tick_loss_alert_guard.rs` were DELETED
//! (their sole consumer, `tick_persistence.rs`, died in the stage-2
//! dead-WS sweep 2026-07-17). The honest-100% templates in both rule
//! files now cite the LIVE absorption tier — the 200,000-seal ring
//! (`SEAL_BUFFER_CAPACITY`, `crates/trading/src/candles/seal_ring.rs`,
//! ratcheted by `test_seal_buffer_capacity_constant_is_locked_value`)
//! — and this guard pins that wording in lockstep.
//!
//! After investigation:
//! - `SEAL_BUFFER_CAPACITY = 200_000` exists in
//!   `crates/trading/src/candles/seal_ring.rs` and is ratcheted there by
//!   `test_seal_buffer_capacity_constant_is_locked_value`.
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
// 2026-07-18 (stage-4 review MEDIUM-1): the operator charter's §C 7-row
// matrix row 1 + §F honest-100% template were re-pointed to the live
// seal-ring envelope in the same sweep — pin them here so they cannot
// silently regress to the retired tick-ring wording.
const CHARTER_PATH: &str = "../../.claude/rules/project/operator-charter-forever.md";

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
fn section8_keeps_seal_ring_claim_with_evidence_pointer() {
    for (label, path) in [
        ("preamble", PREAMBLE_PATH),
        ("matrix", MATRIX_PATH),
        ("charter", CHARTER_PATH),
    ] {
        let text = read(path);
        // 2026-07-18 (stage-4 sweep): the honest-100% template cites the
        // LIVE seal-ring envelope. The claim must cite the constant +
        // its in-file ratchet test so future readers can verify the proof.
        assert!(
            text.contains("200,000-seal ring buffer capacity"),
            "{label} ({path}) must keep the proven 200,000-seal ring \
             buffer capacity claim (the live absorption tier per the L-C1 \
             design lock in seal_ring.rs)."
        );
        assert!(
            text.contains("`SEAL_BUFFER_CAPACITY`"),
            "{label} ({path}) must cite the SEAL_BUFFER_CAPACITY constant \
             so reviewers can verify the proof."
        );
        assert!(
            text.contains("seal_ring.rs"),
            "{label} ({path}) must cite the seal_ring.rs ratchet that pins \
             the 200,000 value."
        );
        // The retired tick-ring wording must never regress back in — the
        // constant + guard it cited were deleted 2026-07-18.
        for banned in [
            "TICK_BUFFER_CAPACITY",
            "zero_tick_loss_alert_guard.rs",
            "100,000-tick ring buffer",
            "100K-tick rescue ring",
        ] {
            assert!(
                !text.contains(banned),
                "{label} ({path}) contains retired tick-ring phrase \
                 {banned:?} — the tick rescue ring + its constant + guard \
                 were deleted 2026-07-18 (stage-4 dead-producer sweep); \
                 cite SEAL_BUFFER_CAPACITY / seal_ring.rs instead."
            );
        }
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
