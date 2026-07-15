//! Feed-toggle lifecycle guard — RETUNED 2026-07-15 (Groww live-feed deletion).
//!
//! Operator lock chain: 2026-06-23 + 2026-06-24 (see git history for the full
//! preamble). The Dhan halves retired PR-C2 (2026-07-13, Dhan live-WS lane
//! deletion); the GROWW halves (`step_groww` sequence storms over
//! `groww_activation::reconcile_lane_action` / `is_dead_activation`) retired
//! 2026-07-15 with `groww_activation.rs` itself — the Groww live lane +
//! runtime toggle FSM are deleted (operator 2026-07-15: "remove the whole
//! Groww live feed; keep only spot 1m and option chain for both brokers").
//! The surviving Groww surface is the per-minute REST legs (config-gated,
//! feed-flag independent) + the `[groww_universe]` daily watch-set rider.
//!
//! What SURVIVES here: the FeedRuntimeState lane-FLAG round-trip — the /feeds
//! page + the 409 Dhan-enable refusal path still read both flags.

use tickvault_api::feed_state::{Feed, FeedRuntimeState};

#[test]
fn feed_runtime_lane_flag_round_trips_idempotently() {
    // The UI flag setters must round-trip and be idempotent on repeat — setting the
    // same value twice has no extra effect (the feed page never lies after a repeat).
    // PR-C2 (2026-07-13): the Dhan lane-running FLAG surface survives the lane
    // deletion (the /feeds page + the 409 Dhan-enable refusal still read it), so
    // BOTH feeds' flag round-trips stay pinned here.
    let state = FeedRuntimeState::default();
    for feed in [Feed::Dhan, Feed::Groww] {
        // Start NOT running (default mirrors prod for both lanes).
        assert!(!state.lane_running(feed));
    }
    // Groww: true, true (idempotent), false, false (idempotent).
    state.set_groww_lane_running(true);
    assert!(state.is_groww_lane_running());
    state.set_groww_lane_running(true);
    assert!(state.is_groww_lane_running(), "repeat-true is idempotent");
    state.set_groww_lane_running(false);
    assert!(!state.is_groww_lane_running());
    state.set_groww_lane_running(false);
    assert!(!state.is_groww_lane_running(), "repeat-false is idempotent");
    // Dhan: same round-trip (flag only — no reconciler exists since PR-C2).
    state.set_dhan_lane_running(true);
    assert!(state.is_dhan_lane_running());
    state.set_dhan_lane_running(true);
    assert!(state.is_dhan_lane_running(), "repeat-true is idempotent");
    state.set_dhan_lane_running(false);
    assert!(!state.is_dhan_lane_running());
    state.set_dhan_lane_running(false);
    assert!(!state.is_dhan_lane_running(), "repeat-false is idempotent");
}
