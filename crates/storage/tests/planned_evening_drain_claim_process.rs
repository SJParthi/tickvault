//! The evening drain's planned-restart claim, in its own test process.
//!
//! Regression: 2026-11-11 — PR AA-G review 2. The boot's STAGE-C replay runs
//! BEFORE the catch-up drain raises the frame cap, so on the planned
//! evening-drain restart only the claim (`note_planned_evening_drain_claim`,
//! called by `evening_drain::claim_planned_boot`) can tell that replay it is
//! on the planned boot. The unit test drives a private slot, so a regression
//! that dropped the claim arm from `planned_evening_drain_boot()` (or made
//! the note a no-op, or point at another static) passed every test, and the
//! STAGE-C deferral line would page again on every planned restart. This
//! file sets the PROCESS flag, which is safe only in its own test binary
//! (nothing else here reads it), and checks the process accessor.

use tickvault_storage::wal_replay_frame_cap::{
    note_planned_evening_drain_claim, planned_evening_drain_boot, replay_frame_cap_this_boot,
};

#[test]
fn test_the_claim_alone_makes_the_process_a_planned_boot() {
    // The frame cap is never raised in this process, so before the claim
    // nothing says "planned" (an ordinary boot keeps the ERROR and the page).
    let cap_before = replay_frame_cap_this_boot();
    assert!(
        !planned_evening_drain_boot(),
        "no claim and the default cap: not the planned boot"
    );

    // An unplanned claim records nothing.
    note_planned_evening_drain_claim(false);
    assert!(
        !planned_evening_drain_boot(),
        "an unplanned claim never marks the process as planned"
    );

    // The planned claim alone (the STAGE-C replay, before the cap is raised)
    // makes the process the planned boot.
    note_planned_evening_drain_claim(true);
    assert_eq!(
        replay_frame_cap_this_boot(),
        cap_before,
        "the claim does not raise the cap; the claim arm alone carries the verdict"
    );
    assert!(
        planned_evening_drain_boot(),
        "the claim alone is the planned boot (STAGE-C runs before the cap is raised)"
    );

    // Never cleared within a process: one process is one boot.
    note_planned_evening_drain_claim(false);
    assert!(
        planned_evening_drain_boot(),
        "a later unplanned note never clears the claim"
    );
}
