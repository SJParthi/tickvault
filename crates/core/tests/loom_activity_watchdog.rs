//! Bounded Loom models for watchdog atomic patterns.
//!
//! These tests check an already-observed counter advance, single-winner CAS,
//! and notification visibility after an explicitly joined writer. They use
//! simplified atomics instead of the production Tokio Notify/read loop, so
//! they do not prove the production shutdown path race-free or cover a
//! notification arriving after a reader's final check. That scheduling
//! boundary remains outside this model.
//!
//! Run with:
//!   cargo test -p tickvault-core --features loom --test loom_activity_watchdog

#[cfg(feature = "loom")]
mod loom_tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use loom::thread;

    /// Minimal shape of the (reader, watchdog) pair. The reader is a
    /// single loop iteration; the watchdog is a single observation.
    struct WatchdogState {
        counter: AtomicU64,
        notified: AtomicBool,
        reader_exits: AtomicBool,
    }

    impl WatchdogState {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                counter: AtomicU64::new(0),
                notified: AtomicBool::new(false),
                reader_exits: AtomicBool::new(false),
            })
        }
    }

    /// **Invariant 1** — reader that bumps ONCE before the watchdog
    /// observation must NOT cause a fire. Equivalent to the healthy-ops
    /// case (live feed actively flowing).
    #[test]
    fn watchdog_does_not_fire_when_reader_advances_counter_first() {
        loom::model(|| {
            let state = WatchdogState::new();
            let last_seen = 0u64;

            // Reader: bump counter once, then check if notified.
            let s_reader = Arc::clone(&state);
            let reader = thread::spawn(move || {
                // Simulate one frame through the read loop.
                s_reader.counter.fetch_add(1, Ordering::Relaxed);
                // Before "await"-ing the next frame, check the notify flag.
                if s_reader.notified.load(Ordering::Acquire) {
                    s_reader.reader_exits.store(true, Ordering::Release);
                }
            });

            // Establish the named premise: the reader finished before this
            // watchdog observation. Concurrent one-shot checks do not imply
            // that an eventual notification was observed by the reader.
            reader
                .join()
                .expect("reader join before watchdog observation");

            // Watchdog: one observation — if counter has advanced, do
            // NOT fire. This is the "fast path" of the production
            // watchdog loop (every 5s).
            let s_watchdog = Arc::clone(&state);
            let watchdog = thread::spawn(move || {
                let current = s_watchdog.counter.load(Ordering::Relaxed);
                if current == last_seen {
                    // Only fires if counter stayed put.
                    s_watchdog.notified.store(true, Ordering::Release);
                }
            });

            watchdog.join().expect("watchdog join");
            assert_eq!(state.counter.load(Ordering::Relaxed), 1);
            assert!(
                !state.notified.load(Ordering::Acquire),
                "an already-observed counter advance must not fire the watchdog"
            );
            assert!(!state.reader_exits.load(Ordering::Acquire));
        });
    }

    /// Two modeled observers of a fixed stale counter produce one CAS winner.
    /// This checks the primitive pattern, not production task multiplicity.
    #[test]
    fn watchdog_fires_at_most_once_across_interleavings() {
        loom::model(|| {
            let state = WatchdogState::new();
            let fire_count = Arc::new(AtomicU64::new(0));

            // Watchdog observation #1: reads counter, if stale, "fires".
            let s1 = Arc::clone(&state);
            let fc1 = Arc::clone(&fire_count);
            let w1 = thread::spawn(move || {
                let c = s1.counter.load(Ordering::Relaxed);
                if c == 0 {
                    // CAS guard — only the first firer wins.
                    if s1
                        .notified
                        .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                    {
                        fc1.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });

            // Watchdog observation #2: same logic, racing against #1.
            // In production this cannot happen (one watchdog per
            // connection), but the CAS guard must hold regardless.
            let s2 = Arc::clone(&state);
            let fc2 = Arc::clone(&fire_count);
            let w2 = thread::spawn(move || {
                let c = s2.counter.load(Ordering::Relaxed);
                if c == 0
                    && s2
                        .notified
                        .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                {
                    fc2.fetch_add(1, Ordering::Relaxed);
                }
            });

            w1.join().expect("w1 join");
            w2.join().expect("w2 join");

            // INVARIANT: at most one CAS fire regardless of interleaving.
            let fires = fire_count.load(Ordering::Relaxed);
            assert_eq!(
                fires, 1,
                "two modeled stale-counter observers must produce exactly one CAS winner"
            );
        });
    }

    /// A reader started after the writer is joined observes its notification.
    /// Joining establishes ordering; this does not claim that an arbitrary
    /// earlier concurrent load must see a future Release store.
    #[test]
    fn reader_observes_watchdog_notify_after_release_store() {
        loom::model(|| {
            let state = WatchdogState::new();

            // Watchdog stores the fire flag first.
            let s_watchdog = Arc::clone(&state);
            let watchdog = thread::spawn(move || {
                s_watchdog.notified.store(true, Ordering::Release);
            });

            watchdog.join().expect("watchdog join before reader starts");

            // The named after-store premise is now established.
            let s_reader = Arc::clone(&state);
            let reader = thread::spawn(move || s_reader.notified.load(Ordering::Acquire));

            let observed = reader.join().expect("reader join");
            assert!(
                observed,
                "reader after the joined notification must observe true"
            );

            // Final state: the watchdog's store is visible if we read
            // it again after the joins.
            let final_flag = state.notified.load(Ordering::Acquire);
            assert!(
                final_flag,
                "after watchdog join, notify flag MUST be observed as true"
            );
        });
    }
}

// Standard (non-loom) sanity tests so the file participates in every
// CI run, not just the weekly loom-feature one.
#[cfg(not(feature = "loom"))]
mod std_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;

    #[test]
    fn stress_watchdog_cas_fires_at_most_once_across_many_racers() {
        let notified = Arc::new(AtomicBool::new(false));
        let fire_count = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..32 {
            let notified = Arc::clone(&notified);
            let fire_count = Arc::clone(&fire_count);
            handles.push(thread::spawn(move || {
                if notified
                    .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    fire_count.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        assert_eq!(
            fire_count.load(Ordering::Relaxed),
            1,
            "exactly one racer must win the CAS fire"
        );
    }
}
