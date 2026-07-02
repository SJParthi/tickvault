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
