//! BOOT-SYMMETRY ratchet (2026-06-09).
//!
//! `main.rs` has two mutually-exclusive boot paths — slow boot (normal /
//! off-hours) and fast boot (mid-session crash restart). The post-market
//! tasks — the 15:31:30 IST end-of-day digest and the 15:31:00 IST 1-minute
//! cross-verify — used to be spawned ONLY in the slow-boot tail, so a
//! mid-session fast-boot restart silently skipped both (the operator's
//! "couldn't see the post-market cross-verification" incident).
//!
//! The fix extracts both spawns into ONE `spawn_post_market_tasks` helper that
//! BOTH boot paths call. This guard fails the build if:
//!   1. the helper is called from fewer than 2 sites (i.e. a boot path stopped
//!      calling it), or
//!   2. the end-of-day digest notify OR the cross-verify call is duplicated
//!      inline instead of living solely inside the helper (drift).
//!
//! It mirrors the repo's S6-G4 "both boot paths must be wired" boot-symmetry
//! rule for these two specific post-market tasks.

#![cfg(test)]

use std::path::PathBuf;

fn main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Section A — `spawn_post_market_tasks` must be CALLED from at least two sites
/// (the slow-boot tail + the fast-boot path). Total occurrences minus the one
/// `fn` definition = number of call sites.
#[test]
fn post_market_tasks_called_from_both_boot_paths() {
    let body = main_rs();
    let total = body.matches("spawn_post_market_tasks(").count();
    let defs = body.matches("fn spawn_post_market_tasks(").count();
    assert_eq!(
        defs, 1,
        "expected exactly one `fn spawn_post_market_tasks` definition, found {defs}"
    );
    let calls = total - defs;
    assert!(
        calls >= 2,
        "spawn_post_market_tasks must be called from BOTH boot paths (>=2 call \
         sites); found {calls}. A boot path stopped spawning the post-market \
         tasks — boot-symmetry regression."
    );
}

/// Section B — the end-of-day digest notify and the cross-verify call must live
/// ONLY inside the shared helper (exactly once each), never duplicated inline in
/// a single boot path.
#[test]
fn eod_digest_and_cross_verify_live_only_in_the_helper() {
    let body = main_rs();
    let eod = body.matches("NotificationEvent::EndOfDayDigest").count();
    assert_eq!(
        eod, 1,
        "EndOfDayDigest notify must appear exactly once (inside \
         spawn_post_market_tasks), found {eod} — inline duplication = drift"
    );
    let cv = body
        .matches("cross_verify_1m_boot::run_cross_verify_1m(")
        .count();
    assert_eq!(
        cv, 1,
        "run_cross_verify_1m must be called exactly once (inside \
         spawn_post_market_tasks), found {cv} — inline duplication = drift"
    );
}
