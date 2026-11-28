//! SEBI-mandated order rate limiter: a strict sliding 1-second window beside a
//! GCRA (governor crate).
//!
//! Enforces the maximum orders-per-second limit mandated by SEBI regulation.
//!
//! # SEBI Rule
//! Maximum 10 orders per second. Violation = regulatory risk.
//! If rate limit hit: reject immediately, do NOT retry.
//!
//! # Why a window AND a GCRA (2026-11-28, plan Cluster H1)
//!
//! `Quota::per_second(n)` is a GCRA with burst `n` that refills one cell every
//! `1/n` s. After an idle second it admits `n` at once and then one more every
//! `1/n` s, so up to `2n − 1` orders can fall inside one second (19 for 10).
//! The self-trader rule is "at most 10 orders per second", and how NSE counts a
//! second (rolling or calendar) is not documented, so a [`SlidingWindow`] of the
//! last `n` admission instants now refuses an order when all `n` fall within the
//! last second (closed window). No 1-second window, rolling or calendar, can
//! then admit more than `n`. The GCRA stays as the smoother behind it.
//!
//! The window is checked first and recorded only after the GCRA also admits, so
//! a refusal by either spends nothing in the other. O(1) time and space: a fixed
//! array of [`WINDOW_SLOTS`] instants, no allocation. Above `WINDOW_SLOTS`
//! (tests and benches only; config validation caps production at 10) the window
//! is off and the GCRA alone applies.

use std::num::NonZeroU32;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use governor::{Quota, RateLimiter, clock::DefaultClock, state::InMemoryState, state::NotKeyed};
use tickvault_common::error_code::ErrorCode;
use tracing::warn;

use super::types::OmsError;

// ---------------------------------------------------------------------------
// SlidingWindow — at most N admissions in any closed 1-second window
// ---------------------------------------------------------------------------

/// Largest per-second limit the strict window tracks. Production is capped at
/// `SEBI_MAX_ORDERS_PER_SECOND` (10) by config validation; tests and benches
/// pass larger values (up to `u32::MAX`) to mean "never refuse", and for those
/// the window is off.
pub const WINDOW_SLOTS: usize = 64;

/// Length of the strict window, in seconds: the "per second" of SEBI's
/// orders-per-second limit.
const ORDER_RATE_WINDOW_SECS: u64 = 1;

/// Length of the strict window.
const WINDOW: Duration = Duration::from_secs(ORDER_RATE_WINDOW_SECS);

/// The last `limit` admission instants, in a fixed ring. O(1) per check, no
/// allocation, `WINDOW_SLOTS` instants of space whatever the order count.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SlidingWindow {
    /// Admission instants as nanoseconds since `anchor`; only the first
    /// `limit` slots are used.
    slots: [u64; WINDOW_SLOTS],
    /// How many slots hold an admission (saturates at `limit`).
    filled: usize,
    /// Index of the oldest admission once the ring is full; the next write.
    next: usize,
    /// Per-second limit, `1..=WINDOW_SLOTS`.
    limit: usize,
}

impl SlidingWindow {
    const fn new(limit: usize) -> Self {
        Self {
            slots: [0; WINDOW_SLOTS],
            filled: 0,
            next: 0,
            limit,
        }
    }

    /// Whether one more admission at `now_nanos` keeps every closed 1-second
    /// window at or under `limit`. Refuses when the ring is full and the
    /// oldest of the last `limit` admissions is at most 1 s before `now`
    /// (an equal or earlier `now` counts as inside the window: errs toward
    /// refusal).
    fn would_admit(&self, now_nanos: u64) -> bool {
        if self.filled < self.limit {
            return true;
        }
        let oldest = self.slots[self.next];
        // u128 so a WINDOW of any size cannot overflow the comparison.
        let window_nanos = WINDOW.as_nanos();
        u128::from(now_nanos.saturating_sub(oldest)) > window_nanos
    }

    /// Records an admission at `now_nanos`, overwriting the oldest once full.
    fn record(&mut self, now_nanos: u64) {
        self.slots[self.next] = now_nanos;
        // A compare instead of `% limit`: no division per admission.
        self.next += 1;
        if self.next == self.limit {
            self.next = 0;
        }
        if self.filled < self.limit {
            self.filled += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// OrderRateLimiter
// ---------------------------------------------------------------------------

/// Order-submission rate limiter: a strict sliding 1-second window (when the
/// limit is at most [`WINDOW_SLOTS`]) checked before a GCRA.
///
/// Cold path — checked before each order submission (~1-100/day).
pub struct OrderRateLimiter {
    limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
    /// The strict window, `None` above `WINDOW_SLOTS` (tests and benches).
    /// A `Mutex` because `check` takes `&self`; uncontended in production
    /// (the OMS is single-owner), so one lock and unlock per order. The
    /// poisoned-lock arm in `admit_with` is reachable only in tests: the
    /// release profile sets `panic = "abort"`.
    window: Option<Mutex<SlidingWindow>>,
    /// Monotonic origin for the window's nanosecond instants.
    anchor: Instant,
    /// `tv_rate_limiter_allowed_total`, resolved once on the first admitted
    /// order instead of a registry lookup per order (about 40 ns each with
    /// the production recorder, measured 2026-10-30). Resolved at first USE,
    /// not at construction, so it binds to the recorder installed at boot
    /// even if this limiter was built before it.
    allowed_total: OnceLock<metrics::Counter>,
    /// `tv_rate_limiter_denied_total`, cached the same way. Until 2026-11-28
    /// it was looked up per refusal, which allocated 32 B per refused order
    /// with the production recorder (found by the DHAT window added with the
    /// strict 1-second window; no gate exercised the refusal path before).
    denied_total: OnceLock<metrics::Counter>,
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
        let limit = max_burst.get() as usize;
        let window = (limit <= WINDOW_SLOTS).then(|| Mutex::new(SlidingWindow::new(limit)));

        Self {
            limiter: RateLimiter::direct(quota),
            window,
            anchor: Instant::now(),
            allowed_total: OnceLock::new(),
            denied_total: OnceLock::new(),
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
        let admitted = match &self.window {
            None => self.limiter.check().is_ok(),
            // u64 nanoseconds since construction: about 584 years.
            Some(window) => self.admit_with(window, || {
                u64::try_from(self.anchor.elapsed().as_nanos()).unwrap_or(u64::MAX)
            }),
        };
        self.report(admitted)
    }

    /// The window-then-GCRA decision. `now_nanos` is read after the lock is
    /// taken, so the instants in the ring rise in lock order. The window is
    /// asked first and written only after the GCRA also admits: a refusal by
    /// either spends nothing in the other. Tests pass a fixed clock.
    fn admit_with(&self, window: &Mutex<SlidingWindow>, now_nanos: impl FnOnce() -> u64) -> bool {
        // A poisoned lock means a panic while holding it; the ring is plain
        // data, so keep using it rather than panic here.
        let mut ring = match window.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = now_nanos();
        if ring.would_admit(now) && self.limiter.check().is_ok() {
            ring.record(now);
            true
        } else {
            false
        }
    }

    /// Counts and returns the decision.
    fn report(&self, admitted: bool) -> Result<(), OmsError> {
        if admitted {
            self.allowed_total
                .get_or_init(|| metrics::counter!("tv_rate_limiter_allowed_total"))
                .increment(1);
            Ok(())
        } else {
            self.denied_total
                .get_or_init(|| metrics::counter!("tv_rate_limiter_denied_total"))
                .increment(1);
            warn!("order rate limit hit — SEBI max orders/sec exceeded, rejecting");
            Err(OmsError::RateLimited)
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
/// # The minute and hour windows roll; the day window is the IST date
///
/// A minute or hour window starts at the first request after the previous
/// one expired. The DAY window is the IST calendar date (00:00 to 24:00 IST)
/// since 2026-11-13 (audit B2 follow-up): the broker counts its 7,000 per
/// calendar day, and a rolling 24 h window can admit up to 14,000 inside one
/// broker day (7,000 just before the window rolls at, say, 23:00 and 7,000
/// just after). Earlier text here called the rolling window "never MORE
/// permissive than the broker's calendar day", which was false. Nothing
/// clears the day tier at an arbitrary instant (the 16:00 IST OMS daily reset
/// did, for one build, which let a full day's ceiling be spent twice inside
/// one IST date). The day window only ever moves forward: a wall clock that
/// steps back across midnight keeps the newer date's count.
///
/// # Per IST date WITHIN ONE PROCESS (honest limit, B2 review 2026-11-13)
///
/// The counts live in memory only. `OrderManagementSystem::new` builds a
/// fresh budget with every tier at 0, so a crash restart, a deploy or the
/// evening-drain planned restart starts a new 7,000 (and new minute and hour
/// windows) inside the same IST date. Orders sent by the earlier process are
/// not counted. Not reachable today: `dry_run` is hardcoded on, so nothing
/// reaches the broker. Keeping the day count across restarts (for example,
/// rebuilt at boot from the day's `order_audit` rows) is a follow-up recorded
/// in the audit plan, and must land before live orders are switched on.
#[derive(Debug, Clone, Copy)]
struct TierWindow {
    limit: u32,
    window_secs: u64,
    used: u32,
    window_start_secs: u64,
    /// `true` only for the day tier: the window is the IST calendar date.
    ist_calendar_day: bool,
}

/// Seconds from UTC to IST, as the unsigned offset the day window uses.
const IST_OFFSET_SECS: u64 = tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64.unsigned_abs();

/// The epoch second at which the IST calendar date holding `now_secs`
/// started (00:00 IST). O(1). Saturates at 0 for the first partial IST day
/// after the epoch, which no real clock reads.
const fn ist_day_start_secs(now_secs: u64) -> u64 {
    let ist_day = now_secs.saturating_add(IST_OFFSET_SECS) / 86_400;
    ist_day
        .saturating_mul(86_400)
        .saturating_sub(IST_OFFSET_SECS)
}

impl TierWindow {
    const fn new(limit: u32, window_secs: u64) -> Self {
        Self {
            limit,
            window_secs,
            used: 0,
            window_start_secs: 0,
            ist_calendar_day: false,
        }
    }

    /// The day tier: one window per IST calendar date.
    const fn ist_day(limit: u32) -> Self {
        Self {
            limit,
            window_secs: 86_400,
            used: 0,
            window_start_secs: 0,
            ist_calendar_day: true,
        }
    }

    /// Rolls the window forward if it has expired, then reports whether one
    /// more request fits. Does NOT consume — see [`Self::consume`].
    fn would_admit(&mut self, now_secs: u64) -> bool {
        if self.ist_calendar_day {
            let start = ist_day_start_secs(now_secs);
            if start > self.window_start_secs {
                self.window_start_secs = start;
                self.used = 0;
            }
        } else if now_secs.saturating_sub(self.window_start_secs) >= self.window_secs {
            self.window_start_secs = now_secs;
            self.used = 0;
        }
        self.used < self.limit
    }

    fn consume(&mut self) {
        self.used = self.used.saturating_add(1);
    }

    /// Undoes one [`Self::consume`]. Never rolls the window, so called right
    /// after the consume it takes back (same `&mut` borrow, no other request
    /// in between) it restores the exact count.
    const fn give_back(&mut self) {
        self.used = self.used.saturating_sub(1);
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
            day: TierWindow::ist_day(Self::MAX_PER_DAY),
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
                // One literal label per tier: a non-literal label value sends the
                // macro down its allocating arm (a label `Vec` per call), so a
                // refusal storm would allocate per order. `tier` is one of the
                // three literals in the array above; `_` is the "day" entry.
                match tier {
                    "minute" => {
                        metrics::counter!("tv_oms_order_budget_refused_total", "tier" => "minute")
                            .increment(1);
                    }
                    "hour" => {
                        metrics::counter!("tv_oms_order_budget_refused_total", "tier" => "hour")
                            .increment(1);
                    }
                    _ => {
                        metrics::counter!("tv_oms_order_budget_refused_total", "tier" => "day")
                            .increment(1);
                    }
                }
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

    /// Gives back the one order the last successful [`Self::try_consume`]
    /// counted, on every tier, when a later local gate refused that order and
    /// nothing reached the broker (B2 review, 2026-11-13). Without it a
    /// refused order still spent budget: retries during a halt could spend
    /// the hour tier, and after the halt was cleared every place, modify and
    /// cancel was refused until the hour rolled.
    ///
    /// Call it only for an order this process admitted and did not send, and
    /// only before any other `try_consume` on this budget. O(1), saturating.
    pub const fn give_back(&mut self) {
        self.minute.give_back();
        self.hour.give_back();
        self.day.give_back();
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

    /// 2026-11-13, 15:59:00 IST as epoch seconds (10:29:00 UTC).
    const NOV13_1559_IST: u64 = 1_794_565_740;

    /// Audit B2 follow-up (2026-11-13): the day tier is the IST calendar
    /// date. A day's ceiling spent before 16:00 IST stays spent for the rest
    /// of that IST date, and opens again at 00:00 IST the next date.
    #[test]
    fn the_day_tier_is_the_ist_calendar_date() {
        let mut budget = OrderBudget::new();
        // From 00:30 IST, spread so the minute and hour tiers never bind.
        let mut now = ist_day_start_secs(NOV13_1559_IST) + 1_800;
        for i in 0..OrderBudget::MAX_PER_DAY {
            if i > 0 && i % 200 == 0 {
                now += 60;
            }
            if i > 0 && i % 1_000 == 0 {
                now += 3_600;
            }
            budget.try_consume(now).expect("day room");
        }
        assert!(now < NOV13_1559_IST, "all spent before 15:59 IST");
        // 16:01 IST, same date: refused by the day tier.
        assert!(matches!(
            budget.try_consume(NOV13_1559_IST + 120),
            Err(OmsError::OrderBudgetExhausted { tier: "day", .. })
        ));
        // 23:59:59 IST, same date: still refused.
        let next_midnight = ist_day_start_secs(NOV13_1559_IST) + 86_400;
        assert!(matches!(
            budget.try_consume(next_midnight - 1),
            Err(OmsError::OrderBudgetExhausted { tier: "day", .. })
        ));
        // 00:00 IST the next date: admitted.
        budget
            .try_consume(next_midnight)
            .expect("a new IST date opens a new day window");
        assert_eq!(budget.used_today(), 1);
    }

    /// The IST day starts at 18:30 UTC of the previous UTC date.
    #[test]
    fn ist_day_start_is_ist_midnight() {
        // 2026-11-13 00:00 IST = 2026-11-12 18:30 UTC = 1_794_508_200.
        assert_eq!(ist_day_start_secs(NOV13_1559_IST), 1_794_508_200);
        assert_eq!(ist_day_start_secs(1_794_508_200), 1_794_508_200);
        assert_eq!(ist_day_start_secs(1_794_508_199), 1_794_508_200 - 86_400);
    }

    /// B2 review (2026-11-13): `give_back` undoes exactly one consume on
    /// every tier, so a full minute that gave one back admits one more.
    #[test]
    fn give_back_undoes_one_consume_on_every_tier() {
        let mut budget = OrderBudget::new();
        for _ in 0..OrderBudget::MAX_PER_MINUTE {
            budget.try_consume(1_000).expect("fills the minute");
        }
        assert!(budget.try_consume(1_000).is_err());
        budget.give_back();
        assert_eq!(budget.used_today(), OrderBudget::MAX_PER_MINUTE - 1);
        budget
            .try_consume(1_000)
            .expect("the given-back slot is free");
        assert!(budget.try_consume(1_000).is_err(), "and only that one");
        // Saturating: giving back on an empty budget stays at 0.
        let mut empty = OrderBudget::new();
        empty.give_back();
        assert_eq!(empty.used_today(), 0);
    }

    /// A wall clock stepping back across IST midnight never reopens the
    /// newer date's spent ceiling.
    #[test]
    fn a_backward_clock_step_never_reopens_the_day() {
        let mut budget = OrderBudget::new();
        let midnight = ist_day_start_secs(NOV13_1559_IST);
        budget.try_consume(midnight + 10).expect("first");
        assert_eq!(budget.used_today(), 1);
        budget.try_consume(midnight - 10).expect("second");
        assert_eq!(budget.used_today(), 2, "the newer date keeps its count");
    }

    /// B2 review round 5 (2026-11-13): a clock that steps forward past IST
    /// midnight and back counts on the newer date, the step back neither
    /// resets it nor adds to the old date. Fails under the old rolling
    /// window (the window started at 23:59:00 IST had not run 24 h, so the
    /// count kept climbing across the true midnight: 2, then 3) and under a
    /// calendar roll on `!=` (the step back reset the newer date to 1).
    #[test]
    fn a_backward_step_after_ist_midnight_counts_on_the_newer_date() {
        let mut budget = OrderBudget::new();
        let next_midnight = ist_day_start_secs(NOV13_1559_IST) + 86_400;
        budget
            .try_consume(next_midnight - 60)
            .expect("23:59:00 IST");
        assert_eq!(budget.used_today(), 1);
        budget
            .try_consume(next_midnight + 30)
            .expect("00:00:30 IST the next date");
        assert_eq!(budget.used_today(), 1, "the new IST date starts at 1");
        budget
            .try_consume(next_midnight - 30)
            .expect("stepped back to 23:59:30 IST");
        assert_eq!(budget.used_today(), 2, "counted on the newer date");
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

    /// `Default` must be the SAME budget as `new()`, not a second definition.
    ///
    /// `OrderBudget::default()` is the shape a caller reaches for without
    /// thinking, and it currently delegates to `new()`. A future edit that
    /// spells the tiers out here instead would compile, pass every existing
    /// test (they all construct via `new()`), and hand that caller a
    /// different order budget than the one the vendor documents — a silent
    /// regulatory-limit divergence, not a crash.
    #[test]
    fn order_budget_default_is_the_documented_vendor_budget_not_a_second_definition() {
        let mut from_default = OrderBudget::default();
        let mut from_new = OrderBudget::new();

        // Same ceilings, tier by tier, proven by consuming to each boundary
        // rather than by reading private fields.
        for _ in 0..OrderBudget::MAX_PER_MINUTE {
            assert!(from_default.try_consume(0).is_ok());
            assert!(from_new.try_consume(0).is_ok());
        }
        assert!(
            from_default.try_consume(0).is_err(),
            "the default budget must bind at the documented 250/min"
        );
        assert!(
            from_new.try_consume(0).is_err(),
            "and `new()` must bind at exactly the same place"
        );
    }

    // -- 2026-11-28 (Cluster H1): the strict sliding 1-second window ---------

    const MS: u64 = 1_000_000;
    const SEC: u64 = 1_000 * MS;

    /// Runs `attempts` (nanosecond instants, in call order) through a window
    /// of `limit` and returns the admitted instants.
    fn admitted_by_window(limit: usize, attempts: &[u64]) -> Vec<u64> {
        let mut window = SlidingWindow::new(limit);
        let mut admitted = Vec::new();
        for &now in attempts {
            if window.would_admit(now) {
                window.record(now);
                admitted.push(now);
            }
        }
        admitted
    }

    /// Largest number of `times` inside any closed window `[t, t + 1 s]`.
    fn max_in_any_closed_second(times: &[u64]) -> usize {
        let mut sorted = times.to_vec();
        sorted.sort_unstable();
        let mut best = 0;
        for (i, &start) in sorted.iter().enumerate() {
            let end = start.saturating_add(SEC);
            let count = sorted[i..].iter().take_while(|&&t| t <= end).count();
            best = best.max(count);
        }
        best
    }

    #[test]
    fn test_window_never_admits_more_than_n_in_any_closed_second() {
        // The shape the GCRA let through: a full burst after an idle second,
        // then steady attempts. One attempt every 50 ms for 1.5 s.
        let attempts: Vec<u64> = (0..=30).map(|i| i * 50 * MS).collect();
        let admitted = admitted_by_window(10, &attempts);
        // 0..450 ms admitted (10), then nothing until more than 1 s after
        // 0 ms: 1,050 ms is the next, then 1,100 .. 1,500 as each old one ages out.
        let expected: Vec<u64> = (0..10)
            .map(|i| i * 50 * MS)
            .chain((21..=30).map(|i| i * 50 * MS))
            .collect();
        assert_eq!(admitted, expected);
        assert_eq!(max_in_any_closed_second(&admitted), 10);
    }

    #[test]
    fn test_window_exact_one_second_boundary_is_refused_one_nano_later_admitted() {
        let mut window = SlidingWindow::new(2);
        for now in [0, 10 * MS] {
            assert!(window.would_admit(now));
            window.record(now);
        }
        // Exactly 1 s after the oldest: the closed window [0, 1 s] would hold 3.
        assert!(!window.would_admit(SEC));
        assert!(window.would_admit(SEC + 1));
    }

    #[test]
    fn test_window_backward_or_equal_instant_is_conservative() {
        let mut window = SlidingWindow::new(3);
        for now in [5 * SEC, 5 * SEC + MS, 5 * SEC + 2 * MS] {
            window.record(now);
        }
        assert!(
            !window.would_admit(5 * SEC),
            "an equal instant stays inside the window"
        );
        assert!(
            !window.would_admit(SEC),
            "an earlier instant stays inside the window"
        );
        assert!(!window.would_admit(0), "and so does zero");
    }

    #[test]
    fn test_window_limit_one_admits_one_per_closed_second() {
        let attempts: Vec<u64> = (0..=40).map(|i| i * 100 * MS).collect();
        let admitted = admitted_by_window(1, &attempts);
        assert_eq!(admitted, vec![0, 1_100 * MS, 2_200 * MS, 3_300 * MS]);
    }

    #[test]
    fn test_window_off_above_slot_cap() {
        assert!(OrderRateLimiter::new(1).window.is_some());
        assert!(OrderRateLimiter::new(10).window.is_some());
        let at_cap = u32::try_from(WINDOW_SLOTS).unwrap_or(u32::MAX);
        assert!(OrderRateLimiter::new(at_cap).window.is_some());
        assert!(OrderRateLimiter::new(at_cap + 1).window.is_none());
        let unlimited = OrderRateLimiter::new(u32::MAX);
        assert!(unlimited.window.is_none());
        for _ in 0..1_000 {
            assert!(unlimited.check().is_ok(), "u32::MAX means never refuse");
        }
    }

    #[test]
    fn test_production_limit_has_the_window() {
        let limit = tickvault_common::constants::SEBI_MAX_ORDERS_PER_SECOND;
        assert!(
            usize::try_from(limit).unwrap_or(usize::MAX) <= WINDOW_SLOTS,
            "the SEBI cap must sit inside the strict window"
        );
        assert!(OrderRateLimiter::new(limit).window.is_some());
    }

    #[test]
    fn test_check_refuses_the_eleventh_within_one_second_and_counts_it() {
        let limiter = OrderRateLimiter::new(10);
        for _ in 0..10 {
            assert!(limiter.check().is_ok());
        }
        assert!(matches!(limiter.check(), Err(OmsError::RateLimited)));
    }

    /// A copy of the ring, to prove a refusal left it untouched.
    fn ring_of(limiter: &OrderRateLimiter) -> SlidingWindow {
        match limiter.window.as_ref() {
            Some(window) => match window.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            },
            None => SlidingWindow::new(1),
        }
    }

    /// Runs one decision at a fixed window instant, with the real GCRA.
    fn admit_at(limiter: &OrderRateLimiter, now: u64) -> bool {
        match limiter.window.as_ref() {
            Some(window) => limiter.admit_with(window, || now),
            None => false,
        }
    }

    #[test]
    fn test_window_refusal_spends_no_gcra_cell() {
        // Limit 2: GCRA burst 2, refilling one cell every 500 ms of REAL time.
        let limiter = OrderRateLimiter::new(2);
        assert!(admit_at(&limiter, 0));
        assert!(admit_at(&limiter, MS));
        // Let the real-time GCRA refill both cells.
        std::thread::sleep(Duration::from_millis(1_100));
        // The window (fixed clock, still inside the second) refuses ...
        let before = ring_of(&limiter);
        assert!(!admit_at(&limiter, 2 * MS));
        assert_eq!(
            ring_of(&limiter),
            before,
            "a refusal must not touch the ring"
        );
        // ... and the GCRA still holds both cells: two admissions in a row
        // once the window has room. Had the refusal asked the GCRA first, one
        // cell would be gone and the second admission below would fail.
        assert!(admit_at(&limiter, 2 * SEC));
        assert!(admit_at(&limiter, 2 * SEC + MS));
    }

    #[test]
    fn test_gcra_refusal_records_nothing_in_the_window() {
        // Window instants 2 s apart, so the window always has room; the GCRA
        // (burst 3, real time) is what refuses the fourth.
        let limiter = OrderRateLimiter::new(3);
        for i in 0..3 {
            assert!(admit_at(&limiter, i * 2 * SEC));
        }
        let before = ring_of(&limiter);
        assert!(
            !admit_at(&limiter, 6 * SEC),
            "the GCRA refuses inside its burst"
        );
        assert_eq!(
            ring_of(&limiter),
            before,
            "a GCRA refusal must not be recorded"
        );
    }

    #[test]
    fn test_window_survives_a_poisoned_lock() {
        let limiter = std::sync::Arc::new(OrderRateLimiter::new(2));
        let clone = std::sync::Arc::clone(&limiter);
        let joined = std::thread::spawn(move || {
            if let Some(window) = clone.window.as_ref() {
                let _guard = window.lock();
                panic!("poison the window lock");
            }
        })
        .join();
        assert!(joined.is_err(), "the helper thread panicked as intended");
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_err());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

        /// Whatever the timing (bursts, gaps, repeated and backward instants),
        /// no closed 1-second window ever holds more than `limit` admissions.
        #[test]
        fn proptest_window_never_exceeds_n_in_any_closed_second(
            limit in 1usize..=WINDOW_SLOTS,
            // Mostly bursts (gaps under 5 ms) so even a 64-order limit fills
            // its ring inside one second, mixed with long gaps.
            steps in proptest::collection::vec(
                (
                    proptest::prop_oneof![3 => 0u64..5 * MS, 1 => 0u64..400 * MS],
                    proptest::bool::weighted(0.05),
                ),
                1..400,
            ),
        ) {
            let mut now: u64 = 10 * SEC;
            let mut attempts = Vec::with_capacity(steps.len());
            for (gap, step_back) in steps {
                now = if step_back { now.saturating_sub(gap) } else { now.saturating_add(gap) };
                attempts.push(now);
            }
            let admitted = admitted_by_window(limit, &attempts);
            proptest::prop_assert!(max_in_any_closed_second(&admitted) <= limit);
        }

        /// The window never refuses while fewer than `limit` admissions sit
        /// inside the last second: it is strict, not starved.
        #[test]
        fn proptest_window_admits_whenever_room_remains(
            limit in 1usize..=16,
            gaps in proptest::collection::vec(
                proptest::prop_oneof![3 => 0u64..5 * MS, 1 => 0u64..300 * MS],
                1..200,
            ),
        ) {
            let mut window = SlidingWindow::new(limit);
            let mut admitted: Vec<u64> = Vec::new();
            let mut now: u64 = 0;
            for gap in gaps {
                now += gap;
                let inside = admitted.iter().filter(|&&t| now - t <= SEC).count();
                let would = window.would_admit(now);
                proptest::prop_assert_eq!(would, inside < limit);
                if would {
                    window.record(now);
                    admitted.push(now);
                }
            }
        }
    }
}
