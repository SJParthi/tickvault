//! Wave 2 Item 8.6 — Loom concurrency test for `TickGapDetector`.
//!
//! The detector's internal map is `papaya::HashMap` which is safe for
//! concurrent readers + writers via a lock-free pin API. But hot-path
//! `record_tick` is invoked from the SAME tokio task (the tick processor)
//! today — concurrent contention on the same key is rare in production.
//! This loom model verifies that even under adversarial interleavings:
//!
//!   1. Concurrent `record_tick` calls never panic / never tear / never deadlock.
//!   2. After both threads complete, `len()` reflects the correct number
//!      of distinct keys regardless of interleaving.
//!   3. `scan_gaps` from one thread concurrent with `record_tick` from
//!      another never produces UB.
//!
//! Run: `cargo test -p tickvault-core --features loom --test loom_tick_gap_detector`

#![cfg(feature = "loom")]

use std::sync::Arc;
use std::time::Instant;

use tickvault_common::types::ExchangeSegment;
use tickvault_core::pipeline::TickGapDetector;

#[test]
fn concurrent_record_tick_on_distinct_keys_never_panics() {
    loom::model(|| {
        let detector = Arc::new(TickGapDetector::new(30));
        let d1 = Arc::clone(&detector);
        let d2 = Arc::clone(&detector);
        let now = Instant::now();

        let h1 = loom::thread::spawn(move || {
            d1.record_tick(1, ExchangeSegment::IdxI, now);
        });
        let h2 = loom::thread::spawn(move || {
            d2.record_tick(2, ExchangeSegment::NseFno, now);
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // Both distinct keys recorded — composite-key keeps them separate.
        assert_eq!(detector.len(), 2);
    });
}

#[test]
fn concurrent_record_tick_on_same_key_never_deadlocks() {
    loom::model(|| {
        let detector = Arc::new(TickGapDetector::new(30));
        let d1 = Arc::clone(&detector);
        let d2 = Arc::clone(&detector);
        let now = Instant::now();

        let h1 = loom::thread::spawn(move || {
            d1.record_tick(1, ExchangeSegment::IdxI, now);
        });
        let h2 = loom::thread::spawn(move || {
            d2.record_tick(1, ExchangeSegment::IdxI, now);
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // Same composite key → exactly one entry regardless of interleaving.
        assert_eq!(detector.len(), 1);
    });
}

#[test]
fn concurrent_record_and_scan_never_tear() {
    loom::model(|| {
        let detector = Arc::new(TickGapDetector::new(30));
        let d_writer = Arc::clone(&detector);
        let d_reader = Arc::clone(&detector);
        let now = Instant::now();

        let h_writer = loom::thread::spawn(move || {
            d_writer.record_tick(1, ExchangeSegment::IdxI, now);
        });
        let h_reader = loom::thread::spawn(move || {
            // Reader may see 0 or 1 entries depending on interleaving;
            // either way the call must terminate without UB.
            let gaps = d_reader.scan_gaps(now);
            // Returned vec is well-formed — every tuple has 3 elements,
            // gap_secs is a u64.
            for (sid, _seg, gap) in &gaps {
                let _ = sid;
                let _ = gap;
            }
        });

        h_writer.join().unwrap();
        h_reader.join().unwrap();
    });
}
