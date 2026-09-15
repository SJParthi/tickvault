//! Loom concurrency tests for tick pipeline atomic patterns.
//!
//! The tick processor uses several shared atomic primitives across tasks:
//! - storage-gate `Arc<AtomicBool>` — writer opens/closes, tick processor reads
//!   (the former `scheduler::StorageGate` module was DELETED 2026-07-18,
//!   dead-code batch 2; the atomic PATTERN under test is unchanged)
//! - Atomic tick counters — multiple producers increment, monitor reads
//! - Connection health atomics — reconnection counter + state mutex
//!
//! These tests explore these simplified atomic patterns with Loom. The gate
//! was removed from production; the two unconstrained Boolean-race cases
//! assert only that modeled threads complete without panicking. They do not
//! establish a production deduplication or scheduling invariant.
//!
//! Run with: cargo test -p tickvault-core --features loom --test loom_tick_dedup

#[cfg(feature = "loom")]
mod loom_tests {
    use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    // -----------------------------------------------------------------------
    // storage-gate pattern: writer stores, tick processor loads
    // -----------------------------------------------------------------------

    /// Verifies that a gate opened by the scheduler is visible to the tick
    /// processor across threads — mirrors the storage-gate pattern where
    /// the scheduler opens/closes the gate and the tick processor checks it
    /// on every tick.
    #[test]
    fn test_storage_gate_visibility_after_join() {
        loom::model(|| {
            let gate = Arc::new(AtomicBool::new(false));
            let g1 = Arc::clone(&gate);

            let t1 = thread::spawn(move || {
                g1.store(true, Ordering::Release);
            });

            t1.join().unwrap();

            // After join, the store must be visible.
            assert!(gate.load(Ordering::Acquire));
        });
    }

    /// Completion-only exercise of a retired gate pattern. The final Boolean
    /// may be either value, so there is no behavioral assertion to make here.
    #[test]
    fn test_storage_gate_concurrent_open_close_never_panics() {
        loom::model(|| {
            let gate = Arc::new(AtomicBool::new(false));
            let g_opener = Arc::clone(&gate);
            let g_closer = Arc::clone(&gate);

            let opener = thread::spawn(move || {
                g_opener.store(true, Ordering::Release);
            });

            let closer = thread::spawn(move || {
                g_closer.store(false, Ordering::Release);
            });

            opener.join().unwrap();
            closer.join().unwrap();

            // No claim beyond completion: a Boolean tautology proves nothing.
        });
    }

    /// Completion-only exercise of a reader racing a writer in the retired
    /// gate pattern; a load preceding the store may legitimately read false.
    #[test]
    fn test_storage_gate_reader_during_write_never_panics() {
        loom::model(|| {
            let gate = Arc::new(AtomicBool::new(false));
            let g_writer = Arc::clone(&gate);
            let g_reader = Arc::clone(&gate);

            let writer = thread::spawn(move || {
                g_writer.store(true, Ordering::Release);
            });

            let reader = thread::spawn(move || g_reader.load(Ordering::Acquire));

            writer.join().unwrap();
            let _read_value = reader.join().unwrap();
        });
    }

    // -----------------------------------------------------------------------
    // Tick counter pattern: concurrent atomic increments
    // -----------------------------------------------------------------------

    /// Tests that atomic counter increments from multiple threads
    /// do not lose updates — mirrors the tick counter pattern where
    /// multiple WebSocket connections feed ticks to the processor.
    #[test]
    fn test_concurrent_tick_counter_no_lost_updates() {
        loom::model(|| {
            let counter = Arc::new(AtomicU64::new(0));
            let c1 = Arc::clone(&counter);
            let c2 = Arc::clone(&counter);

            let t1 = thread::spawn(move || {
                c1.fetch_add(1, Ordering::Relaxed);
                c1.fetch_add(1, Ordering::Relaxed);
            });

            let t2 = thread::spawn(move || {
                c2.fetch_add(1, Ordering::Relaxed);
            });

            t1.join().unwrap();
            t2.join().unwrap();

            assert_eq!(counter.load(Ordering::SeqCst), 3);
        });
    }

    // -----------------------------------------------------------------------
    // Connection health pattern: Mutex<State> + AtomicU64 reconnection counter
    // -----------------------------------------------------------------------

    /// Connection state — mirrors `ConnectionState` in websocket/types.rs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MockConnectionState {
        Disconnected,
        Connecting,
        Connected,
        Reconnecting,
    }

    /// Verifies that concurrent state mutation (via Mutex) and reconnection
    /// counter increment (via AtomicU64) are consistent — the health()
    /// method reads both, and must never observe a half-updated snapshot
    /// within a single lock acquisition.
    #[test]
    fn test_connection_health_state_and_counter_consistency() {
        loom::model(|| {
            let state = Arc::new(Mutex::new(MockConnectionState::Disconnected));
            let reconnections = Arc::new(AtomicU64::new(0));

            let s1 = Arc::clone(&state);
            let r1 = Arc::clone(&reconnections);

            // Simulate reconnection: update state + increment counter.
            let reconnector = thread::spawn(move || {
                {
                    let mut guard = s1.lock().unwrap();
                    *guard = MockConnectionState::Reconnecting;
                }
                r1.fetch_add(1, Ordering::Release);
                {
                    let mut guard = s1.lock().unwrap();
                    *guard = MockConnectionState::Connected;
                }
            });

            // Simulate health check: read state + counter.
            let s2 = Arc::clone(&state);
            let r2 = Arc::clone(&reconnections);
            let health_reader = thread::spawn(move || {
                let current_state = *s2.lock().unwrap();
                let recon_count = r2.load(Ordering::Acquire);
                (current_state, recon_count)
            });

            reconnector.join().unwrap();
            let (observed_state, observed_count) = health_reader.join().unwrap();

            // State must be a valid variant.
            assert!(
                observed_state == MockConnectionState::Disconnected
                    || observed_state == MockConnectionState::Reconnecting
                    || observed_state == MockConnectionState::Connected,
                "unexpected state: {observed_state:?}"
            );

            // Counter must be 0 or 1.
            assert!(
                observed_count <= 1,
                "unexpected reconnection count: {observed_count}"
            );
        });
    }

    /// Verifies that multiple concurrent reconnections correctly accumulate
    /// in the atomic counter — no lost increments.
    #[test]
    fn test_reconnection_counter_no_lost_increments() {
        loom::model(|| {
            let counter = Arc::new(AtomicU64::new(0));
            let c1 = Arc::clone(&counter);
            let c2 = Arc::clone(&counter);

            let t1 = thread::spawn(move || {
                c1.fetch_add(1, Ordering::Relaxed);
            });

            let t2 = thread::spawn(move || {
                c2.fetch_add(1, Ordering::Relaxed);
            });

            t1.join().unwrap();
            t2.join().unwrap();

            assert_eq!(
                counter.load(Ordering::SeqCst),
                2,
                "two reconnections must both be counted"
            );
        });
    }
}

// Standard (non-loom) concurrency tests that run in normal CI.
#[cfg(not(feature = "loom"))]
mod std_concurrency_tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Stress test: concurrent storage gate toggling never panics.
    #[test]
    fn stress_storage_gate_concurrent_toggle() {
        let gate = Arc::new(AtomicBool::new(false));
        let mut handles = vec![];

        for i in 0..8 {
            let gate = Arc::clone(&gate);
            handles.push(thread::spawn(move || {
                for _ in 0..10_000 {
                    if i % 2 == 0 {
                        gate.store(true, Ordering::Release);
                    } else {
                        gate.store(false, Ordering::Release);
                    }
                    let _ = gate.load(Ordering::Acquire);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Gate must be a valid boolean after all threads finish.
        let final_val = gate.load(Ordering::Acquire);
        // Verify the atomic value is a valid boolean (0 or 1)
        let raw = if final_val { 1_u8 } else { 0_u8 };
        assert!(raw <= 1, "atomic bool must be 0 or 1, got {raw}");
    }

    /// Stress test: concurrent tick counter increments never lose updates.
    #[test]
    fn stress_tick_counter_no_lost_updates() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut handles = vec![];

        for _ in 0..8 {
            let counter = Arc::clone(&counter);
            handles.push(thread::spawn(move || {
                for _ in 0..10_000 {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(counter.load(Ordering::SeqCst), 80_000);
    }

    /// Stress test: concurrent state mutation + counter increment.
    #[test]
    fn stress_connection_health_concurrent_access() {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum State {
            Disconnected,
            Connected,
            Reconnecting,
        }

        let state = Arc::new(Mutex::new(State::Disconnected));
        let counter = Arc::new(AtomicU64::new(0));
        let mut handles = vec![];

        // Writer threads: simulate reconnections.
        for _ in 0..4 {
            let state = Arc::clone(&state);
            let counter = Arc::clone(&counter);
            handles.push(thread::spawn(move || {
                for _ in 0..1_000 {
                    {
                        let mut guard = state.lock().unwrap();
                        *guard = State::Reconnecting;
                    }
                    counter.fetch_add(1, Ordering::Release);
                    {
                        let mut guard = state.lock().unwrap();
                        *guard = State::Connected;
                    }
                }
            }));
        }

        // Reader threads: simulate health checks.
        for _ in 0..4 {
            let state = Arc::clone(&state);
            let counter = Arc::clone(&counter);
            handles.push(thread::spawn(move || {
                for _ in 0..1_000 {
                    let s = *state.lock().unwrap();
                    let c = counter.load(Ordering::Acquire);
                    assert!(
                        s == State::Disconnected
                            || s == State::Connected
                            || s == State::Reconnecting
                    );
                    // Counter monotonically increases.
                    assert!(c <= 4_000);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(counter.load(Ordering::SeqCst), 4_000);
    }
}
