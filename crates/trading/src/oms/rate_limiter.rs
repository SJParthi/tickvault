//! SEBI-mandated order rate limiter using the governor crate (GCRA algorithm).
//!
//! Enforces the maximum orders-per-second limit mandated by SEBI regulation.
//! Uses Generic Cell Rate Algorithm for smooth rate limiting.
//!
//! # SEBI Rule
//! Maximum 10 orders per second. Violation = regulatory risk.
//! If rate limit hit: reject immediately, do NOT retry.

use std::num::NonZeroU32;

use governor::{Quota, RateLimiter, clock::DefaultClock, state::InMemoryState, state::NotKeyed};
use tracing::warn;

use super::types::OmsError;

// ---------------------------------------------------------------------------
// OrderRateLimiter
// ---------------------------------------------------------------------------

/// GCRA-based rate limiter for order submission.
///
/// Wraps `governor::RateLimiter` with SEBI-compliant configuration.
/// Cold path — checked before each order submission (~1-100/day).
pub struct OrderRateLimiter {
    limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
}

impl OrderRateLimiter {
    /// Creates a new rate limiter.
    ///
    /// # Arguments
    /// * `max_orders_per_second` — Maximum orders per second (from config, SEBI limit = 10).
    ///
    /// # Panics
    /// Panics if `max_orders_per_second` is 0 (compile-time guaranteed by config validation).
    pub fn new(max_orders_per_second: u32) -> Self {
        #[allow(clippy::expect_used)] // APPROVED: config validation ensures > 0
        let max_burst = NonZeroU32::new(max_orders_per_second)
            .expect("max_orders_per_second must be > 0 (validated at config load)");

        let quota = Quota::per_second(max_burst);

        Self {
            limiter: RateLimiter::direct(quota),
        }
    }

    /// Checks whether an order can be submitted without exceeding the rate limit.
    ///
    /// # Returns
    /// `Ok(())` if the order is allowed.
    ///
    /// # Errors
    /// `OmsError::RateLimited` if the SEBI rate limit would be exceeded.
    /// Caller must NOT retry — SEBI violation risk.
    pub fn check(&self) -> Result<(), OmsError> {
        match self.limiter.check() {
            Ok(()) => Ok(()),
            Err(_) => {
                warn!("order rate limit hit — SEBI max orders/sec exceeded, rejecting");
                Err(OmsError::RateLimited)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_within_burst() {
        let limiter = OrderRateLimiter::new(10);
        // First order should always pass
        assert!(limiter.check().is_ok());
    }

    #[test]
    fn rate_limiter_exhausts_burst() {
        let limiter = OrderRateLimiter::new(3);
        // First 3 should pass (burst capacity)
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        // 4th should be rate limited
        let result = limiter.check();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), OmsError::RateLimited));
    }

    #[test]
    fn rate_limiter_with_sebi_limit() {
        let limiter = OrderRateLimiter::new(10);
        // Should allow at least 10 orders in burst
        for _ in 0..10 {
            assert!(limiter.check().is_ok());
        }
        // 11th should be rate limited
        assert!(limiter.check().is_err());
    }

    #[test]
    #[should_panic(expected = "max_orders_per_second must be > 0")]
    fn rate_limiter_zero_panics() {
        let _ = OrderRateLimiter::new(0);
    }
}
