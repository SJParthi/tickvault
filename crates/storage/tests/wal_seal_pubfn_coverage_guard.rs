//! Pub-fn coverage restoration — stage-2 dead-WS sweep (2026-07-17).
//!
//! The sweep deleted the dead tick chain's test files
//! (`chaos_zero_tick_loss.rs`, `tick_resilience.rs`, etc.); two SURVIVING
//! pub fns lost their only NAME-pattern-matched coverage in the process
//! (the pub-fn-test-guard ratchet matches by test-fn NAME substring):
//!
//!   - `wal_suspension_watcher::WalSuspensionTracker::observe`
//!   - `seal_absorption::SealAbsorptionPipeline::rescue_in_flight`
//!
//! Both were ALREADY behaviour-covered by in-file unit tests whose names
//! do not embed the fn name; the tests below are REAL (asserting the
//! documented contracts), not name-satisfying stubs.

use tickvault_common::feed::Feed;
use tickvault_storage::seal_absorption::{SealAbsorptionPipeline, SubmitOutcome};
use tickvault_storage::wal_suspension_watcher::{WalSuspensionTracker, WalTableRow};
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

fn row(name: &str, suspended: bool) -> WalTableRow {
    WalTableRow {
        name: name.to_string(),
        suspended,
        writer_txn: None,
        sequencer_txn: None,
        error_tag: None,
        error_message: None,
    }
}

/// `observe` edge-latches per-table suspension episodes: rising edge
/// reported ONCE, steady-state suppressed, falling edge reported as
/// recovery (Rule 4 edge discipline).
#[test]
fn test_wal_suspension_tracker_observe_edge_latch_contract() {
    let mut t = WalSuspensionTracker::new();

    // Rising edge: one newly_suspended entry.
    let d = t.observe(&[row("ticks", true), row("candles_1m", false)]);
    assert_eq!(d.newly_suspended.len(), 1);
    assert_eq!(d.newly_suspended[0].name, "ticks");
    assert!(d.recovered.is_empty());
    assert_eq!(d.currently_suspended, 1);

    // Steady state: latched — no re-page.
    let d = t.observe(&[row("ticks", true), row("candles_1m", false)]);
    assert!(d.newly_suspended.is_empty());
    assert!(d.recovered.is_empty());

    // Falling edge: recovery reported once.
    let d = t.observe(&[row("ticks", false), row("candles_1m", false)]);
    assert!(d.newly_suspended.is_empty());
    assert_eq!(d.recovered, vec!["ticks".to_string()]);
    assert_eq!(d.currently_suspended, 0);
    assert_eq!(t.currently_suspended(), 0);
}

fn mk_seal() -> BufferedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = 34_200;
    state.open = 100.0;
    state.high = 105.0;
    state.low = 99.0;
    state.close = 101.0;
    state.volume = 1_234;
    BufferedSeal::new(13, 0, TfIndex::M1, state, Feed::Dhan)
}

/// `rescue_in_flight` bypasses the ring (the seal already left it) and
/// walks straight to tier-2 disk spill — the documented contract is that
/// `Buffered` is impossible and a healthy spill dir yields `Spilled`.
#[test]
fn test_seal_absorption_rescue_in_flight_spills_bypassing_ring() {
    let base = std::env::temp_dir().join(format!("tv-rescue-in-flight-cov-{}", std::process::id()));
    let spill = base.join("spill");
    let dlq = base.join("dlq");
    let p = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(4, spill.clone(), dlq.clone());

    let outcome = p.rescue_in_flight(mk_seal(), 1_767_248_400); // 2026-01-01 12:00 UTC
    assert!(
        matches!(outcome, SubmitOutcome::Spilled),
        "rescue_in_flight with a healthy spill dir must return Spilled, got {outcome:?}"
    );
    // Ring bypass: nothing re-buffered (would invert FIFO order).
    assert_eq!(
        p.ring_len(),
        0,
        "rescue_in_flight must never re-buffer into the ring"
    );

    let _ = std::fs::remove_dir_all(&base);
}
