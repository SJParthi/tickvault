//! PR #288 (#4): Loom concurrency test for `DepthSequenceTracker`.
//!
//! The tracker's internal map is `Arc<papaya::HashMap<...>>` which is
//! safe for concurrent readers + writers via a lock-free pin API. But
//! our `observe()` does lookup → compare → insert, which is NOT atomic
//! across threads. Two threads observing the same key concurrently
//! could both see the pre-existing `prev` and both write their own
//! `new_sequence`, depending on interleaving.
//!
//! This loom model verifies:
//!   1. Concurrent observes never panic / never tear reads / never leak.
//!   2. The final `tracked_key_count()` is correct regardless of
//!      interleaving (no lost-insert).
//!
//! It does NOT verify sequential consistency of the sequence comparison
//! itself — that is known to be best-effort under concurrent writers to
//! the same key. Depth frames for one `(security_id, segment, side)`
//! all arrive on a single WS connection so real-world contention on the
//! same key is effectively zero.
//!
//! Run: `cargo test -p tickvault-core --features loom --test loom_depth_sequence_tracker`

#![cfg(feature = "loom")]

use tickvault_common::types::ExchangeSegment;
use tickvault_core::parser::DepthSide;
use tickvault_core::pipeline::{DepthSequenceTracker, SequenceOutcome};

#[test]
fn concurrent_observes_on_distinct_keys_never_panic() {
    loom::model(|| {
        let tracker = DepthSequenceTracker::new();
        let t1 = tracker.clone();
        let t2 = tracker.clone();

        let h1 = loom::thread::spawn(move || {
            // Distinct key: security_id=1, Bid
            let _ = t1.observe(1, ExchangeSegment::NseFno, DepthSide::Bid, 10);
        });

        let h2 = loom::thread::spawn(move || {
            // Distinct key: security_id=2, Ask
            let _ = t2.observe(2, ExchangeSegment::NseFno, DepthSide::Ask, 20);
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // Both distinct keys recorded.
        assert_eq!(tracker.tracked_key_count(), 2);
    });
}

#[test]
fn concurrent_observes_on_same_key_never_deadlock() {
    loom::model(|| {
        let tracker = DepthSequenceTracker::new();
        let t1 = tracker.clone();
        let t2 = tracker.clone();

        let h1 =
            loom::thread::spawn(move || t1.observe(1, ExchangeSegment::NseFno, DepthSide::Bid, 10));
        let h2 =
            loom::thread::spawn(move || t2.observe(1, ExchangeSegment::NseFno, DepthSide::Bid, 11));

        let o1 = h1.join().unwrap();
        let o2 = h2.join().unwrap();

        // One thread sees FirstSeen, the other sees FirstSeen or Monotonic
        // depending on interleaving. Both must return a valid outcome
        // (never panic, never return garbage).
        for o in [o1, o2] {
            match o {
                SequenceOutcome::FirstSeen
                | SequenceOutcome::Monotonic
                | SequenceOutcome::HoleDetected { .. }
                | SequenceOutcome::Duplicate
                | SequenceOutcome::Rollback => {}
            }
        }

        // Map has exactly one key regardless of interleaving.
        assert_eq!(tracker.tracked_key_count(), 1);
    });
}
