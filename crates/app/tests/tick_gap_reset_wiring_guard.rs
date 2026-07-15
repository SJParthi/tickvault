//! RETIRED GUARD — Wave-2-D Fix 2 tick-gap daily-reset wiring guard.
//!
//! **RETIRED in PR-C3 (2026-07-14).** The `TickGapDetector` (and with it
//! the 15:35 IST `reset_daily()` task in main.rs this guard pinned) was
//! DELETED per the operator's 2026-07-13 Q4-ii ruling ("tick-gap detector
//! + WS-GAP-06 deleted; the Groww feed-stall watchdog owns stall
//! detection" — `websocket-connection-scope-lock.md` "2026-07-13
//! Amendment" §B item 4). The detector was fed ONLY by the retired Dhan
//! WS pipeline (`record_tick_global` in tick_processor.rs), so after
//! PR-C2 it was a no-input shell.
//!
//! This file is retained as a dated tombstone per the C2 guard-retirement
//! precedent (retire loudly with the authority cited, never delete
//! silently). The single test below pins the RETIREMENT: the detector
//! module and its main.rs wiring must STAY deleted.

use std::path::PathBuf;

const APP_MAIN_RS: &str = "src/main.rs";

fn read_main() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(APP_MAIN_RS);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

/// PR-C3 tombstone: the tick-gap detector wiring must stay deleted.
/// Re-introducing `TickGapDetector` / its reset task in main.rs requires
/// a fresh dated operator quote in
/// `.claude/rules/project/websocket-connection-scope-lock.md` FIRST.
#[test]
fn tick_gap_detector_wiring_stays_deleted() {
    let src = read_main();
    assert!(
        !src.contains("set_global_tick_gap_detector"),
        "PR-C3 retirement violated: the global TickGapDetector install \
         reappeared in main.rs. The detector was deleted per operator \
         Q4-ii (2026-07-13); re-introduction needs a fresh dated quote \
         in websocket-connection-scope-lock.md first."
    );
    let detector_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../core/src/pipeline/tick_gap_detector.rs");
    assert!(
        !detector_path.exists(),
        "PR-C3 retirement violated: crates/core/src/pipeline/\
         tick_gap_detector.rs must stay DELETED (operator Q4-ii 2026-07-13)."
    );
}
