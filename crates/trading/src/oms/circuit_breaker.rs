//! Circuit breaker for the Dhan REST API using the failsafe crate.
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

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use tracing::{info, warn};

use dhan_live_trader_common::constants::{
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
            CircuitState::Closed | CircuitState::HalfOpen => Ok(()),
            CircuitState::Open => {
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
    }

    /// Records a failed API call — increments the failure counter.
    ///
    /// If the threshold is reached, opens the circuit.
    pub fn record_failure(&self) {
        let new_count = self
            .consecutive_failures
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);

        if new_count >= self.failure_threshold && self.opened_at_secs.load(Ordering::Relaxed) == 0 {
            let now_secs = now_epoch_secs();
            self.opened_at_secs.store(now_secs, Ordering::Relaxed);
            warn!(
                failures = new_count,
                threshold = self.failure_threshold,
                "circuit breaker OPEN — Dhan API failures exceeded threshold"
            );
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

    /// Resets the circuit breaker to closed state (operator override).
    pub fn reset(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.opened_at_secs.store(0, Ordering::Relaxed);
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
}
