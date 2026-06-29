//! Day OHLC tracker for IDX_I (indices).
//!
//! ## Why this exists
//!
//! Dhan's Ticker mode (16-byte packet) carries only LTP + LTT — no day open /
//! high / low / volume fields. For our 4 IDX_I SIDs (NIFTY, BANKNIFTY, SENSEX,
//! INDIA VIX) we are LOCKED to Ticker mode per the operator-charter §I
//! WebSocket scope lock.
//!
//! Yet we still need day OHLC for:
//! - 1-day candle: cell.open / cell.high / cell.low / cell.close at session close
//! - Indicators that reference day range (ATR, Bollinger Bands, day range %)
//!
//! ## Design (locked 2026-05-26 — pre-market buffer deletion)
//!
//! Per-SID state held in a small `papaya::HashMap<(SecurityId, ExchangeSegment),
//! DayOhlc>` keyed by composite (security_id, exchange_segment) per the I-P1-11
//! uniqueness invariant. Updated on every Ticker tick:
//!
//! - `day_open` — auto-armed from the FIRST tick observed for the SID after
//!   the daily reset. NOT from a pre-market REST fetch. This means
//!   `day_open == first traded LTP after 09:15:00 IST` (or whenever the first
//!   tick lands). Operator accepts the ~paise difference vs NSE equilibrium
//!   open since Dhan historical / cross-verify / backfill is being removed.
//! - `day_high` — `max(day_high, tick.last_price)` on every tick
//! - `day_low` — `min(day_low, tick.last_price)` on every tick
//! - `day_close` — set to last tick's LTP at 15:30:00 IST seal
//! - `day_volume` — STAYS 0 (Ticker mode carries no volume field; documented gap)
//!
//! Daily reset at 00:00 IST resets all fields. The reset task lives in
//! `day_ohlc_orchestrator.rs::spawn_midnight_reset_task`.
//!
//! ## Hot-path budget
//!
//! Per `.claude/rules/project/hot-path.md`: zero allocation, O(1) per update.
//! Bench-tested under `bench_score_compute_le_1us` — first-tick auto-arm adds
//! a single branch (~1 ns).
//!
//! ## Composite key per I-P1-11
//!
//! Keyed by `(security_id, exchange_segment)` not `security_id` alone, per
//! `.claude/rules/project/security-id-uniqueness.md`. The 4 SIDs are all
//! `ExchangeSegment::IdxI` so the composite is degenerate today, but the
//! invariant must hold for future scope extension.

use std::sync::Arc;

use papaya::HashMap as PapayaHashMap;
use parking_lot::Mutex;

use tickvault_common::types::ExchangeSegment;

/// Day OHLC state for a single instrument.
///
/// Fields are intentionally non-`Option<f64>` because the tracker initialises
/// all 4 fields atomically when the first tick lands. `day_open` may carry a
/// sentinel value of `f64::NAN` only between IST midnight reset and the first
/// tick — `is_armed()` reflects this.
#[derive(Debug, Clone, Copy)]
pub struct DayOhlc {
    /// First-traded LTP for the trading day. Auto-armed from the first
    /// `update_tick()` call after a daily reset; never mutated thereafter.
    pub day_open: f64,
    /// Cumulative max LTP since first tick.
    pub day_high: f64,
    /// Cumulative min LTP since first tick.
    pub day_low: f64,
    /// Most recent LTP — becomes the day_close at 15:30:00 IST seal.
    pub day_close: f64,
    /// Has `day_open` been initialised by the first tick yet?
    /// `false` between IST midnight reset and first post-reset tick.
    ///
    /// Note: VOLUME is intentionally NOT tracked. Operator-locked 2026-05-18:
    /// Dhan historical data has no volume field for indices, BRUTEX backtesting
    /// does not use volume, our trading decisions do not reference volume.
    armed: bool,
}

impl DayOhlc {
    /// Sentinel `disarmed` state — all fields meaningless until first tick.
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

    /// Initialises all 4 OHLC fields from the first tick's LTP.
    /// Called internally by `update_tick` on the first call after a reset.
    fn arm_from_first_tick(&mut self, first_tick_price: f64) {
        debug_assert!(
            first_tick_price.is_finite() && first_tick_price > 0.0,
            "first_tick_price must be a finite positive price"
        );
        self.day_open = first_tick_price;
        self.day_high = first_tick_price;
        self.day_low = first_tick_price;
        self.day_close = first_tick_price;
        self.armed = true;
    }

    /// True iff a tick has been observed for the current trading day.
    #[must_use]
    #[inline]
    pub const fn is_armed(&self) -> bool {
        self.armed
    }

    /// Hot-path tick update. O(1), zero allocation.
    ///
    /// On the FIRST call after a daily reset, auto-arms by setting all 4 fields
    /// to `last_price`. On subsequent calls, updates `day_high`/`day_low`/`day_close`.
    /// `day_open` is set ONCE per trading day and never mutated thereafter.
    #[inline]
    pub fn update_tick(&mut self, last_price: f64) {
        if !self.armed {
            self.arm_from_first_tick(last_price);
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
    inner: Arc<PapayaHashMap<(u64, ExchangeSegment), Mutex<DayOhlc>>>,
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

    /// Constructs an empty tracker. Populated lazily by the first
    /// `update_tick()` call for each SID.
    #[must_use]
    pub fn new() -> Self {
        Self {
            // O(1) EXEMPT: bounded boot-time allocation; never grows past 4 SIDs
            // in the locked indices-only universe per operator-charter §I.
            inner: Arc::new(PapayaHashMap::with_capacity(Self::TRACKER_CAPACITY)),
        }
    }

    /// Hot-path per-tick update. O(1), zero allocation on the happy path.
    ///
    /// On the FIRST call for a given (security_id, segment) after a daily reset
    /// (or on a fresh tracker), auto-arms `day_open = day_high = day_low =
    /// day_close = last_price`. On subsequent calls, updates high/low/close.
    ///
    /// Returns `true` always (the call is now infallible — auto-arm + update).
    #[inline]
    pub fn update_tick(&self, security_id: u64, segment: ExchangeSegment, last_price: f64) -> bool {
        let pinned = self.inner.pin();
        let key = (security_id, segment);
        if let Some(slot) = pinned.get(&key) {
            slot.lock().update_tick(last_price);
            return true;
        }
        // First tick for this SID — create the slot and arm it.
        let mut fresh = DayOhlc::disarmed();
        fresh.update_tick(last_price);
        pinned.insert(key, Mutex::new(fresh));
        true
    }

    /// Snapshot the current OHLC for one instrument. Returns `None` if no
    /// tick has been observed yet (slot does not exist).
    #[must_use]
    pub fn snapshot(&self, security_id: u64, segment: ExchangeSegment) -> Option<DayOhlc> {
        let pinned = self.inner.pin();
        let key = (security_id, segment);
        let slot = pinned.get(&key)?;
        let guard = slot.lock();
        if !guard.is_armed() {
            return None;
        }
        Some(*guard)
    }

    /// Number of currently tracked instruments.
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
// Boot orchestration helpers
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

#[cfg(test)]
mod tests {
    use super::*;

    fn nifty() -> (u64, ExchangeSegment) {
        (13, ExchangeSegment::IdxI)
    }

    fn banknifty() -> (u64, ExchangeSegment) {
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
    fn test_first_tick_auto_arms_all_four_fields() {
        let mut ohlc = DayOhlc::default();
        ohlc.update_tick(25_650.5);
        assert!(ohlc.is_armed());
        assert_eq!(ohlc.day_open, 25_650.5);
        assert_eq!(ohlc.day_high, 25_650.5);
        assert_eq!(ohlc.day_low, 25_650.5);
        assert_eq!(ohlc.day_close, 25_650.5);
    }

    #[test]
    fn test_subsequent_ticks_never_mutate_day_open() {
        let mut ohlc = DayOhlc::default();
        ohlc.update_tick(25_650.5);
        ohlc.update_tick(25_665.0);
        ohlc.update_tick(25_640.0);
        ohlc.update_tick(25_660.0);
        // day_open MUST remain the first tick's LTP forever.
        assert_eq!(ohlc.day_open, 25_650.5);
        assert_eq!(ohlc.day_high, 25_665.0);
        assert_eq!(ohlc.day_low, 25_640.0);
        assert_eq!(ohlc.day_close, 25_660.0);
    }

    #[test]
    fn test_reset_daily_returns_to_disarmed() {
        let mut ohlc = DayOhlc::default();
        ohlc.update_tick(25_650.5);
        ohlc.update_tick(25_700.0);
        ohlc.reset_daily();
        assert!(!ohlc.is_armed());
        assert!(ohlc.day_open.is_nan());
    }

    #[test]
    fn test_post_reset_first_tick_re_arms_to_new_price() {
        let mut ohlc = DayOhlc::default();
        ohlc.update_tick(25_650.5);
        ohlc.reset_daily();
        ohlc.update_tick(25_700.0);
        assert!(ohlc.is_armed());
        assert_eq!(ohlc.day_open, 25_700.0);
        assert_eq!(ohlc.day_high, 25_700.0);
        assert_eq!(ohlc.day_low, 25_700.0);
        assert_eq!(ohlc.day_close, 25_700.0);
    }

    #[test]
    fn test_tracker_first_tick_creates_slot_and_arms() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        assert!(tracker.update_tick(sid, seg, 25_650.5));
        let snap = tracker.snapshot(sid, seg).unwrap();
        assert_eq!(snap.day_open, 25_650.5);
        assert_eq!(snap.day_high, 25_650.5);
        assert_eq!(snap.day_low, 25_650.5);
    }

    #[test]
    fn test_tracker_update_advances_high() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        tracker.update_tick(sid, seg, 25_650.5);
        tracker.update_tick(sid, seg, 25_700.0);
        let snap = tracker.snapshot(sid, seg).unwrap();
        assert_eq!(snap.day_open, 25_650.5);
        assert_eq!(snap.day_high, 25_700.0);
        assert_eq!(snap.day_low, 25_650.5);
        assert_eq!(snap.day_close, 25_700.0);
    }

    #[test]
    fn test_tracker_snapshot_on_fresh_sid_returns_none() {
        let tracker = DayOhlcTracker::new();
        let (sid, seg) = nifty();
        assert!(tracker.snapshot(sid, seg).is_none());
    }

    #[test]
    fn test_tracker_isolates_securities() {
        let tracker = DayOhlcTracker::new();
        let (nifty_sid, nifty_seg) = nifty();
        let (bn_sid, bn_seg) = banknifty();
        tracker.update_tick(nifty_sid, nifty_seg, 25_650.5);
        tracker.update_tick(bn_sid, bn_seg, 55_000.0);
        tracker.update_tick(nifty_sid, nifty_seg, 25_700.0);
        // BANKNIFTY day_open untouched.
        let bn_snap = tracker.snapshot(bn_sid, bn_seg).unwrap();
        assert_eq!(bn_snap.day_open, 55_000.0);
        assert_eq!(bn_snap.day_high, 55_000.0);
        let nifty_snap = tracker.snapshot(nifty_sid, nifty_seg).unwrap();
        assert_eq!(nifty_snap.day_open, 25_650.5);
        assert_eq!(nifty_snap.day_high, 25_700.0);
    }

    #[test]
    fn test_tracker_reset_daily_disarms_all() {
        let tracker = DayOhlcTracker::new();
        let (nifty_sid, nifty_seg) = nifty();
        let (bn_sid, bn_seg) = banknifty();
        tracker.update_tick(nifty_sid, nifty_seg, 25_650.5);
        tracker.update_tick(bn_sid, bn_seg, 55_000.0);
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
        tracker.update_tick(sid, seg, 25_650.5);
        assert!(!tracker.is_empty());
        assert_eq!(tracker.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Boot orchestration helpers
    // -----------------------------------------------------------------------

    #[test]
    fn test_secs_until_next_ist_target_in_future() {
        assert_eq!(secs_until_next_ist(9, 15, 0, 28_800), 33_300 - 28_800);
    }

    #[test]
    fn test_secs_until_next_ist_target_in_past_wraps_to_tomorrow() {
        assert_eq!(
            secs_until_next_ist(9, 15, 0, 36_000),
            86_400 - (36_000 - 33_300),
        );
    }

    #[test]
    fn test_secs_until_next_ist_exact_target_returns_full_day() {
        assert_eq!(secs_until_next_ist(9, 15, 0, 33_300), 86_400);
    }

    #[test]
    fn test_secs_until_next_ist_midnight() {
        assert_eq!(secs_until_next_ist(0, 0, 0, 86_340), 60);
    }

    #[test]
    fn test_secs_until_next_ist_1530_seal() {
        assert_eq!(secs_until_next_ist(15, 30, 0, 33_300), 55_800 - 33_300);
    }

    #[test]
    fn test_ist_seconds_of_day_within_bounds() {
        let sec = ist_seconds_of_day();
        assert!(sec < 86_400);
    }
}
