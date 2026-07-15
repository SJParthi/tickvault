//! WAL-REPLAY-CONFIRM ratchet (zero-loss MEDIUM fix 2026-06-30) — re-shaped
//! by PR-C2 (2026-07-13, operator retirement directive per
//! websocket-connection-scope-lock.md "2026-07-13 Amendment" §B) and again
//! by PR-C3 (2026-07-14, order-update leg).
//!
//! Original shape: BOTH boot paths (slow + fast crash-recovery) had to call
//! `ws_frame_spill::confirm_replayed(...)` after re-injecting staged WAL
//! frames, each gated on a `*_reinjection_clean` flag. The fast arm, the
//! Dhan pool re-injection target, and both clean flags DIED with the Dhan
//! live-WS lane (PR-C2). PR-C3 then retired the order-update broadcast
//! drain too: the 2026-07-14 Dhan noise lock (#1532) deleted the
//! order-update WS spawn, and the trading pipeline has zero spawn sites
//! (order_side_wiring_guard), so the process-shared order-update broadcast
//! was PERMANENTLY receiver-less — draining into it was delivery theater
//! (every send() returned Err while the confirm archived the frames).
//!
//! The surviving single-boot-path shape (pinned below):
//!
//! 1. Residual WAL frames of EITHER type (LiveFeed + OrderUpdate — possible
//!    only from a pre-retirement session's WAL) have no live consumer:
//!    each type is LOUDLY counted
//!    (`tv_ws_frame_wal_reinjected_dropped_total{ws_type=...}`) BEFORE the
//!    confirm — never a silent drop (Rule 11).
//! 2. Exactly ONE `confirm_replayed` call, AFTER both loud arms — so the
//!    staged segments never re-stage on every boot (the growth loop the
//!    2026-06-30 fix closed) and are never silently dropped. The raw
//!    frames stay on disk in the WAL archive (`confirm_replayed` MOVES,
//!    never deletes) for the live-trading re-wire / forensics.

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
    // PR-C3 shape: BOTH residual arms (live_feed + order_update) must stay
    // LOUD (dropped-counter per ws_type) and must precede the single
    // confirm — a frame is only archived after being loudly accounted.
    let body = main_rs();
    let confirm = body
        .find("ws_frame_spill::confirm_replayed(")
        .expect("confirm_replayed must exist");
    for ws_type in ["live_feed", "order_update"] {
        let needle = format!(
            "\"tv_ws_frame_wal_reinjected_dropped_total\",\n            \
             \"ws_type\" => \"{ws_type}\""
        );
        let at = body.find(&needle).unwrap_or_else(|| {
            panic!(
                "the residual {ws_type} WAL arm must increment \
                 tv_ws_frame_wal_reinjected_dropped_total{{ws_type=\"{ws_type}\"}} — \
                 silence on residual frames is a Rule-11 false-OK (PR-C3, 2026-07-14)"
            )
        });
        assert!(
            at < confirm,
            "the residual {ws_type} accounting (byte {at}) must precede the \
             confirm (byte {confirm}) — archiving before the loud count \
             would hide pre-retirement frames."
        );
    }
}
