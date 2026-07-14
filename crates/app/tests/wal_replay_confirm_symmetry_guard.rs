//! WAL-REPLAY-CONFIRM ratchet (zero-loss MEDIUM fix 2026-06-30) — re-shaped
//! by PR-C2 (2026-07-13, operator retirement directive per
//! websocket-connection-scope-lock.md "2026-07-13 Amendment" §B).
//!
//! Original shape: BOTH boot paths (slow + fast crash-recovery) had to call
//! `ws_frame_spill::confirm_replayed(...)` after re-injecting staged WAL
//! frames, each gated on a `*_reinjection_clean` flag. The fast arm, the
//! Dhan pool re-injection target, and both clean flags DIED with the Dhan
//! live-WS lane. The surviving single-boot-path shape (pinned below):
//!
//! 1. The order-update WAL leg drains synchronously into the order-update
//!    broadcast (a direct send — no bounded channel that can drop, so
//!    "clean" is structural, not a flag).
//! 2. Residual LiveFeed WAL frames from a pre-retirement session have no
//!    re-injection target — they are LOUDLY counted
//!    (`tv_ws_frame_wal_reinjected_dropped_total{ws_type="live_feed"}`) and
//!    archived WITH the segments (the raw frames stay on disk in the WAL
//!    archive — `confirm_replayed` MOVES, never deletes).
//! 3. Exactly ONE `confirm_replayed` call, AFTER both legs settle — so the
//!    staged segments never re-stage on every boot (the growth loop the
//!    2026-06-30 fix closed) and are never silently dropped.

#![cfg(test)]

use std::path::PathBuf;

fn main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

#[test]
fn confirm_replayed_called_from_both_boot_paths() {
    // PR-C2: single boot path → exactly one confirm site.
    let body = main_rs();
    let calls = body.matches("ws_frame_spill::confirm_replayed(").count();
    assert_eq!(
        calls, 1,
        "ws_frame_spill::confirm_replayed must be called exactly once (the \
         single boot path since PR-C2); found {calls}. Zero = staged WAL \
         segments re-stage + re-replay every boot (the 2026-06-30 growth \
         loop); two = a boot-path fork crept back without updating this \
         guard."
    );
}

#[test]
fn both_paths_gate_confirm_on_a_reinjection_clean_flag() {
    // PR-C2 re-shape: the per-path `*_reinjection_clean` flags died with
    // the fast arm + the Dhan pool. The surviving safety shape: the
    // residual-LiveFeed arm must stay LOUD (counted, never a silent drop)
    // and must precede the confirm — a frame is only archived after being
    // drained (order-update) or loudly accounted (live-feed residue).
    let body = main_rs();
    let residual_count = body
        .find("\"ws_type\" => \"live_feed\"")
        .expect("the residual LiveFeed WAL arm must count its frames");
    let confirm = body
        .find("ws_frame_spill::confirm_replayed(")
        .expect("confirm_replayed must exist");
    assert!(
        residual_count < confirm,
        "the residual LiveFeed accounting (byte {residual_count}) must \
         precede the confirm (byte {confirm}) — archiving before the loud \
         count would hide pre-retirement frames."
    );
    assert!(
        body.contains("tv_ws_frame_wal_reinjected_dropped_total"),
        "the residual LiveFeed arm must increment \
         tv_ws_frame_wal_reinjected_dropped_total — silence on residual \
         frames is a Rule-11 false-OK."
    );
}

// Section C (the order-runtime dry-run PR's rest-stack conditional-confirm
// pin) was REMOVED 2026-07-14 on the merge with main's #1532 Dhan noise
// lock: the dhan-OFF REST stack no longer drains or confirms order-update
// WAL segments at all (socket-free shape — the whole capture/drain/confirm
// surface is gated behind a fresh dated operator quote per
// dhan-rest-only-noise-lock-2026-07-14 §3). The two main.rs sites above are
// the complete confirm surface again; the stack's WAL-free shape is pinned
// by `test_rest_stack_wires_order_runtime` in dhan_rest_stack.rs.
