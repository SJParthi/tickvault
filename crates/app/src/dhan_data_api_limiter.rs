//! Shared self-tuning Dhan Data-API rate limiter (operator pacing
//! directive 2026-07-14, relayed via the coordinator session):
//!
//! > pace Dhan to 3 requests/sec (tunable DOWN to 2), spread overflow into
//! > the next second(s), route the option-chain API through the SAME
//! > limiter, with incremental/decremental self-tuning — "if it accepts
//! > max 3 or 2, stick to that and split it up". Dhan-ONLY; Groww
//! > untouched.
//!
//! ONE process-wide async token-bucket limiter that every per-minute Dhan
//! Data-API REST fire passes through: the spot-1m per-minute fires, the
//! in-minute ladder re-polls, the 15:33:30 post-session sweep, the #1524
//! serving-delay diagnostic probes, AND the option-chain per-minute fires
//! + expirylist warmup/probe. The routing choke points are
//! `spot_1m_rest_boot::spot_1m_fetch_once` and
//! `option_chain_1m_boot::chain_fetch_once` — every enumerated caller
//! funnels through those two fns, so one [`DhanDataApiLimiter::acquire`]
//! per fn covers the whole scope (wiring pinned by
//! `crates/app/tests/dhan_data_api_limiter_wiring_guard.rs`). The chain
//! leg's 1-unique-per-3s per-underlying gap stays LAYERED ON TOP,
//! unchanged.
//!
//! ## HONESTY — what this limiter does and does NOT do
//! 2026-07-14 live session: the spot-1m leg was 0/980 (0%) with ~244
//! wasted 429s — the ladder re-fired ~20 req/min against all-empty
//! responses (the first two ladder rungs are 0.7s apart → ~8 requests in
//! a rolling second at the fire instant). THE LIMITER ELIMINATES THE 429
//! WASTE AND MAKES US A GOOD API CITIZEN — IT DOES NOT MAKE DHAN SERVE
//! SAME-DAY CANDLES. The 429s are a symptom of hammering empties, not
//! the root cause of the 0%.
//!
//! ## Mechanics
//! - Pre-built `governor` GCRA cells, one per legal rps level
//!   (`DHAN_DATA_API_RPS_FLOOR..=DHAN_DATA_API_RPS_CEILING` = 2..=4) —
//!   governor quotas are FIXED at construction, so "rate switching" =
//!   swapping the active pre-built cell through an `ArcSwap` (lock-free
//!   reads on every acquire).
//! - [`DhanDataApiLimiter::acquire`] awaits `until_ready()` on the active
//!   cell — overflow naturally spills into the next second(s) instead of
//!   erroring or dropping.
//! - Self-tuning is the pure, unit-tested [`RpsTuner`] FSM fed by REAL
//!   HTTP-429 observations ([`DhanDataApiLimiter::record_429`] at the two
//!   fetch fns' `StatusCode::TOO_MANY_REQUESTS` arms): ≥
//!   [`DHAN_DATA_API_TUNER_429_STEP_DOWN_THRESHOLD`] 429s inside a
//!   rolling [`DHAN_DATA_API_TUNER_429_WINDOW_MINUTES`]-minute window →
//!   ONE step DOWN to the 2 rps floor (the operator's literal "step DOWN
//!   to 2"; at the default target 3 that is also exactly one level); a
//!   clean streak of [`DHAN_DATA_API_TUNER_CLEAN_MINUTES_FOR_STEP_UP`]
//!   minutes → ONE step back UP one level toward the config cap. Every
//!   transition is logged ONCE (edge — never per-request) + the
//!   `tv_dhan_data_api_rps` gauge +
//!   `tv_dhan_data_api_tuner_transitions_total{direction}` counter.
//!
//! ## Honest envelope
//! - An in-flight `acquire` that started on the OLD cell finishes there
//!   (bounded — one request); every later acquire uses the new level.
//! - The limiter state is process-scoped: a restart re-learns from the
//!   config target (fresh window) — the same session-scoped envelope as
//!   the spot leg's FailureEdge/PersistTracker.
//! - Worst-case queue depth at a minute boundary is ~7 concurrent
//!   requests (4 spot + 3 chain) → ≤ ~4s wait at the 2 rps floor —
//!   inside both legs' 20s hard budgets (the per-request timeout still
//!   bounds each HTTP leg; a pathological queue surfaces as the EXISTING
//!   budget-exceeded failure, counted + coalesced-logged, never silent).
//! - Scope boundary: the boot-time prev-day fetch (its own 4-rps governor
//!   cell) and the 15:31 bulk cross-verify (its own `OrderRateLimiter`)
//!   were NOT in the operator's enumerated scope and run in disjoint
//!   windows — unifying them is a flagged follow-up. Groww is UNTOUCHED.
//!
//! Runbook: `.claude/rules/project/rest-1m-pipeline-error-codes.md`
//! (2026-07-14 section).

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwap;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use tracing::{error, info, warn};

use tickvault_common::constants::{
    DHAN_DATA_API_DEFAULT_TARGET_RPS, DHAN_DATA_API_RPS_CEILING, DHAN_DATA_API_RPS_FLOOR,
    DHAN_DATA_API_TUNER_429_STEP_DOWN_THRESHOLD, DHAN_DATA_API_TUNER_429_WINDOW_MINUTES,
    DHAN_DATA_API_TUNER_CLEAN_MINUTES_FOR_STEP_UP,
};

// ---------------------------------------------------------------------------
// Pure self-tuning FSM
// ---------------------------------------------------------------------------

/// What the limiter must APPLY after feeding the tuner one observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuneAction {
    /// Step DOWN to the floor after a 429 burst (edge — window cleared so
    /// one burst can never cascade).
    StepDown { from: u32, to: u32 },
    /// Step UP one level toward the config cap after a clean streak.
    StepUp { from: u32, to: u32 },
}

/// Pure decision core of the self-tuner. NO clock, NO I/O — callers feed
/// explicit minute indices (wall-clock minutes since the UNIX epoch), so
/// every transition is deterministic and unit-testable.
///
/// State machine:
/// - `on_429(minute)` — records one observed HTTP-429. When the rolling
///   window's total reaches the step-down threshold AND the current rate
///   is above the floor, steps DOWN to the floor (the operator's literal
///   "step DOWN to 2 rps") and clears the window + streak.
/// - `advance(minute)` — rolls the window forward; each fully completed
///   429-free minute extends the clean streak; at the streak threshold
///   (and below the cap) steps UP one level and resets the streak. At most
///   ONE step-up per call (bounded — an overnight idle gap never ratchets
///   multiple levels in one observation).
/// - `set_cap(cap)` — reconfigure: clamps the cap into the legal ladder;
///   a current rate above the new cap steps down immediately; a RAISED
///   cap never jumps the current rate (upward movement still requires the
///   clean streak) unless the tuner has never stepped down (fresh boot
///   configure).
#[derive(Debug)]
pub struct RpsTuner {
    cap_rps: u32,
    current_rps: u32,
    /// The most recent minute index observed (window head).
    minute: Option<u64>,
    /// Per-minute 429 counts for the rolling window, newest first;
    /// `counts[0]` is the CURRENT minute, `counts[i]` is `minute - i`.
    counts: [u32; DHAN_DATA_API_TUNER_429_WINDOW_MINUTES as usize],
    clean_streak: u32,
    ever_stepped_down: bool,
}

impl RpsTuner {
    /// Build at the (clamped) target cap.
    #[must_use]
    pub fn new(target_rps: u32) -> Self {
        let cap = clamp_rps(target_rps);
        Self {
            cap_rps: cap,
            current_rps: cap,
            minute: None,
            counts: [0; DHAN_DATA_API_TUNER_429_WINDOW_MINUTES as usize],
            clean_streak: 0,
            ever_stepped_down: false,
        }
    }

    /// The rate the limiter should currently enforce.
    #[must_use]
    pub fn current_rps(&self) -> u32 {
        self.current_rps
    }

    /// The configured cap (clamped).
    #[must_use]
    pub fn cap_rps(&self) -> u32 {
        self.cap_rps
    }

    /// Roll the window to `minute`, crediting completed clean minutes.
    /// Returns the step-up action when the clean streak matures.
    pub fn advance(&mut self, minute: u64) -> Option<TuneAction> {
        let Some(head) = self.minute else {
            self.minute = Some(minute);
            return None;
        };
        if minute <= head {
            // Same minute (or a clock step backwards — never rewound).
            return None;
        }
        let gap = minute - head;
        // Every fully COMPLETED minute in (head..minute) with zero 429s
        // extends the clean streak; any 429'd minute resets it. Bounded
        // fold: beyond the window, completed minutes are 429-free by
        // construction (nothing was recorded in them).
        let window_len = self.counts.len() as u64;
        let in_window = gap.min(window_len);
        for i in 0..in_window {
            // counts[0] is the minute closing now, then older shifts.
            let idx = i as usize;
            if self.counts[idx] == 0 {
                self.clean_streak = self.clean_streak.saturating_add(1);
            } else {
                self.clean_streak = 0;
            }
        }
        if gap > window_len {
            // The idle remainder had zero observations — all clean, but
            // bound the credit so one call can never mature more than one
            // full streak (deterministic, no unbounded loop).
            let idle =
                (gap - window_len).min(u64::from(DHAN_DATA_API_TUNER_CLEAN_MINUTES_FOR_STEP_UP));
            #[allow(clippy::cast_possible_truncation)] // APPROVED: bounded by the min() above
            {
                self.clean_streak = self.clean_streak.saturating_add(idle as u32);
            }
        }
        // Shift the window: everything recorded is now `gap` minutes older.
        if gap >= window_len {
            self.counts = [0; DHAN_DATA_API_TUNER_429_WINDOW_MINUTES as usize];
        } else {
            let shift = gap as usize;
            for i in (shift..self.counts.len()).rev() {
                self.counts[i] = self.counts[i - shift];
            }
            for slot in self.counts.iter_mut().take(shift) {
                *slot = 0;
            }
        }
        self.minute = Some(minute);
        // At most ONE step-up per advance call.
        if self.current_rps < self.cap_rps
            && self.clean_streak >= DHAN_DATA_API_TUNER_CLEAN_MINUTES_FOR_STEP_UP
        {
            let from = self.current_rps;
            self.current_rps = (self.current_rps + 1).min(self.cap_rps);
            self.clean_streak = 0;
            return Some(TuneAction::StepUp {
                from,
                to: self.current_rps,
            });
        }
        None
    }

    /// Record one observed HTTP-429 at `minute`. Returns the step-down
    /// action when the window total trips the threshold.
    pub fn on_429(&mut self, minute: u64) -> Option<TuneAction> {
        // Roll the window first. A step-up that matures on this same
        // observation is carried through UNLESS the fresh 429 immediately
        // trips the step-down (whose `from` then reflects the raised
        // level) — either way exactly ONE action reaches the caller and
        // the tuner state and the active cell stay in lockstep.
        let advance_action = self.advance(minute);
        if self.minute.is_none() {
            self.minute = Some(minute);
        }
        self.counts[0] = self.counts[0].saturating_add(1);
        self.clean_streak = 0;
        let window_total: u32 = self.counts.iter().sum();
        if window_total >= DHAN_DATA_API_TUNER_429_STEP_DOWN_THRESHOLD
            && self.current_rps > DHAN_DATA_API_RPS_FLOOR
        {
            let from = self.current_rps;
            // The operator's literal directive: step DOWN to 2 rps. At the
            // default target 3 this is also exactly one level.
            self.current_rps = DHAN_DATA_API_RPS_FLOOR;
            self.ever_stepped_down = true;
            // Clear the window so one burst can never cascade a second
            // decision; the streak restarts from the reduced rate.
            self.counts = [0; DHAN_DATA_API_TUNER_429_WINDOW_MINUTES as usize];
            self.clean_streak = 0;
            return Some(TuneAction::StepDown {
                from,
                to: self.current_rps,
            });
        }
        advance_action
    }

    /// Reconfigure the cap. A current rate above the new cap steps down
    /// immediately; a fresh tuner (never stepped down) follows a raised
    /// cap directly, otherwise upward movement still requires the clean
    /// streak.
    pub fn set_cap(&mut self, cap: u32) -> Option<TuneAction> {
        let cap = clamp_rps(cap);
        self.cap_rps = cap;
        if self.current_rps > cap {
            let from = self.current_rps;
            self.current_rps = cap;
            return Some(TuneAction::StepDown { from, to: cap });
        }
        if self.current_rps < cap && !self.ever_stepped_down {
            let from = self.current_rps;
            self.current_rps = cap;
            return Some(TuneAction::StepUp { from, to: cap });
        }
        None
    }
}

/// Clamp an rps value into the legal ladder (belt + braces — the config
/// `validate()` already rejects out-of-range values at boot).
#[must_use]
pub fn clamp_rps(rps: u32) -> u32 {
    rps.clamp(DHAN_DATA_API_RPS_FLOOR, DHAN_DATA_API_RPS_CEILING)
}

/// Wall-clock minute index (minutes since the UNIX epoch). The tuner only
/// compares indices, so the epoch base is irrelevant.
fn minute_index_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 60)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The shared limiter
// ---------------------------------------------------------------------------

/// The active level: an rps value + its pre-built GCRA cell.
struct ActiveLevel {
    rps: u32,
    limiter: Arc<DefaultDirectRateLimiter>,
}

/// Process-wide Dhan Data-API pacing gate. Cold REST path only — never
/// touched by the tick hot path.
pub struct DhanDataApiLimiter {
    /// Pre-built cells for every legal level, indexed by
    /// `rps - DHAN_DATA_API_RPS_FLOOR` (governor quotas are fixed at
    /// construction — switching = swapping the active pre-built cell).
    levels: Vec<Arc<DefaultDirectRateLimiter>>,
    active: ArcSwap<ActiveLevel>,
    tuner: Mutex<RpsTuner>,
}

impl DhanDataApiLimiter {
    /// Build with every legal level pre-constructed and the (clamped)
    /// target as the active cell.
    #[must_use]
    pub fn new(target_rps: u32) -> Self {
        let levels: Vec<Arc<DefaultDirectRateLimiter>> = (DHAN_DATA_API_RPS_FLOOR
            ..=DHAN_DATA_API_RPS_CEILING)
            .map(|rps| {
                let quota = Quota::per_second(NonZeroU32::new(rps).unwrap_or(NonZeroU32::MIN));
                Arc::new(RateLimiter::direct(quota))
            })
            .collect();
        let tuner = RpsTuner::new(target_rps);
        let current = tuner.current_rps();
        let active = ArcSwap::from_pointee(ActiveLevel {
            rps: current,
            limiter: Arc::clone(&levels[level_index(current)]),
        });
        metrics::gauge!("tv_dhan_data_api_rps").set(f64::from(current));
        Self {
            levels,
            active,
            tuner: Mutex::new(tuner),
        }
    }

    /// The currently enforced rps level (observability / tests).
    #[must_use]
    pub fn current_rps(&self) -> u32 {
        self.active.load().rps
    }

    /// Wait for one Dhan Data-API request permit. Overflow spills into the
    /// next second(s) — never an error, never a drop. Also rolls the
    /// tuner's clean-minute clock (lazy — no background task).
    pub async fn acquire(&self) {
        let action = {
            let mut tuner = self.lock_tuner();
            tuner.advance(minute_index_now())
        };
        if let Some(action) = action {
            self.apply(action);
        }
        let level = self.active.load_full();
        level.limiter.until_ready().await;
        metrics::counter!("tv_dhan_data_api_acquires_total").increment(1);
    }

    /// Feed one observed HTTP-429 (called at the two fetch fns' REAL
    /// `StatusCode::TOO_MANY_REQUESTS` arms — never a substring scan).
    pub fn record_429(&self) {
        let action = {
            let mut tuner = self.lock_tuner();
            tuner.on_429(minute_index_now())
        };
        if let Some(action) = action {
            self.apply(action);
        }
    }

    /// Reconfigure the tuning cap from `[dhan_data_api] target_rps`
    /// (called at the Dhan spawn seams; idempotent).
    pub fn configure_target_rps(&self, target_rps: u32) {
        let clamped = clamp_rps(target_rps);
        if clamped != target_rps {
            warn!(
                requested = target_rps,
                clamped,
                "dhan_data_api: target_rps outside the 2..=4 ladder — \
                 clamped (config validate() should have rejected this)"
            );
        }
        let action = {
            let mut tuner = self.lock_tuner();
            tuner.set_cap(clamped)
        };
        if let Some(action) = action {
            self.apply(action);
        }
        info!(
            target_rps = clamped,
            current_rps = self.current_rps(),
            "dhan_data_api: shared Data-API limiter configured \
             (operator pacing directive 2026-07-14)"
        );
    }

    /// Poisoning-safe tuner lock (short critical sections, no await held).
    fn lock_tuner(&self) -> std::sync::MutexGuard<'_, RpsTuner> {
        self.tuner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Apply one tuner transition: swap the active pre-built cell, stamp
    /// the gauge + counter, log ONCE (edge — never per-request).
    fn apply(&self, action: TuneAction) {
        match action {
            TuneAction::StepDown { from, to } => {
                self.swap_level(to);
                metrics::counter!("tv_dhan_data_api_tuner_transitions_total", "direction" => "down")
                    .increment(1);
                // Loud (coalesced — once per transition): Dhan is
                // rejecting the current pace; we back off to the floor
                // and keep serving from there.
                error!(
                    from_rps = from,
                    to_rps = to,
                    "dhan_data_api: 429 burst at the current pace — stepped \
                     the shared Data-API limiter DOWN (self-tuning; requests \
                     now spread across more seconds, nothing is dropped)"
                );
            }
            TuneAction::StepUp { from, to } => {
                self.swap_level(to);
                metrics::counter!("tv_dhan_data_api_tuner_transitions_total", "direction" => "up")
                    .increment(1);
                info!(
                    from_rps = from,
                    to_rps = to,
                    "dhan_data_api: clean streak at the reduced pace — \
                     stepped the shared Data-API limiter UP one level \
                     toward the config target"
                );
            }
        }
    }

    fn swap_level(&self, rps: u32) {
        let rps = clamp_rps(rps);
        self.active.store(Arc::new(ActiveLevel {
            rps,
            limiter: Arc::clone(&self.levels[level_index(rps)]),
        }));
        metrics::gauge!("tv_dhan_data_api_rps").set(f64::from(rps));
    }
}

/// Index of a (clamped) rps level in the pre-built cell vec.
fn level_index(rps: u32) -> usize {
    (clamp_rps(rps) - DHAN_DATA_API_RPS_FLOOR) as usize
}

/// The ONE process-wide limiter. Lazily built at the directed 3 rps
/// default; the Dhan spawn seams call
/// [`configure_shared_dhan_data_api_limiter`] with the config value
/// (fail-safe: an unconfigured process still paces at 3).
pub fn shared_dhan_data_api_limiter() -> &'static DhanDataApiLimiter {
    static SHARED: OnceLock<DhanDataApiLimiter> = OnceLock::new();
    SHARED.get_or_init(|| DhanDataApiLimiter::new(DHAN_DATA_API_DEFAULT_TARGET_RPS))
}

/// Configure the shared limiter's tuning cap from
/// `[dhan_data_api] target_rps` (called at both Dhan spawn seams).
pub fn configure_shared_dhan_data_api_limiter(target_rps: u32) {
    shared_dhan_data_api_limiter().configure_target_rps(target_rps);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: u32 = DHAN_DATA_API_RPS_FLOOR;
    const CEIL: u32 = DHAN_DATA_API_RPS_CEILING;
    const THRESH: u32 = DHAN_DATA_API_TUNER_429_STEP_DOWN_THRESHOLD;
    const CLEAN: u32 = DHAN_DATA_API_TUNER_CLEAN_MINUTES_FOR_STEP_UP;

    #[test]
    fn test_tuner_steps_down_to_floor_on_429_burst() {
        let mut t = RpsTuner::new(3);
        let mut action = None;
        for _ in 0..THRESH {
            action = t.on_429(100);
        }
        assert_eq!(
            action,
            Some(TuneAction::StepDown { from: 3, to: FLOOR }),
            "the threshold-th 429 inside one window must trip ONE step-down \
             to the floor"
        );
        assert_eq!(t.current_rps(), FLOOR);
        // The window was cleared — the very next 429 must NOT re-trip.
        assert_eq!(t.on_429(100), None, "cleared window must not cascade");
    }

    #[test]
    fn test_tuner_holds_below_threshold() {
        let mut t = RpsTuner::new(3);
        for _ in 0..THRESH - 1 {
            assert_eq!(t.on_429(100), None);
        }
        assert_eq!(t.current_rps(), 3, "below the threshold the rate holds");
        // The two 429s age out of the rolling window; a later single 429
        // (window total 1 < threshold) still holds.
        assert_eq!(
            t.on_429(100 + DHAN_DATA_API_TUNER_429_WINDOW_MINUTES + 1),
            None
        );
        assert_eq!(t.current_rps(), 3);
    }

    #[test]
    fn test_tuner_window_expires_old_429s() {
        let mut t = RpsTuner::new(3);
        // threshold-1 429s at minute 100, then one more AFTER the window
        // rolled past minute 100 — total inside the window is 1, no trip.
        for _ in 0..THRESH - 1 {
            t.on_429(100);
        }
        let far = 100 + DHAN_DATA_API_TUNER_429_WINDOW_MINUTES;
        assert_eq!(t.on_429(far), None, "aged-out 429s must not count");
        assert_eq!(t.current_rps(), 3);
    }

    #[test]
    fn test_tuner_steps_up_one_level_after_clean_streak() {
        let mut t = RpsTuner::new(3);
        for _ in 0..THRESH {
            t.on_429(100);
        }
        assert_eq!(t.current_rps(), FLOOR);
        // Clean minutes accumulate one per completed 429-free minute.
        let mut action = None;
        for m in 101..=(100 + u64::from(CLEAN)) {
            action = t.advance(m);
        }
        assert_eq!(
            action,
            Some(TuneAction::StepUp {
                from: FLOOR,
                to: FLOOR + 1
            }),
            "a clean streak must step UP exactly one level"
        );
        assert_eq!(t.current_rps(), FLOOR + 1);
    }

    #[test]
    fn test_tuner_never_below_floor_never_above_cap() {
        let mut t = RpsTuner::new(FLOOR);
        // Already at the floor: a burst changes nothing.
        for _ in 0..THRESH * 2 {
            t.on_429(50);
        }
        assert_eq!(t.current_rps(), FLOOR, "never below the floor");
        // Clean streaks at the cap change nothing.
        let mut t = RpsTuner::new(3);
        for m in 1..=u64::from(CLEAN) * 3 {
            assert_eq!(t.advance(m), None, "at the cap there is no step-up");
        }
        assert_eq!(t.current_rps(), 3, "never above the cap");
        // The constructor clamps a hostile cap.
        let t = RpsTuner::new(999);
        assert_eq!(t.cap_rps(), CEIL);
        let t = RpsTuner::new(0);
        assert_eq!(t.cap_rps(), FLOOR);
    }

    #[test]
    fn test_tuner_idle_gap_credits_bounded_clean_minutes() {
        let mut t = RpsTuner::new(3);
        for _ in 0..THRESH {
            t.on_429(100);
        }
        assert_eq!(t.current_rps(), FLOOR);
        // One giant idle gap (overnight) matures AT MOST one step-up.
        let action = t.advance(100 + 10_000);
        assert_eq!(
            action,
            Some(TuneAction::StepUp {
                from: FLOOR,
                to: FLOOR + 1
            }),
            "an idle gap credits a bounded clean streak — one step per call"
        );
    }

    #[test]
    fn test_tuner_set_cap_clamps_current_downward() {
        let mut t = RpsTuner::new(4);
        assert_eq!(
            t.set_cap(2),
            Some(TuneAction::StepDown { from: 4, to: 2 }),
            "lowering the cap below the current rate steps down immediately"
        );
        assert_eq!(t.current_rps(), 2);
        // Raising the cap on a FRESH tuner (never stepped down on 429s)
        // follows directly — the boot-configure path.
        assert_eq!(t.set_cap(3), Some(TuneAction::StepUp { from: 2, to: 3 }));
        // After a real 429 step-down, a raised cap does NOT jump current.
        for _ in 0..THRESH {
            t.on_429(10);
        }
        assert_eq!(t.current_rps(), FLOOR);
        assert_eq!(
            t.set_cap(4),
            None,
            "post-429 upward movement requires the clean streak"
        );
        assert_eq!(t.current_rps(), FLOOR);
        assert_eq!(t.cap_rps(), 4);
    }

    #[test]
    fn test_limiter_levels_prebuilt_and_quota_matches_rps() {
        let limiter = DhanDataApiLimiter::new(3);
        assert_eq!(limiter.current_rps(), 3);
        assert_eq!(
            limiter.levels.len(),
            (CEIL - FLOOR + 1) as usize,
            "every legal level must be pre-built"
        );
        // Deterministic quota check (no wall-clock sleeps): a fresh 2-rps
        // cell admits exactly 2 immediate permits, then defers.
        let two = &limiter.levels[level_index(2)];
        assert!(two.check().is_ok());
        assert!(two.check().is_ok());
        assert!(
            two.check().is_err(),
            "the 3rd immediate permit at 2 rps must defer to a later second"
        );
    }

    #[test]
    fn test_configure_clamps_out_of_range_target() {
        let limiter = DhanDataApiLimiter::new(3);
        limiter.configure_target_rps(999);
        assert_eq!(
            limiter.current_rps(),
            CEIL,
            "a hostile target clamps to the ceiling (validate() is the \
             real gate; this is belt + braces)"
        );
        limiter.configure_target_rps(0);
        assert_eq!(limiter.current_rps(), FLOOR);
    }

    #[test]
    fn test_limiter_record_429_swaps_active_level() {
        let limiter = DhanDataApiLimiter::new(3);
        for _ in 0..THRESH {
            limiter.record_429();
        }
        assert_eq!(
            limiter.current_rps(),
            FLOOR,
            "a 429 burst must swap the ACTIVE cell down to the floor"
        );
    }

    #[tokio::test]
    async fn test_acquire_completes_and_counts() {
        // Smoke: acquire never errors/never drops — the first permits of a
        // fresh cell are immediate.
        let limiter = DhanDataApiLimiter::new(3);
        limiter.acquire().await;
        limiter.acquire().await;
        assert_eq!(limiter.current_rps(), 3);
    }
}
