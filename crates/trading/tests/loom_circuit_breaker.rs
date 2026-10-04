//! Loom concurrency tests for the OrderCircuitBreaker.
//!
//! The breaker keeps its state in three atomics. Since audit H3
//! (2026-10-04) it takes them from `tickvault_trading`'s `sync` shim, so with
//! the crate's `loom` feature they ARE loom atomics and every test below
//! drives the REAL production struct through every interleaving loom can
//! produce, orderings included. (Before, the struct used std atomics, which
//! loom cannot see, and the probe-gate test modelled a hand-written copy of
//! the compare-exchange instead of the real one.)
//!
//! Run with: cargo test -p tickvault-trading --features loom --test loom_circuit_breaker

#[cfg(feature = "loom")]
mod loom_tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicU32, Ordering};
    use loom::thread;

    use tickvault_trading::oms::circuit_breaker::{CircuitState, OrderCircuitBreaker};

    /// Concurrent record_failure + record_success never leaves a state that
    /// is not Closed or Open (the clock does not move inside a model, so
    /// HalfOpen is unreachable from a fresh breaker).
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

            let state = cb.state();
            assert!(
                state == CircuitState::Closed || state == CircuitState::Open,
                "unexpected state: {state:?}"
            );
        });
    }

    /// Two threads' failures crossing the threshold together open the circuit
    /// in every interleaving.
    #[test]
    fn loom_concurrent_failures_open_circuit() {
        loom::model(|| {
            let cb = Arc::new(OrderCircuitBreaker::new());

            let cb1 = Arc::clone(&cb);
            let cb2 = Arc::clone(&cb);

            // Each thread adds 2 failures, total 4; the threshold is 3.
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

            assert_eq!(cb.failure_count(), 4);
            assert_eq!(
                cb.state(),
                CircuitState::Open,
                "4 failures must open circuit (threshold=3)"
            );
        });
    }

    /// The REAL half-open gate lets exactly one of two racing callers through.
    #[test]
    fn loom_half_open_probe_gate_exactly_one() {
        loom::model(|| {
            let cb = Arc::new(OrderCircuitBreaker::new_half_open_for_model());
            let allowed = Arc::new(AtomicU32::new(0));

            let mut handles = Vec::new();
            for _ in 0..2 {
                let cb = Arc::clone(&cb);
                let allowed = Arc::clone(&allowed);
                handles.push(thread::spawn(move || {
                    if cb.check().is_ok() {
                        allowed.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }

            assert_eq!(
                allowed.load(Ordering::Relaxed),
                1,
                "exactly one caller may pass the half-open gate"
            );
        });
    }

    /// A probe that fails re-opens the circuit: in no interleaving does a
    /// second caller get a probe straight after the first one failed, and the
    /// breaker never ends stuck half-open with its probe spent (the defect
    /// this model found on 2026-10-04: every later request was refused until
    /// a manual reset).
    #[test]
    fn loom_failed_probe_reopens_and_allows_no_second_probe() {
        loom::model(|| {
            let cb = Arc::new(OrderCircuitBreaker::new_half_open_for_model());
            let allowed = Arc::new(AtomicU32::new(0));

            let mut handles = Vec::new();
            for _ in 0..2 {
                let cb = Arc::clone(&cb);
                let allowed = Arc::clone(&allowed);
                handles.push(thread::spawn(move || {
                    if cb.check().is_ok() {
                        allowed.fetch_add(1, Ordering::Relaxed);
                        // The probe fails.
                        cb.record_failure();
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }

            assert_eq!(
                allowed.load(Ordering::Relaxed),
                1,
                "a failed probe must not be followed at once by another"
            );
            assert_eq!(
                cb.state(),
                CircuitState::Open,
                "a failed probe re-opens the circuit for another reset timeout"
            );
        });
    }
}
// Standard (non-loom) concurrency tests that run in normal CI
#[cfg(not(feature = "loom"))]
mod std_concurrency_tests {
    use std::sync::Arc;
    use std::thread;

    use tickvault_trading::oms::circuit_breaker::{CircuitState, OrderCircuitBreaker};

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
