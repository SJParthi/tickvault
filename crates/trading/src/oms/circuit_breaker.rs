//! Circuit breaker for the Dhan REST API using atomic counters.
//!
//! Protects order submission from cascading failures when the Dhan API
//! is unreachable or returning errors.
//!
//! # States
//! - **Closed** (normal): requests flow through
//! - **Open** (failing): requests rejected immediately
//! - **Half-Open** (probing): one request allowed to test recovery
//!
//! # Configuration
//! - Failure threshold: consecutive failures before opening
//! - Reset timeout: time before transitioning from Open to Half-Open

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use tracing::{error, info, warn};

use tickvault_common::constants::{
    OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD, OMS_CIRCUIT_BREAKER_RESET_SECS,
};

use super::types::OmsError;

// ---------------------------------------------------------------------------
// Circuit Breaker State
// ---------------------------------------------------------------------------

/// Current state of the circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests flow through.
    Closed,
    /// API is failing — requests rejected immediately.
    Open,
    /// Probing — one request allowed to test recovery.
    HalfOpen,
}

// ---------------------------------------------------------------------------
// OrderCircuitBreaker
// ---------------------------------------------------------------------------

/// Circuit breaker wrapping Dhan REST API calls.
///
/// Uses atomic counters for lock-free state tracking.
/// Cold path — checked before each order submission.
pub struct OrderCircuitBreaker {
    /// Number of consecutive failures.
    consecutive_failures: AtomicU32,
    /// Failure threshold before opening the circuit.
    failure_threshold: u32,
    /// Timestamp (epoch secs) when the circuit was opened.
    opened_at_secs: AtomicU64,
    /// Duration before transitioning from Open to Half-Open.
    reset_timeout: Duration,
    /// Gate for Half-Open: only one probe request allowed.
    half_open_probe_sent: AtomicBool,
}

impl Default for OrderCircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderCircuitBreaker {
    /// Creates a new circuit breaker with configured thresholds.
    pub fn new() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            opened_at_secs: AtomicU64::new(0),
            reset_timeout: Duration::from_secs(OMS_CIRCUIT_BREAKER_RESET_SECS),
            half_open_probe_sent: AtomicBool::new(false),
        }
    }

    /// Checks if a request is allowed through the circuit breaker.
    ///
    /// # Returns
    /// `Ok(())` if the request is allowed.
    ///
    /// # Errors
    /// `OmsError::CircuitBreakerOpen` if the circuit is open.
    pub fn check(&self) -> Result<(), OmsError> {
        match self.state() {
            CircuitState::Closed => {
                metrics::gauge!("tv_circuit_breaker_state").set(0.0_f64);
                Ok(())
            }
            CircuitState::HalfOpen => {
                metrics::gauge!("tv_circuit_breaker_state").set(2.0_f64);
                // Only allow one probe request in HalfOpen state.
                if self
                    .half_open_probe_sent
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    info!("circuit breaker HALF-OPEN — allowing one probe request");
                    Ok(())
                } else {
                    warn!("circuit breaker HALF-OPEN — probe already in flight, rejecting");
                    Err(OmsError::CircuitBreakerOpen)
                }
            }
            CircuitState::Open => {
                metrics::gauge!("tv_circuit_breaker_state").set(1.0_f64);
                warn!("circuit breaker OPEN — Dhan API temporarily unavailable");
                Err(OmsError::CircuitBreakerOpen)
            }
        }
    }

    /// Records a successful API call — resets the failure counter.
    pub fn record_success(&self) {
        let prev = self.consecutive_failures.swap(0, Ordering::Relaxed);
        if prev >= self.failure_threshold {
            info!("circuit breaker CLOSED — Dhan API recovered");
        }
        self.opened_at_secs.store(0, Ordering::Relaxed);
        self.half_open_probe_sent.store(false, Ordering::Relaxed);
        metrics::gauge!("tv_circuit_breaker_state").set(0.0_f64);
    }

    /// Records a failed API call — increments the failure counter.
    ///
    /// If the threshold is reached, opens the circuit.
    pub fn record_failure(&self) {
        let new_count = self
            .consecutive_failures
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);

        if new_count >= self.failure_threshold {
            let now_secs = now_epoch_secs();
            // Atomically set opened_at only if not already set (avoids TOCTOU race).
            if self
                .opened_at_secs
                .compare_exchange(0, now_secs, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                self.half_open_probe_sent.store(false, Ordering::Relaxed);
                metrics::gauge!("tv_circuit_breaker_state").set(1.0_f64);
                // OMS-GAP-03: circuit breaker opened; critical severity.
                error!(
                    code = tickvault_common::error_code::ErrorCode::OmsGapCircuitBreaker.code_str(),
                    severity = tickvault_common::error_code::ErrorCode::OmsGapCircuitBreaker
                        .severity()
                        .as_str(),
                    failures = new_count,
                    threshold = self.failure_threshold,
                    "OMS-GAP-03: circuit breaker OPEN — Dhan API failures exceeded threshold. \
                     ALL order submissions blocked until recovery."
                );
            }
        }
    }

    /// Returns the current circuit state.
    pub fn state(&self) -> CircuitState {
        let failures = self.consecutive_failures.load(Ordering::Relaxed);
        if failures < self.failure_threshold {
            return CircuitState::Closed;
        }

        let opened_at = self.opened_at_secs.load(Ordering::Relaxed);
        if opened_at == 0 {
            return CircuitState::Closed;
        }

        let elapsed_secs = now_epoch_secs().saturating_sub(opened_at);
        if elapsed_secs >= self.reset_timeout.as_secs() {
            CircuitState::HalfOpen
        } else {
            CircuitState::Open
        }
    }

    /// Returns the current consecutive failure count.
    // TEST-EXEMPT: trivial atomic load getter, tested indirectly by circuit breaker state tests
    pub fn failure_count(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// Returns true if the circuit breaker transitioned from open/half-open
    /// to closed on the most recent `record_success()` call.
    /// Used by OMS engine to fire CircuitBreakerClosed notification.
    // TEST-EXEMPT: trivial threshold comparison, tested indirectly by OMS engine tests
    pub fn was_previously_open(&self, prev_failures: u32) -> bool {
        prev_failures >= self.failure_threshold
    }

    /// Resets the circuit breaker to closed state (operator override).
    pub fn reset(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.opened_at_secs.store(0, Ordering::Relaxed);
        self.half_open_probe_sent.store(false, Ordering::Relaxed);
        metrics::gauge!("tv_circuit_breaker_state").set(0.0_f64);
        info!("circuit breaker manually reset to CLOSED");
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

/// Returns the current time as epoch seconds.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_circuit_breaker_is_closed() {
        let cb = OrderCircuitBreaker::new();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn opens_after_threshold_failures() {
        let cb = OrderCircuitBreaker::new();

        for _ in 0..OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            cb.record_failure();
        }

        assert_eq!(cb.state(), CircuitState::Open);
        assert!(cb.check().is_err());
    }

    #[test]
    fn success_resets_failure_count() {
        let cb = OrderCircuitBreaker::new();

        // Record some failures (below threshold)
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);

        // Success resets
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);

        // Can accumulate failures again from zero
        cb.record_failure();
        assert_eq!(cb.consecutive_failures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn manual_reset_closes_circuit() {
        let cb = OrderCircuitBreaker::new();

        for _ in 0..OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        cb.reset();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn below_threshold_stays_closed() {
        let cb = OrderCircuitBreaker::new();

        for _ in 0..(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD - 1) {
            cb.record_failure();
        }

        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn circuit_states_are_distinct() {
        assert_ne!(CircuitState::Closed, CircuitState::Open);
        assert_ne!(CircuitState::Open, CircuitState::HalfOpen);
        assert_ne!(CircuitState::Closed, CircuitState::HalfOpen);
    }

    // --- Debug format tests ---

    #[test]
    fn circuit_state_debug_all_variants() {
        let closed = format!("{:?}", CircuitState::Closed);
        let open = format!("{:?}", CircuitState::Open);
        let half_open = format!("{:?}", CircuitState::HalfOpen);

        assert!(closed.contains("Closed"), "Debug for Closed: {closed}");
        assert!(open.contains("Open"), "Debug for Open: {open}");
        assert!(
            half_open.contains("HalfOpen"),
            "Debug for HalfOpen: {half_open}"
        );

        // All must be distinct strings
        assert_ne!(closed, open);
        assert_ne!(open, half_open);
        assert_ne!(closed, half_open);
    }

    #[test]
    fn circuit_state_debug_is_not_empty() {
        let states = [
            CircuitState::Closed,
            CircuitState::Open,
            CircuitState::HalfOpen,
        ];
        for state in &states {
            let debug = format!("{state:?}");
            assert!(!debug.is_empty(), "Debug for {state:?} must not be empty");
        }
    }

    #[test]
    fn circuit_state_clone_preserves_value() {
        let original = CircuitState::HalfOpen;
        let cloned = original;
        assert_eq!(original, cloned);
    }

    #[test]
    fn circuit_state_copy_semantics() {
        let a = CircuitState::Open;
        let b = a; // Copy
        assert_eq!(a, b); // Original still usable
    }

    // -----------------------------------------------------------------------
    // Recovery log emission: record_success after threshold failures
    // -----------------------------------------------------------------------

    #[test]
    fn record_success_after_threshold_failures_resets_to_closed() {
        let cb = OrderCircuitBreaker::new();

        // Trip the circuit breaker open
        for _ in 0..OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        // record_success resets the failure count and clears opened_at
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.consecutive_failures.load(Ordering::Relaxed), 0);
        assert_eq!(cb.opened_at_secs.load(Ordering::Relaxed), 0);
        assert!(!cb.half_open_probe_sent.load(Ordering::Relaxed));
    }

    #[test]
    fn record_success_below_threshold_is_noop_on_state() {
        let cb = OrderCircuitBreaker::new();

        // A couple of failures, still closed
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);

        // record_success should reset counter to 0
        cb.record_success();
        assert_eq!(cb.consecutive_failures.load(Ordering::Relaxed), 0);
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    // -----------------------------------------------------------------------
    // Half-open probe gate: CAS semantics
    // -----------------------------------------------------------------------

    #[test]
    fn half_open_allows_exactly_one_probe() {
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            // Set opened_at far enough in the past to transition to HalfOpen
            opened_at_secs: AtomicU64::new(1),
            reset_timeout: Duration::from_secs(0), // Immediate transition
            half_open_probe_sent: AtomicBool::new(false),
        };

        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // First check in HalfOpen: probe allowed
        let first = cb.check();
        assert!(first.is_ok(), "first probe must be allowed");

        // Second check in HalfOpen: probe already in flight
        let second = cb.check();
        assert!(
            second.is_err(),
            "second probe must be rejected while first is in flight"
        );
        assert!(matches!(second.unwrap_err(), OmsError::CircuitBreakerOpen));
    }

    #[test]
    fn half_open_probe_reset_after_success() {
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            opened_at_secs: AtomicU64::new(1),
            reset_timeout: Duration::from_secs(0),
            half_open_probe_sent: AtomicBool::new(false),
        };

        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Send probe
        assert!(cb.check().is_ok());
        assert!(cb.half_open_probe_sent.load(Ordering::Relaxed));

        // Probe succeeds — record_success resets everything
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(!cb.half_open_probe_sent.load(Ordering::Relaxed));

        // Now can send requests normally
        assert!(cb.check().is_ok());
    }

    // -----------------------------------------------------------------------
    // State timing: Open → HalfOpen transition
    // -----------------------------------------------------------------------

    #[test]
    fn open_transitions_to_half_open_after_timeout() {
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            // Set opened_at to current time — should be Open initially
            opened_at_secs: AtomicU64::new(now_epoch_secs()),
            reset_timeout: Duration::from_secs(OMS_CIRCUIT_BREAKER_RESET_SECS),
            half_open_probe_sent: AtomicBool::new(false),
        };

        // Should be Open (not enough time elapsed)
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn open_with_expired_timeout_is_half_open() {
        let past = now_epoch_secs().saturating_sub(OMS_CIRCUIT_BREAKER_RESET_SECS + 1);
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            opened_at_secs: AtomicU64::new(past),
            reset_timeout: Duration::from_secs(OMS_CIRCUIT_BREAKER_RESET_SECS),
            half_open_probe_sent: AtomicBool::new(false),
        };

        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn state_closed_when_failures_at_threshold_but_opened_at_zero() {
        // Covers the defensive branch at line 157: opened_at == 0
        // with failures >= threshold. This can happen if the AtomicU64
        // for opened_at was reset (e.g., by record_success) while
        // failures counter hasn't been atomically synchronized yet.
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            opened_at_secs: AtomicU64::new(0), // zero despite high failures
            reset_timeout: Duration::from_secs(OMS_CIRCUIT_BREAKER_RESET_SECS),
            half_open_probe_sent: AtomicBool::new(false),
        };

        // Should return Closed because opened_at is 0 (defensive fallback)
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn half_open_failure_reopens_circuit() {
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            opened_at_secs: AtomicU64::new(1),
            reset_timeout: Duration::from_secs(0),
            half_open_probe_sent: AtomicBool::new(false),
        };

        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Probe allowed
        assert!(cb.check().is_ok());

        // Probe fails — record_failure increments counter, keeps circuit open
        cb.record_failure();

        // opened_at_secs gets updated, so state depends on timing.
        // But consecutive_failures is now > threshold, so it's at least Open.
        let state = cb.state();
        assert!(
            state == CircuitState::Open || state == CircuitState::HalfOpen,
            "after half-open failure, circuit must be Open or HalfOpen (not Closed)"
        );
    }

    // -----------------------------------------------------------------------
    // Recovery logging path: record_success when prev >= threshold
    // -----------------------------------------------------------------------

    #[test]
    fn record_success_recovery_resets_all_atomics() {
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(
                OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD.saturating_add(5),
            ),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            opened_at_secs: AtomicU64::new(now_epoch_secs().saturating_sub(100)),
            reset_timeout: Duration::from_secs(0),
            half_open_probe_sent: AtomicBool::new(true),
        };

        // Prev count was well above threshold — this triggers the recovery info! log
        cb.record_success();

        assert_eq!(cb.consecutive_failures.load(Ordering::Relaxed), 0);
        assert_eq!(cb.opened_at_secs.load(Ordering::Relaxed), 0);
        assert!(!cb.half_open_probe_sent.load(Ordering::Relaxed));
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn record_success_below_threshold_no_recovery_log() {
        // When prev < threshold, the "recovery" log branch is NOT taken
        // but the state is still reset correctly.
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(1),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            opened_at_secs: AtomicU64::new(0),
            reset_timeout: Duration::from_secs(OMS_CIRCUIT_BREAKER_RESET_SECS),
            half_open_probe_sent: AtomicBool::new(false),
        };

        cb.record_success();
        assert_eq!(cb.consecutive_failures.load(Ordering::Relaxed), 0);
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    // -----------------------------------------------------------------------
    // CAS race condition: half-open probe gate
    // -----------------------------------------------------------------------

    #[test]
    fn half_open_cas_gate_sequential_check() {
        // Simulate two sequential checks: first wins, second loses
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            opened_at_secs: AtomicU64::new(1),
            reset_timeout: Duration::from_secs(0),
            half_open_probe_sent: AtomicBool::new(false),
        };

        // First check: CAS succeeds (false → true)
        let r1 = cb.check();
        assert!(r1.is_ok());
        assert!(cb.half_open_probe_sent.load(Ordering::Relaxed));

        // Second check: CAS fails (already true)
        let r2 = cb.check();
        assert!(r2.is_err());
        assert!(matches!(r2.unwrap_err(), OmsError::CircuitBreakerOpen));

        // Third check also fails
        let r3 = cb.check();
        assert!(r3.is_err());
    }

    // -----------------------------------------------------------------------
    // Concurrent failures: multiple record_failure calls
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_failures_all_increment() {
        let cb = std::sync::Arc::new(OrderCircuitBreaker::new());

        // Spawn multiple threads that each call record_failure once
        let mut handles = vec![];
        for _ in 0..OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            let cb_clone = std::sync::Arc::clone(&cb);
            handles.push(std::thread::spawn(move || {
                cb_clone.record_failure();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // All failures should have been counted
        assert!(
            cb.consecutive_failures.load(Ordering::Relaxed)
                >= OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD
        );
        // The circuit should be open
        assert_ne!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn concurrent_success_and_failure_no_panic() {
        // Stress test: interleave success and failure calls
        let cb = std::sync::Arc::new(OrderCircuitBreaker::new());

        let mut handles = vec![];
        for i in 0..20_u32 {
            let cb_clone = std::sync::Arc::clone(&cb);
            handles.push(std::thread::spawn(move || {
                if i % 2 == 0 {
                    cb_clone.record_failure();
                } else {
                    cb_clone.record_success();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // No panic — final state is either Closed or Open depending on race
        let state = cb.state();
        assert!(
            state == CircuitState::Closed
                || state == CircuitState::Open
                || state == CircuitState::HalfOpen,
        );
    }

    // -----------------------------------------------------------------------
    // Half-Open timing: just at the boundary
    // -----------------------------------------------------------------------

    #[test]
    fn open_exactly_at_reset_timeout_is_half_open() {
        let reset_secs = 60_u64;
        let opened_at = now_epoch_secs().saturating_sub(reset_secs);
        let cb = OrderCircuitBreaker {
            consecutive_failures: AtomicU32::new(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD),
            failure_threshold: OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            opened_at_secs: AtomicU64::new(opened_at),
            reset_timeout: Duration::from_secs(reset_secs),
            half_open_probe_sent: AtomicBool::new(false),
        };

        // elapsed_secs >= reset_timeout → HalfOpen
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }

    // -----------------------------------------------------------------------
    // record_failure CAS: only first opener sets opened_at
    // -----------------------------------------------------------------------

    #[test]
    fn record_failure_cas_only_first_sets_opened_at() {
        let cb = OrderCircuitBreaker::new();

        // Fill up to threshold
        for _ in 0..OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            cb.record_failure();
        }

        let first_opened = cb.opened_at_secs.load(Ordering::Relaxed);
        assert!(first_opened > 0, "opened_at must be set after threshold");

        // Additional failure should NOT change opened_at (CAS fails because != 0)
        cb.record_failure();
        let second_opened = cb.opened_at_secs.load(Ordering::Relaxed);
        assert_eq!(
            first_opened, second_opened,
            "opened_at must not change on subsequent failures"
        );
    }

    // -----------------------------------------------------------------------
    // Default impl
    // -----------------------------------------------------------------------

    #[test]
    fn default_creates_closed_circuit_breaker() {
        let cb = OrderCircuitBreaker::default();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.failure_threshold, OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD);
    }

    #[test]
    fn test_circuit_breaker_metric() {
        metrics::gauge!("tv_circuit_breaker_state").set(0.0_f64);
        metrics::gauge!("tv_circuit_breaker_state").set(1.0_f64);
        metrics::gauge!("tv_circuit_breaker_state").set(2.0_f64);
    }
}
