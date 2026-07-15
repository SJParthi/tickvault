//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B).
//!
//! This ratchet (§36.7 D7 / AM-r1 F7, 2026-07-10) pinned the two
//! alarm-facing consumer sites of the far-month IndexFuture exclusion —
//! the 60s tick-gap silent-gauge subtraction and the 10s SLO
//! tick_freshness filter — plus the plan-seed refresh
//! (`seed_tick_gap_detector_from_plan` → `store_far_month_future_exclusions`)
//! and the canonical-grouping/shared-singular derivation
//! (`far_month_future_sids`). ALL of those sites died with the Dhan lane:
//!
//! - the exclusion set was seeded ONLY from the deleted Dhan subscription
//!   plan, so post-retirement it is permanently empty — the surviving
//!   tick-gap gauge loop in main.rs carries the honest
//!   `let excluded_silent = 0usize;` constant with a dated inline note;
//! - the SLO publisher (and its tick_freshness filter) is PARKED per the
//!   wave-3-d-error-codes.md 2026-07-13 banner;
//! - the whole tick-gap detector (the remaining gauge loop included) is
//!   deleted in PR-C3 with WS-GAP-06 (operator Q4-ii "agreed dude" —
//!   futidx-4-error-codes.md §3: the far-month alarm-gate exclusion "dies
//!   with the tick-gap detector").
//!
//! The false-page class this guard prevented (~8 quiet far months lifting
//! the ~33 always-silent floor toward alarm threshold 40) is structurally
//! gone: there are NO Dhan-subscribed far-month SIDs anymore — the detector
//! only ever received Dhan-pipeline ticks (Phase B map, Verified).

#[test]
fn far_month_alarm_gate_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired.
}
