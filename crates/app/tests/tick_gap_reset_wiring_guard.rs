//! Tick-gap detector governance guard.
//!
//! # History — read this before changing the assertions
//!
//! **2026-07-13 (operator Q4-ii):** the `TickGapDetector` was DELETED. It was
//! fed ONLY by the Dhan WS pipeline, which retired the same day, so it became
//! a no-input shell. This file was then a tombstone pinning the deletion.
//!
//! **2026-08-09 (operator, dated quote):** the Dhan live main-feed WebSocket
//! was REVIVED, and the revival section of
//! `.claude/rules/project/websocket-connection-scope-lock.md` names the
//! tick-gap detector explicitly in its "What the revival MUST rebuild" list.
//! The tombstone's own release condition — "re-introduction needs a fresh
//! dated quote in websocket-connection-scope-lock.md first" — is therefore
//! SATISFIED, and the detector is rebuilt at
//! `crates/core/src/pipeline/tick_gap_detector.rs`.
//!
//! # Why this guard still exists
//!
//! The obvious move on a superseded ratchet is to delete it. That would be
//! wrong: the property worth enforcing was never "this file must not exist",
//! it was "this file must not exist WITHOUT operator authority". Deleting the
//! guard discards the second half and leaves nothing checking that the
//! authority is real.
//!
//! So the guard is INVERTED rather than removed. It now enforces the same
//! governance rule from the other side: if the detector exists, the scope-lock
//! must carry the authorization that permits it. A future session that
//! re-adds the detector after someone reverts the authorization gets the same
//! build failure the original tombstone would have produced.
//!
//! This also matters because the detector is not merely permitted, it is
//! REQUIRED by the revival: the Dhan feed carries no sequence number, so
//! without synthesised loss detection there is no way to know a packet was
//! missed at all.

use std::path::PathBuf;

const APP_MAIN_RS: &str = "src/main.rs";
const DETECTOR_REL: &str = "../core/src/pipeline/tick_gap_detector.rs";
const SCOPE_LOCK_REL: &str = "../../.claude/rules/project/websocket-connection-scope-lock.md";

fn manifest_join(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn read(rel: &str) -> String {
    let path = manifest_join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

/// The detector may exist ONLY while the scope-lock carries the dated
/// authorization that permits it.
///
/// Both directions are checked, so neither the code nor the rule file can
/// drift away from the other unnoticed.
#[test]
fn tick_gap_detector_exists_only_with_dated_operator_authority() {
    let detector_exists = manifest_join(DETECTOR_REL).exists();
    let scope_lock = read(SCOPE_LOCK_REL);

    // The authorization is the 2026-08-09 revival section naming this module
    // in its MUST-rebuild list.
    let authorized = scope_lock.contains("What the revival MUST rebuild")
        && scope_lock.contains("the tick-gap detector");

    if detector_exists {
        assert!(
            authorized,
            "crates/core/src/pipeline/tick_gap_detector.rs exists, but \
             websocket-connection-scope-lock.md no longer carries the dated \
             operator authorization for it.\n\n\
             The detector was deleted 2026-07-13 (operator Q4-ii) and rebuilt \
             only because the 2026-08-09 Dhan live-WS revival quote names it \
             in 'What the revival MUST rebuild'. If that authorization was \
             removed or reworded, the detector must go with it -- or the rule \
             file must be restored. Do not weaken this test to make the build \
             pass."
        );
    } else {
        assert!(
            !authorized,
            "websocket-connection-scope-lock.md REQUIRES the tick-gap \
             detector as part of the Dhan live-feed revival, but \
             crates/core/src/pipeline/tick_gap_detector.rs does not exist.\n\n\
             The Dhan feed has NO sequence number, so without this module \
             there is no way to detect a missed packet at all. Rebuild it, or \
             amend the rule file with a fresh dated quote."
        );
    }
}

/// The 2026-07-13 deletion also removed the global-install + 15:35 IST
/// `reset_daily()` wiring from `main.rs`.
///
/// The revival authorizes the MODULE; it does not by itself authorize any
/// particular wiring shape, and nothing consumes the detector in production
/// yet. This pins that honestly: when the wiring lands it should arrive with
/// its own tests, and whoever adds it must update this assertion deliberately
/// rather than discovering later that a guard quietly stopped meaning
/// anything.
#[test]
fn tick_gap_detector_global_install_is_not_yet_wired_in_main() {
    let src = read(APP_MAIN_RS);
    assert!(
        !src.contains("set_global_tick_gap_detector"),
        "main.rs installs a global TickGapDetector. That is plausibly correct \
         now that the 2026-08-09 revival authorizes the module -- but the \
         wiring must land WITH its own coverage (who feeds it, when \
         reset_daily runs, what consumes the reports). Update this test \
         deliberately as part of that change."
    );
}
