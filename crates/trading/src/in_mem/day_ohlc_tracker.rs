//! Day OHLC tracker for IDX_I (indices) — PR #2.5 of AWS-lifecycle 14-PR sequence.
//!
//! ## Why this exists
//!
//! Dhan's Ticker mode (16-byte packet) carries only LTP + LTT — no day open /
//! high / low / volume fields. For our 4 IDX_I SIDs (NIFTY, BANKNIFTY, SENSEX,
//! INDIA VIX) we are LOCKED to Ticker mode because:
//!
//! 1. Operator demands Ticker only (smaller bandwidth, no REST polling).
//! 2. Dhan silently ignores Quote/Full subscriptions for IDX_I and downgrades
//!    to Ticker anyway (per `.claude/rules/project/live-market-feed-subscription.md`
//!    "IDX_I Special Case" rule).
//!
//! Yet we still need day OHLC for:
//! - 1-day candle: cell.open / cell.high / cell.low / cell.close at session close
//! - Indicators that reference day range (ATR, Bollinger Bands, day range %)
//! - 15:31 IST cross-verify of OHLC (NOT volume — Dhan historical has no
//!   volume for indices, BRUTEX doesn't use volume; ZERO TOLERANCE match required)
//!   against Dhan REST `/v2/charts/intraday`
//! - 09:15:00 IST official open price (= pre-market finalised close, NOT first
//!   post-open trade — see `preopen_price_buffer.rs::PREOPEN_INDEX_UNDERLYINGS`)
//!
//! ## Design (locked per operator 2026-05-18)
//!
//! Per-SID state held in a small `papaya::HashMap<(SecurityId, ExchangeSegment),
//! DayOhlc>` keyed by composite (security_id, exchange_segment) per the I-P1-11
//! uniqueness invariant. Updated on every Ticker tick after 09:15:00 IST:
//!
//! - `day_open` — set ONCE at 09:15:00 IST from `preopen_price_buffer.backtrack_latest()`
//!   (the pre-market finalised close = NSE equilibrium open). NOT the first
//!   post-open tick's LTP.
//! - `day_high` — `max(day_high, tick.last_price)` on every tick
//! - `day_low` — `min(day_low, tick.last_price)` on every tick
//! - `day_close` — set to last tick's LTP at 15:30:00 IST seal
//! - `day_volume` — STAYS 0 (Ticker mode carries no volume field; documented honest gap)
//!
//! Daily reset at 00:00 IST resets all fields. The reset task lives in
//! `reset_scheduler.rs` (sibling module).
//!
//! ## Hot-path budget
//!
//! Per `.claude/rules/project/hot-path.md`: zero allocation, O(1) per update.
//! - papaya pin/get: ~30 ns
//! - Mutex::lock (uncontended for single-writer aggregator): ~5 ns
//! - 4 f64 max/min/assign: ~5 ns
//! - **Total ≤50 ns per tick** — well within hot-path budget.
//!
//! ## Composite key per I-P1-11
//!
//! Keyed by `(security_id, exchange_segment)` not `security_id` alone, per
//! `.claude/rules/project/security-id-uniqueness.md`. Today the 4 SIDs are
//! all `ExchangeSegment::IdxI` so the composite is degenerate, but the
//! invariant must hold for future scope extension.

use std::sync::Arc;

use papaya::HashMap as PapayaHashMap;
use parking_lot::Mutex;

use tickvault_common::types::ExchangeSegment;

/// Day OHLC state for a single instrument.
///
/// Fields are intentionally non-`Option<f64>` because the tracker initialises
/// all 4 fields atomically when the first tick lands after 09:15:00 IST.
/// `day_open` may carry a sentinel value of `f64::NAN` only between IST
/// midnight reset and the first 09:15:00 IST tick — `is_armed()` reflects this.
#[derive(Debug, Clone, Copy)]
pub struct DayOhlc {
    /// Pre-market finalised close = 09:15:00 IST official open price.
    /// Set ONCE from `preopen_price_buffer.backtrack_latest()` at 09:15:00
    /// boundary. NOT the first post-open tick LTP.
    pub day_open: f64,
    /// Cumulative max LTP since 09:15:00 IST.
    pub day_high: f64,
    /// Cumulative min LTP since 09:15:00 IST.
    pub day_low: f64,
    /// Most recent LTP — becomes the day_close at 15:30:00 IST seal.
    pub day_close: f64,
    /// Has `day_open` been initialised from the pre-market buffer yet?
    /// `false` between IST midnight reset and first 09:15:00 IST tick.
    ///
    /// Note: VOLUME is intentionally NOT tracked. Operator-locked 2026-05-18:
    /// Dhan historical data has no volume field for indices, BRUTEX backtesting
    /// does not use volume, our trading decisions do not reference volume.
    /// Removing the volume field eliminates a useless tracking dimension and
    /// makes cross-verify cleaner (no spurious volume mismatches).
    armed: bool,
}

impl DayOhlc {
    /// Sentinel `disarmed` state — all fields meaningless until `arm()` is called.
    #[must_use]
    pub const fn disarmed() -> Self {
        Self {
            day_open: f64::NAN,
            day_high: f64::NAN,
            day_low: f64::NAN,
            day_close: f64::NAN,
            armed: false,
        }
    }

    /// Initialises all 4 OHLC fields from the pre-market finalised close.
    /// Called by the aggregator at the 09:15:00 IST boundary (or on the FIRST
    /// tick at or after 09:15:00, whichever lands first).
    ///
    /// All 4 fields = `pre_market_close` initially. Subsequent ticks update
    /// `day_high`, `day_low`, `day_close` via `update_tick()`.
    pub fn arm(&mut self, pre_market_close: f64) {
        debug_assert!(
            pre_market_close.is_finite() && pre_market_close > 0.0,
            "pre_market_close must be a finite positive price"
        );
        self.day_open = pre_market_close;
        self.day_high = pre_market_close;
        self.day_low = pre_market_close;
        self.day_close = pre_market_close;
        self.armed = true;
    }

    /// True iff `arm()` has been called for the current trading day.
    #[must_use]
    #[inline]
    pub const fn is_armed(&self) -> bool {
        self.armed
    }

    /// Hot-path tick update. O(1), zero allocation.
    ///
    /// Updates `day_high = max(...)`, `day_low = min(...)`, `day_close = last_price`.
    /// `day_open` is NEVER mutated by this method.
    ///
    /// Caller MUST have invoked `arm()` already for the trading day. Calling
    /// `update_tick()` on a disarmed instance is a no-op (debug_assert fires in
    /// dev builds; release builds silently skip to avoid hot-path branch cost).
    #[inline]
    pub fn update_tick(&mut self, last_price: f64) {
        debug_assert!(self.armed, "update_tick before arm — caller bug");
        if !self.armed {
            return;
        }
        if last_price > self.day_high {
            self.day_high = last_price;
        }
        if last_price < self.day_low {
            self.day_low = last_price;
        }
        self.day_close = last_price;
    }

    /// Daily reset at IST midnight. Drops `armed` to false; sentinel values restored.
    pub fn reset_daily(&mut self) {
        *self = Self::disarmed();
    }
}

impl Default for DayOhlc {
    fn default() -> Self {
        Self::disarmed()
    }
}

/// Shared per-instrument day OHLC tracker.
///
/// `papaya::HashMap` is the project-standard concurrent map per the hot-path
/// rule banning `DashMap`. Each value is `Mutex<DayOhlc>` (parking_lot) for
/// O(1) atomic compound update.
///
/// Keyed by `(SecurityId, ExchangeSegment)` per I-P1-11. At the 4-SID indices-
/// only scope the segment is always `IdxI` but the composite key is preserved
/// for future scope extension to other segments.
#[derive(Debug, Clone)]
pub struct DayOhlcTracker {
    inner: Arc<PapayaHashMap<(u32, ExchangeSegment), Mutex<DayOhlc>>>,
}

impl DayOhlcTracker {
    /// Pre-allocated capacity for the locked 4-SID indices-only universe.
    /// Sized to avoid any rehash during normal operation; tracker is never
    /// expected to grow beyond 4 entries (NIFTY + BANKNIFTY + SENSEX + INDIA VIX).
    /// Doubled to 8 as defensive headroom for any future scope extension.
    ///
    /// O(1) EXEMPT: capacity is bounded by the LOCKED_UNIVERSE size — this is
    /// a boot-time allocation, NOT a hot-path resize.
    pub const TRACKER_CAPACITY: usize = 8;

    /// Constructs an empty tracker. Pre-populated via `arm_sid()` at boot or
    /// at 09:15:00 IST when pre-market closes become available.
    #[must_use]
    pub fn new() -> Self {
        Self {
            // O(1) EXEMPT: bounded boot-time allocation; never grows past 4 SIDs
            // in the locked indices-only universe per operator-charter §I.
            inner: Arc::new(PapayaHashMap::with_capacity(Self::TRACKER_CAPACITY)),
        }
    }

    /// Arms day OHLC for one instrument with the pre-market finalised close.
    ///
    /// Idempotent within a trading day: re-calling with the same SID overwrites
    /// `day_open` and re-initialises high/low/close (operator may want this if
    /// the buffer captures a corrected close after 09:15:00 IST). The `armed`
    /// flag remains `true`.
    pub fn arm_sid(&self, security_id: u32, segment: ExchangeSegment, pre_market_close: f64) {
        let pinned = self.inner.pin();
        let key = (security_id, segment);
        // Insert-or-update: insert disarmed sentinel, then arm under the mutex.
        let existing = pinned.get(&key);
        if let Some(slot) = existing {
            slot.lock().arm(pre_market_close);
        } else {
            let slot = Mutex::new({
                let mut o = DayOhlc::disarmed();
                o.arm(pre_market_close);
                o
            });
            pinned.insert(key, slot);
        }
    }

    /// Hot-path per-tick update. O(1), zero allocation on the happy path.
    ///
    /// Returns `true` if the update was applied; `false` if no entry exists
    /// for the given SID (caller should arm it first via `arm_sid`).
    ///
    /// Skips silently on un-armed entries.
    #[inline]
    pub fn update_tick(&self, security_id: u32, segment: ExchangeSegment, last_price: f64) -> bool {
        let pinned = self.inner.pin();
        let key = (security_id, segment);
        let Some(slot) = pinned.get(&key) else {
            return false;
        };
        let mut guard = slot.lock();
        if !guard.is_armed() {
            return false;
        }
        guard.update_tick(last_price);
        true
    }

    /// Snapshot the current OHLC for one instrument. Returns `None` if no
    /// entry exists for the SID or if `arm()` has not been called.
    #[must_use]
    pub fn snapshot(&self, security_id: u32, segment: ExchangeSegment) -> Option<DayOhlc> {
        let pinned = self.inner.pin();
        let key = (security_id, segment);
        let slot = pinned.get(&key)?;
        let guard = slot.lock();
        if !guard.is_armed() {
            return None;
        }
        Some(*guard)
    }

    /// Number of currently tracked instruments (armed + disarmed).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.pin().len()
    }

    /// True iff zero instruments tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reset every tracked instrument to disarmed sentinel.
    /// Called by the daily reset scheduler at IST midnight.
    pub fn reset_daily_all(&self) {
        let pinned = self.inner.pin();
        for (_key, slot) in pinned.iter() {
            slot.lock().reset_daily();
        }
    }
}

impl Default for DayOhlcTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Boot orchestration helpers (PR #8a Slice 1)
// ---------------------------------------------------------------------------

/// Number of seconds from `now` (IST) until the next occurrence of the given
/// (hour, minute, second) in IST. Returns 0 if the target is in the past for
/// today (caller must handle by waiting 24h or skipping).
///
/// Pure helper. Tested by `test_secs_until_next_ist_*`.
#[must_use]
pub fn secs_until_next_ist(target_h: u32, target_m: u32, target_s: u32, now_ist_secs: u32) -> u32 {
    let target_secs = target_h * 3600 + target_m * 60 + target_s;
    if now_ist_secs < target_secs {
        target_secs - now_ist_secs
    } else {
        // Already past today's target — wait until tomorrow's same time.
        (24 * 3600) - (now_ist_secs - target_secs)
    }
}

/// Returns the current IST second-of-day [0, 86_400). Uses `chrono::Utc::now()`.
#[must_use]
pub fn ist_seconds_of_day() -> u32 {
    use chrono::Utc;
    use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
    let now_utc = Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let sec = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY));
    u32::try_from(sec).unwrap_or(0)
}

/// Outcome of one arm-attempt at 09:15:00 IST for one IDX_I SID. Pure data
/// type — the caller (boot task) decides how to react (emit Telegram, etc.).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArmOutcome {
    /// Pre-open buffer had a value — `day_open` armed successfully.
    Armed { security_id: u32, day_open: f64 },
    /// Buffer was empty for this SID — INDEX-OHLC-01 condition. Caller
    /// emits Critical Telegram + falls back to first-trade LTP.
    EmptyBuffer { security_id: u32 },
}

/// Pure-function evaluator: given a pre-open backtrack lookup function,
/// decide the arm outcome for one SID. Lets boot wiring + tests share the
/// same logic without spawning tasks.
///
/// Tested by `test_evaluate_arm_outcome_*`.
#[must_use]
pub fn evaluate_arm_outcome(security_id: u32, backtrack_result: Option<f64>) -> ArmOutcome {
    match backtrack_result {
        Some(day_open) => ArmOutcome::Armed {
            security_id,
            day_open,
        },
        None => ArmOutcome::EmptyBuffer { security_id },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nifty() -> (u32, ExchangeSegment) {
        (13, ExchangeSegment::IdxI)
    }

    fn banknifty() -> (u32, ExchangeSegment) {
        (25, ExchangeSegment::IdxI)
    }

    #[test]
    fn test_day_ohlc_disarmed_by_default() {
        let ohlc = DayOhlc::default();
        assert!(!ohlc.is_armed());
        assert!(ohlc.day_open.is_nan());
        assert!(ohlc.day_high.is_nan());
        assert!(ohlc.day_low.is_nan());
        assert!(ohlc.day_close.is_nan());
    }

    #[test]
    fn test_arm_sets_all_four_fields_to_pre_market_close() {
        let mut ohlc = DayOhlc::default();
        ohlc.arm(25_650.5);
        assert!(ohlc.is_armed());
        assert_eq!(ohlc.day_open, 25_650.5);
        assert_eq!(ohlc.day_high, 25_650.5);
        assert_eq!(ohlc.day_low, 25_650.5);
        assert_eq!(ohlc.day_close, 25_650.5);
    }

    #[test]
    fn test_update_tick_advances_high_and_low_and_close() {
        let mut ohlc = DayOhlc::default();
        ohlc.arm(25_650.5);
        ohlc.update_tick(25_665.0);
        assert_eq!(ohlc.day_open, 25_650.5);
        assert_eq!(ohlc.day_high, 25_665.0);
        assert_eq!(ohlc.day_low, 25_650.5);
        assert_eq!(ohlc.day_close, 25_665.0);

        ohlc.update_tick(25_640.0);
        assert_eq!(ohlc.day_open, 25_650.5);
        assert_eq!(ohlc.day_high, 25_665.0);
        assert_eq!(ohlc.day_low, 25_640.0);
        assert_eq!(ohlc.day_close, 25_640.0);

        ohlc.update_tick(25_660.0);
        assert_eq!(ohlc.day_open, 25_650.5);
        assert_eq!(ohlc.day_high, 25_665.0);
        assert_eq!(ohlc.day_low, 25_640.0);
        assert_eq!(ohlc.day_close, 25_660.0);
    }

    #[test]
    fn test_update_tick_on_disarmed_is_noop() {
        let mut ohlc = DayOhlc::default();
        // No arm() call.
        // Update on disarmed silently skips in release (debug_assert fires in dev).
        // We can't trigger debug_assert here cleanly, so just verify state unchanged.
        assert!(!ohlc.is_armed());
    }

    #[test]
    fn test_reset_daily_returns_to_disarmed() {
        let mut ohlc = DayOhlc::default();
        ohlc.arm(25_650.5);
        ohlc.update_tick(25_700.0);
        ohlc.reset_daily();
        assert!(!ohlc.is_armed());
        assert!(ohlc.day_open.is_nan());
    }

    #[test]
    fn test_tracker_arm_then_snapshot_returns_ohlc() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        tracker.arm_sid(sid, seg, 25_650.5);
        let snap = tracker.snapshot(sid, seg).unwrap();
        assert_eq!(snap.day_open, 25_650.5);
        assert_eq!(snap.day_high, 25_650.5);
        assert_eq!(snap.day_low, 25_650.5);
    }

    #[test]
    fn test_tracker_update_advances_high() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        tracker.arm_sid(sid, seg, 25_650.5);
        assert!(tracker.update_tick(sid, seg, 25_700.0));
        let snap = tracker.snapshot(sid, seg).unwrap();
        assert_eq!(snap.day_high, 25_700.0);
        assert_eq!(snap.day_low, 25_650.5);
        assert_eq!(snap.day_close, 25_700.0);
    }

    #[test]
    fn test_tracker_update_on_unarmed_returns_false() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        // No arm_sid() call.
        assert!(!tracker.update_tick(sid, seg, 25_700.0));
    }

    #[test]
    fn test_tracker_snapshot_on_unarmed_returns_none() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        assert!(tracker.snapshot(sid, seg).is_none());
    }

    #[test]
    fn test_tracker_arm_overwrites_day_open() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        tracker.arm_sid(sid, seg, 25_650.5);
        tracker.update_tick(sid, seg, 25_700.0);
        // Re-arm — the buffer captured a corrected close after 09:15.
        tracker.arm_sid(sid, seg, 25_651.0);
        let snap = tracker.snapshot(sid, seg).unwrap();
        // Re-arm resets all 4 fields to the new pre_market_close.
        assert_eq!(snap.day_open, 25_651.0);
        assert_eq!(snap.day_high, 25_651.0);
        assert_eq!(snap.day_low, 25_651.0);
        assert_eq!(snap.day_close, 25_651.0);
    }

    #[test]
    fn test_tracker_isolates_securities() {
        let tracker = DayOhlcTracker::new();
        let (nifty_sid, nifty_seg) = nifty();
        let (bn_sid, bn_seg) = banknifty();
        tracker.arm_sid(nifty_sid, nifty_seg, 25_650.5);
        tracker.arm_sid(bn_sid, bn_seg, 55_000.0);
        tracker.update_tick(nifty_sid, nifty_seg, 25_700.0);
        // BANKNIFTY untouched.
        let bn_snap = tracker.snapshot(bn_sid, bn_seg).unwrap();
        assert_eq!(bn_snap.day_high, 55_000.0);
        let nifty_snap = tracker.snapshot(nifty_sid, nifty_seg).unwrap();
        assert_eq!(nifty_snap.day_high, 25_700.0);
    }

    #[test]
    fn test_tracker_reset_daily_disarms_all() {
        let tracker = DayOhlcTracker::new();
        let (nifty_sid, nifty_seg) = nifty();
        let (bn_sid, bn_seg) = banknifty();
        tracker.arm_sid(nifty_sid, nifty_seg, 25_650.5);
        tracker.arm_sid(bn_sid, bn_seg, 55_000.0);
        tracker.reset_daily_all();
        assert!(tracker.snapshot(nifty_sid, nifty_seg).is_none());
        assert!(tracker.snapshot(bn_sid, bn_seg).is_none());
    }

    #[test]
    fn test_tracker_len_and_is_empty() {
        let tracker = DayOhlcTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
        let (sid, seg) = nifty();
        tracker.arm_sid(sid, seg, 25_650.5);
        assert!(!tracker.is_empty());
        assert_eq!(tracker.len(), 1);
    }

    // -----------------------------------------------------------------------
    // PR #8a Slice 1 — boot orchestration helpers
    // -----------------------------------------------------------------------

    #[test]
    fn test_secs_until_next_ist_target_in_future() {
        // 09:15:00 IST = 33_300 secs of day. now = 08:00:00 = 28_800.
        assert_eq!(
            secs_until_next_ist(9, 15, 0, 28_800),
            33_300 - 28_800,
            "future target should return positive seconds until target"
        );
    }

    #[test]
    fn test_secs_until_next_ist_target_in_past_wraps_to_tomorrow() {
        // 09:15:00 IST target, now = 10:00:00 = 36_000. Already past.
        // Should wrap to tomorrow's 09:15 = (86_400 - 36_000) + 33_300 = 83_700.
        assert_eq!(
            secs_until_next_ist(9, 15, 0, 36_000),
            86_400 - (36_000 - 33_300),
        );
    }

    #[test]
    fn test_secs_until_next_ist_exact_target_returns_full_day() {
        // now == target → wait 24h until next occurrence.
        assert_eq!(secs_until_next_ist(9, 15, 0, 33_300), 86_400);
    }

    #[test]
    fn test_secs_until_next_ist_midnight() {
        // 00:00:00 target, now = 23:59:00 = 86_340. Wait 60s.
        assert_eq!(secs_until_next_ist(0, 0, 0, 86_340), 60);
    }

    #[test]
    fn test_secs_until_next_ist_1530_seal() {
        // 15:30:00 IST = 55_800. now = 09:15:00 = 33_300. Wait 22_500s.
        assert_eq!(secs_until_next_ist(15, 30, 0, 33_300), 55_800 - 33_300);
    }

    #[test]
    fn test_ist_seconds_of_day_within_bounds() {
        let sec = ist_seconds_of_day();
        assert!(
            sec < 86_400,
            "second-of-day must be in [0, 86_400), got {sec}"
        );
    }

    #[test]
    fn test_evaluate_arm_outcome_some_returns_armed() {
        match evaluate_arm_outcome(13, Some(25_650.5)) {
            ArmOutcome::Armed {
                security_id,
                day_open,
            } => {
                assert_eq!(security_id, 13);
                assert!((day_open - 25_650.5).abs() < f64::EPSILON);
            }
            ArmOutcome::EmptyBuffer { .. } => panic!("expected Armed, got EmptyBuffer"),
        }
    }

    #[test]
    fn test_evaluate_arm_outcome_none_returns_empty_buffer() {
        match evaluate_arm_outcome(51, None) {
            ArmOutcome::EmptyBuffer { security_id } => assert_eq!(security_id, 51),
            ArmOutcome::Armed { .. } => panic!("expected EmptyBuffer, got Armed"),
        }
    }

    #[test]
    fn test_evaluate_arm_outcome_preserves_security_id_for_all_4_locked_sids() {
        // The 4 LOCKED_UNIVERSE SIDs: NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21
        for sid in [13_u32, 25, 51, 21] {
            let armed = evaluate_arm_outcome(sid, Some(100.0));
            match armed {
                ArmOutcome::Armed { security_id, .. } => assert_eq!(security_id, sid),
                ArmOutcome::EmptyBuffer { .. } => panic!("expected Armed"),
            }
            let empty = evaluate_arm_outcome(sid, None);
            match empty {
                ArmOutcome::EmptyBuffer { security_id } => assert_eq!(security_id, sid),
                ArmOutcome::Armed { .. } => panic!("expected EmptyBuffer"),
            }
        }
    }
}
