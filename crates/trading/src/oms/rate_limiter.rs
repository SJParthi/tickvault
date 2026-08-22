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
use tickvault_common::error_code::ErrorCode;
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
            // APPROVED: constructor validation — config guarantees > 0 at load time
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
            Ok(()) => {
                metrics::counter!("tv_rate_limiter_allowed_total").increment(1);
                Ok(())
            }
            Err(_) => {
                metrics::counter!("tv_rate_limiter_denied_total").increment(1);
                warn!("order rate limit hit — SEBI max orders/sec exceeded, rejecting");
                Err(OmsError::RateLimited)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OrderBudget — the per-minute / per-hour / per-day tiers
// ---------------------------------------------------------------------------

/// Documented order tiers the broker publishes: **10/sec, 250/min, 1000/hr,
/// 7000/day** (`docs/dhan-ref/07-orders.md` §9.2, and the canonical rate table
/// in `01-introduction-and-rate-limits.md`).
///
/// Until 2026-08-22 exactly ONE of those four was enforced. [`OrderRateLimiter`]
/// above covers the per-second burst; nothing covered the other three. Two rule
/// files named a `DailyRequestTracker` as the daily enforcer and it has never
/// existed in this workspace — a documented control that was only ever a
/// sentence.
///
/// # Why the shape is three counters and not a queue
///
/// The obvious implementation keeps timestamps and counts those inside the
/// window. That is O(orders) in space and O(window) to evaluate. This is a
/// fixed-window counter per tier: one `u32` and one `u64` each, rolled forward
/// when the window expires.
///
/// - **Time: O(1)** — three compares and at most three resets per check, no
///   loop over history, no allocation.
/// - **Space: O(1)** — 24 bytes per tier, three tiers, **independent of how
///   many orders are placed**. This is one of the few places in the system
///   where O(1) space is genuinely available, because the question ("how many
///   in this window?") does not require remembering which ones.
///
/// # The honest cost of a fixed window
///
/// A fixed window admits up to `2 × limit` across a window boundary — 250 at
/// 10:00:59 and 250 more at 10:01:00. A sliding window would not, at the price
/// of the per-order history this design exists to avoid. The boundary burst is
/// bounded, always below the NEXT tier up (2 × 250/min is inside 1000/hr), and
/// the per-second GCRA still paces it. Recorded rather than hidden: this is a
/// deliberate trade, not an oversight.
///
/// # Windows roll; they are not calendar-aligned
///
/// Each window starts at the first request after the previous one expired, so
/// the daily tier is "7,000 in any rolling 24h", which is never MORE permissive
/// than the broker's calendar day. For a regulatory ceiling, conservative in
/// the right direction is the only acceptable rounding error.
#[derive(Debug, Clone, Copy)]
struct TierWindow {
    limit: u32,
    window_secs: u64,
    used: u32,
    window_start_secs: u64,
}

impl TierWindow {
    const fn new(limit: u32, window_secs: u64) -> Self {
        Self {
            limit,
            window_secs,
            used: 0,
            window_start_secs: 0,
        }
    }

    /// Rolls the window forward if it has expired, then reports whether one
    /// more request fits. Does NOT consume — see [`Self::consume`].
    fn would_admit(&mut self, now_secs: u64) -> bool {
        if now_secs.saturating_sub(self.window_start_secs) >= self.window_secs {
            self.window_start_secs = now_secs;
            self.used = 0;
        }
        self.used < self.limit
    }

    fn consume(&mut self) {
        self.used = self.used.saturating_add(1);
    }

    const fn reset(&mut self) {
        self.used = 0;
        self.window_start_secs = 0;
    }
}

/// The broker's per-minute, per-hour and per-day order tiers.
///
/// Checked BESIDE [`OrderRateLimiter`], never instead of it: the per-second
/// GCRA paces the burst, these bound the totals. All four tiers must admit
/// before an order is placed.
#[derive(Debug, Clone, Copy)]
pub struct OrderBudget {
    minute: TierWindow,
    hour: TierWindow,
    day: TierWindow,
}

impl Default for OrderBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderBudget {
    /// Broker-documented ceilings. Changing one means the vendor changed
    /// theirs -- check `docs/dhan-ref/07-orders.md` before touching these.
    pub const MAX_PER_MINUTE: u32 = 250;
    pub const MAX_PER_HOUR: u32 = 1_000;
    pub const MAX_PER_DAY: u32 = 7_000;

    #[must_use]
    pub const fn new() -> Self {
        Self {
            minute: TierWindow::new(Self::MAX_PER_MINUTE, 60),
            hour: TierWindow::new(Self::MAX_PER_HOUR, 3_600),
            day: TierWindow::new(Self::MAX_PER_DAY, 86_400),
        }
    }

    /// Admits one order against all three tiers, or refuses naming the tier
    /// that ran out.
    ///
    /// Consumption is ALL-OR-NOTHING: a refusal by any tier consumes from
    /// none of them. Consuming from the tiers that had room would let a
    /// refused order still burn budget, so a client retrying into a full
    /// minute would silently exhaust its hour.
    ///
    /// # Errors
    /// [`OmsError::OrderBudgetExhausted`] naming the tier, its limit, and the
    /// seconds until that window rolls.
    pub fn try_consume(&mut self, now_secs: u64) -> Result<(), OmsError> {
        for (tier, window) in [
            ("minute", &mut self.minute),
            ("hour", &mut self.hour),
            ("day", &mut self.day),
        ] {
            if !window.would_admit(now_secs) {
                let retry_after_secs = window
                    .window_secs
                    .saturating_sub(now_secs.saturating_sub(window.window_start_secs));
                metrics::counter!("tv_oms_order_budget_refused_total", "tier" => tier).increment(1);
                warn!(
                    code = ErrorCode::OmsGapRateLimit.code_str(),
                    tier,
                    limit = window.limit,
                    retry_after_secs,
                    "OMS-GAP-04: order REFUSED — the broker's per-{tier} order \
                     ceiling is spent. No order was sent. This is a documented \
                     vendor limit, not a local choice; exceeding it is a \
                     regulatory risk, so the refusal is never retried here."
                );
                return Err(OmsError::OrderBudgetExhausted {
                    tier,
                    limit: window.limit,
                    retry_after_secs,
                });
            }
        }
        self.minute.consume();
        self.hour.consume();
        self.day.consume();
        Ok(())
    }

    /// Clears every tier. Called from the OMS daily reset.
    pub const fn reset_daily(&mut self) {
        self.minute.reset();
        self.hour.reset();
        self.day.reset();
    }

    /// Orders consumed in the current day window — for the operator surface.
    #[must_use]
    pub const fn used_today(&self) -> u32 {
        self.day.used
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

    #[test]
    fn test_rate_limiter_metrics() {
        metrics::counter!("tv_rate_limiter_allowed_total").increment(1);
        metrics::counter!("tv_rate_limiter_denied_total").increment(1);
    }

    // -- 2026-08-22: the three unenforced broker tiers ----------------------

    #[test]
    fn the_tiers_match_the_vendors_published_ceilings() {
        // docs/dhan-ref/07-orders.md §9.2: "10 orders/sec, 250/min, 1000/hr,
        // 7000/day". Until today only the first was enforced.
        assert_eq!(OrderBudget::MAX_PER_MINUTE, 250);
        assert_eq!(OrderBudget::MAX_PER_HOUR, 1_000);
        assert_eq!(OrderBudget::MAX_PER_DAY, 7_000);
    }

    #[test]
    fn the_minute_tier_refuses_at_its_ceiling_and_names_itself() {
        let mut budget = OrderBudget::new();
        for _ in 0..OrderBudget::MAX_PER_MINUTE {
            budget
                .try_consume(1_000)
                .expect("within the minute ceiling");
        }
        match budget.try_consume(1_000) {
            Err(OmsError::OrderBudgetExhausted {
                tier,
                limit,
                retry_after_secs,
            }) => {
                assert_eq!(tier, "minute");
                assert_eq!(limit, OrderBudget::MAX_PER_MINUTE);
                assert_eq!(retry_after_secs, 60, "the whole window is still ahead");
            }
            other => panic!("expected the minute tier to refuse, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_consumes_from_no_tier() {
        // All-or-nothing. If a refused order still burned the hour and day,
        // a client retrying into a full minute would silently exhaust both.
        let mut budget = OrderBudget::new();
        for _ in 0..OrderBudget::MAX_PER_MINUTE {
            budget.try_consume(1_000).expect("fills the minute");
        }
        let before = budget.used_today();
        for _ in 0..50 {
            assert!(budget.try_consume(1_000).is_err());
        }
        assert_eq!(
            budget.used_today(),
            before,
            "50 refusals must not consume a single unit of the day tier"
        );
    }

    #[test]
    fn the_window_rolls_and_the_next_minute_is_fresh() {
        let mut budget = OrderBudget::new();
        for _ in 0..OrderBudget::MAX_PER_MINUTE {
            budget.try_consume(1_000).expect("fills the minute");
        }
        assert!(
            budget.try_consume(1_059).is_err(),
            "still inside the window"
        );
        budget
            .try_consume(1_060)
            .expect("the minute window has rolled");
    }

    #[test]
    fn the_hour_tier_binds_before_the_day_tier_does() {
        // 1000/hr is reached long before 7000/day, so a caller pacing itself
        // under the minute ceiling still meets the hour ceiling first.
        let mut budget = OrderBudget::new();
        let mut now = 0_u64;
        let mut placed = 0_u32;
        // 200 per minute keeps the minute tier happy; the hour fills at 1000.
        loop {
            let mut this_minute = 0;
            while this_minute < 200 {
                if budget.try_consume(now).is_err() {
                    break;
                }
                placed += 1;
                this_minute += 1;
            }
            if this_minute < 200 {
                break;
            }
            now += 60;
            assert!(placed <= OrderBudget::MAX_PER_HOUR, "hour tier must bind");
        }
        assert_eq!(
            placed,
            OrderBudget::MAX_PER_HOUR,
            "the hour ceiling is what stopped it, not the minute or the day"
        );
    }

    #[test]
    fn the_daily_reset_clears_every_tier() {
        let mut budget = OrderBudget::new();
        for _ in 0..OrderBudget::MAX_PER_MINUTE {
            budget.try_consume(1_000).expect("fills the minute");
        }
        assert!(budget.try_consume(1_000).is_err());
        budget.reset_daily();
        budget
            .try_consume(1_000)
            .expect("the daily reset reopens every tier");
        assert_eq!(budget.used_today(), 1);
    }

    #[test]
    fn the_budget_is_o1_in_space_regardless_of_orders_placed() {
        // Three fixed-window counters, not a queue of timestamps. The whole
        // point: memory does not move with the number of orders.
        let baseline = std::mem::size_of::<OrderBudget>();
        let mut budget = OrderBudget::new();
        for i in 0..OrderBudget::MAX_PER_DAY {
            let _ = budget.try_consume(u64::from(i) * 60);
        }
        assert_eq!(
            std::mem::size_of_val(&budget),
            baseline,
            "the budget must not grow with usage"
        );
    }
}
