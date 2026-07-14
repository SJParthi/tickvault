//! WAL-REPLAY-CONFIRM BOOT-SYMMETRY ratchet (zero-loss MEDIUM fix 2026-06-30).
//!
//! `main.rs` has two mutually-exclusive boot paths — slow boot (normal /
//! off-hours) and fast boot (mid-session crash restart). After re-injecting
//! the staged WAL frames, each path MUST call
//! `ws_frame_spill::confirm_replayed(...)` to archive the staged segments out
//! of `replaying/`.
//!
//! The original crash-safety fix wired `confirm_replayed` into the SLOW-boot
//! path ONLY. The fast-boot (crash-recovery) path re-injected replayed frames
//! but never confirmed, so on that path the segments stayed in `replaying/`
//! and re-replayed every fast boot — bounded by QuestDB dedup, self-healing at
//! the next slow boot, but growing `replaying/` + replay cost on a crash-loop
//! and defeating the fix's own cleanup on the exact path it targets.
//!
//! This guard fails the build if `confirm_replayed` is called from fewer than
//! TWO sites (i.e. a boot path stopped confirming) — so the slow-only wiring
//! can never silently regress again. It also pins that each path gates its
//! confirm on a `*_reinjection_clean` flag, so a dropped/no-pool re-injection
//! leaves the staged segments un-confirmed in `replaying/` to re-replay next
//! boot (never confirmed early = no frame loss).
//!
//! It mirrors the repo's S6-G4 "both boot paths must be wired" boot-symmetry
//! rule (see `boot_symmetry_post_market_guard.rs`).

#![cfg(test)]

use std::path::PathBuf;

fn main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

fn dhan_rest_stack_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_rest_stack.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Section A — `confirm_replayed` must be CALLED from at least two sites
/// (the slow-boot tail + the fast-boot path). The function is defined in the
/// storage crate, so every occurrence in `main.rs` is a call site.
#[test]
fn confirm_replayed_called_from_both_boot_paths() {
    let body = main_rs();
    let calls = body.matches("ws_frame_spill::confirm_replayed(").count();
    assert!(
        calls >= 2,
        "ws_frame_spill::confirm_replayed must be called from BOTH boot paths \
         (slow-boot tail + fast-boot crash-recovery path); found {calls} call \
         site(s). A boot path stopped confirming replayed WAL segments — they \
         would stay in replaying/ and re-replay every boot on that path \
         (boot-symmetry / crash-safety regression)."
    );
}

/// Section B — each boot path must gate its confirm on a `*_reinjection_clean`
/// flag, so a dropped/no-pool re-injection leaves the staged segments
/// un-confirmed (re-replay next boot = no frame loss). The slow path uses
/// `ws_wal_replay_reinjection_clean`; the fast path uses
/// `fast_ws_wal_replay_reinjection_clean`.
#[test]
fn both_paths_gate_confirm_on_a_reinjection_clean_flag() {
    let body = main_rs();
    assert!(
        body.contains("if ws_wal_replay_reinjection_clean {"),
        "slow-boot confirm must be gated on `ws_wal_replay_reinjection_clean` — \
         confirming when a re-injection dropped frames would lose them"
    );
    assert!(
        body.contains("if fast_ws_wal_replay_reinjection_clean {"),
        "fast-boot confirm must be gated on `fast_ws_wal_replay_reinjection_clean` \
         — confirming when a re-injection dropped frames (or no pool existed) \
         would lose them; un-confirmed segments must re-replay next boot"
    );
}

/// Section C — order-runtime dry-run PR (2026-07-14): the dhan-OFF REST
/// stack is the THIRD confirm site (the two main.rs sites above are both
/// dhan-gated and dead on a dhan-off boot — the exact Phase-A residual the
/// runtime's WAL drain closes). Its confirm MUST stay CONDITIONAL: gated by
/// the pure `confirm_decision` verdict (parse-clean AND zero stale live-feed
/// frames staged), with a loud coalesced defer arm — an UNCONDITIONAL
/// whole-dir confirm would archive un-reinjected stale live-feed frames
/// (silent tick loss, design F6).
#[test]
fn rest_stack_confirm_is_gated_on_confirm_decision() {
    let body = dhan_rest_stack_rs();
    let prod = tickvault_common::source_scan::production_region(&body)
        .expect("dhan_rest_stack.rs must keep its test module"); // APPROVED: test
    // M1 (fix-round 2026-07-14): textual PRECEDENCE is not GATING — the
    // regression `let v = confirm_decision(..); confirm_replayed(..);
    // if let Defer{..} = v { … }` (an UNCONDITIONAL confirm) passed the old
    // precedence check. Pin CONTAINMENT instead: confirm_replayed occurs
    // EXACTLY ONCE in production code, strictly INSIDE the slice between
    // the `ConfirmVerdict::Confirm =>` match arm and the
    // `ConfirmVerdict::Defer` arm.
    let confirm_calls = prod
        .matches("tickvault_storage::ws_frame_spill::confirm_replayed(")
        .count();
    assert_eq!(
        confirm_calls, 1,
        "dhan_rest_stack.rs production region must contain EXACTLY ONE \
         confirm_replayed call (inside the Confirm verdict arm); found \
         {confirm_calls}"
    );
    let decision = prod
        .find("crate::order_runtime::confirm_decision(")
        .expect("rest stack must route its confirm through confirm_decision (F6)"); // APPROVED: test
    let confirm_arm = prod
        .find("ConfirmVerdict::Confirm =>")
        .expect("the Confirm match arm must exist"); // APPROVED: test
    let defer_arm = prod
        .find("ConfirmVerdict::Defer")
        .expect("the Defer match arm must exist"); // APPROVED: test
    let confirm_call = prod
        .find("tickvault_storage::ws_frame_spill::confirm_replayed(")
        .expect("confirm_replayed call present (counted above)"); // APPROVED: test
    assert!(
        decision < confirm_arm && confirm_arm < confirm_call && confirm_call < defer_arm,
        "confirm_replayed @{confirm_call} must sit INSIDE the Confirm arm \
         (decision @{decision} < Confirm-arm @{confirm_arm} < call < \
         Defer-arm @{defer_arm}) — an unconditional confirm archives \
         un-reinjected stale live-feed frames (silent tick loss)"
    );
    assert!(
        prod.contains("metrics::counter!(\"tv_wal_confirm_deferred_total\")"),
        "the defer counter disappeared — deferred confirms must be visible \
         (warn-level by design: WS-REINJECT-01 has an ERROR-level CloudWatch \
         alarm and a per-boot page for expected stale residue is pager noise)"
    );
}
