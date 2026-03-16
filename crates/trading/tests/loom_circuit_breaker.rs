//! Loom concurrency tests for the OrderCircuitBreaker.
//!
//! The circuit breaker uses atomic operations (AtomicU32, AtomicU64, AtomicBool)
//! for lock-free state management. These tests verify correctness under all
//! possible thread interleavings using the Loom model checker.
//!
//! Tests 1-2 use the ACTUAL OrderCircuitBreaker struct (loom-compatible via
//! `#[cfg(loom)]` conditional imports in circuit_breaker.rs).
//! Test 3 tests the CAS gate pattern directly (since reaching HalfOpen state
//! requires elapsed time > reset_timeout, which loom doesn't control).
//!
//! Run with: RUSTFLAGS="--cfg loom" cargo test -p dhan-live-trader-trading --test loom_circuit_breaker

#![allow(unexpected_cfgs)] // APPROVED: loom cfg is set via RUSTFLAGS, not Cargo features

#[cfg(loom)]
mod loom_tests {
    use loom::sync::Arc;
    use loom::thread;

    use dhan_live_trader_trading::oms::circuit_breaker::{CircuitState, OrderCircuitBreaker};

    /// Verifies that concurrent record_failure + record_success on the ACTUAL
    /// OrderCircuitBreaker never produces an inconsistent state.
    ///
    /// Thread 1: calls record_failure 3 times (enough to open the circuit)
    /// Thread 2: calls record_success (resets failure counter)
    ///
    /// Loom exhaustively checks all interleavings. Final state must always be
    /// either Closed (success reset after all failures) or Open (failures
    /// completed after success).
    #[test]
    fn loom_concurrent_failure_and_success_real_struct() {
        loom::model(|| {
            let cb = Arc::new(OrderCircuitBreaker::new());

            let cb1 = Arc::clone(&cb);
            let cb2 = Arc::clone(&cb);

            let h1 = thread::spawn(move || {
                cb1.record_failure();
                cb1.record_failure();
                cb1.record_failure();
            });

            let h2 = thread::spawn(move || {
                cb2.record_success();
            });

            h1.join().unwrap();
            h2.join().unwrap();

            // State must be valid: Closed or Open (never garbage)
            let state = cb.state();
            assert!(
                state == CircuitState::Closed || state == CircuitState::Open,
                "unexpected state: {state:?}"
            );
        });
    }

    /// Verifies that concurrent record_failure from two threads opens the
    /// circuit correctly — the failure counter must reach or exceed the
    /// threshold regardless of interleaving.
    #[test]
    fn loom_concurrent_failures_open_circuit() {
        loom::model(|| {
            let cb = Arc::new(OrderCircuitBreaker::new());

            let cb1 = Arc::clone(&cb);
            let cb2 = Arc::clone(&cb);

            // Each thread adds 2 failures → total 4, threshold is 3 → should open
            let h1 = thread::spawn(move || {
                cb1.record_failure();
                cb1.record_failure();
            });

            let h2 = thread::spawn(move || {
                cb2.record_failure();
                cb2.record_failure();
            });

            h1.join().unwrap();
            h2.join().unwrap();

            // With 4 total failures and threshold 3, circuit must be Open
            let state = cb.state();
            assert_eq!(
                state,
                CircuitState::Open,
                "4 failures must open circuit (threshold=3)"
            );
        });
    }

    /// Verifies that the half-open probe gate (compare_exchange) correctly
    /// allows exactly one thread through.
    ///
    /// This tests the CAS pattern directly because reaching HalfOpen state
    /// requires elapsed time > reset_timeout, which loom cannot control.
    /// The pattern verified here is identical to OrderCircuitBreaker::check()
    /// in HalfOpen state.
    #[test]
    fn loom_half_open_probe_gate_exactly_one() {
        loom::model(|| {
            let probe_sent = Arc::new(loom::sync::atomic::AtomicBool::new(false));
            let allowed = Arc::new(loom::sync::atomic::AtomicU32::new(0));

            let p1 = Arc::clone(&probe_sent);
            let a1 = Arc::clone(&allowed);
            let p2 = Arc::clone(&probe_sent);
            let a2 = Arc::clone(&allowed);

            let h1 = thread::spawn(move || {
                if p1
                    .compare_exchange(
                        false,
                        true,
                        loom::sync::atomic::Ordering::Relaxed,
                        loom::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    a1.fetch_add(1, loom::sync::atomic::Ordering::Relaxed);
                }
            });

            let h2 = thread::spawn(move || {
                if p2
                    .compare_exchange(
                        false,
                        true,
                        loom::sync::atomic::Ordering::Relaxed,
                        loom::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    a2.fetch_add(1, loom::sync::atomic::Ordering::Relaxed);
                }
            });

            h1.join().unwrap();
            h2.join().unwrap();

            let total_allowed = allowed.load(loom::sync::atomic::Ordering::Relaxed);
            assert_eq!(
                total_allowed, 1,
                "exactly one thread must pass through half-open gate"
            );
        });
    }
}

// Standard (non-loom) concurrency tests that run in normal CI
#[cfg(not(loom))]
mod std_concurrency_tests {
    use std::sync::Arc;
    use std::thread;

    use dhan_live_trader_trading::oms::circuit_breaker::{CircuitState, OrderCircuitBreaker};

    /// Stress test: concurrent failure recording opens the circuit.
    #[test]
    fn concurrent_failures_open_circuit() {
        let cb = Arc::new(OrderCircuitBreaker::new());
        let mut handles = vec![];

        for _ in 0..8 {
            let cb = Arc::clone(&cb);
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    cb.record_failure();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // After 8000 failures, circuit must be open
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(cb.check().is_err());
    }

    /// Stress test: mixed success/failure — final state is always valid.
    #[test]
    fn concurrent_mixed_operations_valid_state() {
        let cb = Arc::new(OrderCircuitBreaker::new());
        let mut handles = vec![];

        for i in 0..8 {
            let cb = Arc::clone(&cb);
            handles.push(thread::spawn(move || {
                for _ in 0..500 {
                    if i % 2 == 0 {
                        cb.record_failure();
                    } else {
                        cb.record_success();
                    }
                    let _ = cb.check();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Final state must be one of the valid states (never garbage)
        let state = cb.state();
        assert!(
            state == CircuitState::Closed
                || state == CircuitState::Open
                || state == CircuitState::HalfOpen,
            "invalid state: {state:?}"
        );
    }

    /// Stress test: concurrent check + reset — no panic, state always valid.
    #[test]
    fn concurrent_check_and_reset_no_panic() {
        let cb = Arc::new(OrderCircuitBreaker::new());

        // Open the circuit first
        for _ in 0..10 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        let mut handles = vec![];

        for i in 0..4 {
            let cb = Arc::clone(&cb);
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    if i == 0 {
                        cb.reset();
                    } else {
                        let _ = cb.check();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // After concurrent reset + check, state must be valid
        let state = cb.state();
        assert!(
            state == CircuitState::Closed
                || state == CircuitState::Open
                || state == CircuitState::HalfOpen,
        );
    }

    /// Concurrent record_success resets failure counter to zero.
    #[test]
    fn concurrent_success_resets_counter() {
        let cb = Arc::new(OrderCircuitBreaker::new());

        // Add 2 failures (below threshold of 3)
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);

        let cb1 = Arc::clone(&cb);
        let cb2 = Arc::clone(&cb);

        // Thread 1: record_success (resets counter)
        // Thread 2: record_failure (adds 1)
        let h1 = thread::spawn(move || {
            cb1.record_success();
        });
        let h2 = thread::spawn(move || {
            cb2.record_failure();
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // After success + 1 failure, circuit should still be Closed
        // (success resets to 0, then 1 failure < threshold 3)
        // OR if failure happened before success, counter was reset to 0
        assert_eq!(cb.state(), CircuitState::Closed);
    }
}
