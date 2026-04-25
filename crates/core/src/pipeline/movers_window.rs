//! Rolling baseline state for multi-timeframe top-movers rankings.
//!
//! The movers tracker needs to answer "what was the price of security X
//! `T` seconds ago?" for every (security, timeframe) pair so it can compute
//! `change_pct = (now − T-ago) / T-ago` and rank the top movers over each
//! 1m..15m window. This module owns that state.
//!
//! # Design
//!
//! For each `(security_id, exchange_segment_code)` we keep a fixed-size
//! ring of 16 slots, one per minute, covering the last 15 minutes plus
//! the current minute:
//!
//! ```text
//!   slot 0   slot 1   slot 2   ...   slot 15
//!   ┌──────┐ ┌──────┐ ┌──────┐       ┌──────┐
//!   │ m+0  │ │ m+1  │ │ m+2  │  ...  │ m+15 │   (modulo 16, "m" = minute index)
//!   │ ltp₀ │ │ ltp₁ │ │ ltp₂ │       │ ltp₁₅│
//!   └──────┘ └──────┘ └──────┘       └──────┘
//! ```
//!
//! Slot index = `(minute_index % 16)`. Stored value is
//! `Some((minute_index, ltp_last_seen_in_that_minute))`. Last-tick-of-minute
//! semantics: multiple ticks within one minute overwrite, so the slot
//! always holds the most-recent price for that minute boundary.
//!
//! # Why 16 slots not 15
//!
//! We need 15 minutes of lookback PLUS the current minute (which is what
//! we record into). 15 + 1 = 16. A 15-slot ring would overwrite the
//! oldest readable minute the moment we try to advance.
//!
//! # Hot path
//!
//! `record` is called once per tick from `MoversTrackerV2::update_v2`. It
//! must be O(1) with zero allocation on the steady state. Lookup is O(1)
//! direct slot index — no scan, no binary search.
//!
//! # Memory bound
//!
//! 25,000 securities × 16 slots × 24 bytes (`Option<(i64, f32)>` with
//! niche-optimised tag) ≈ 9.6 MB worst case. Bounded.
//!
//! # Segment isolation (I-P1-11)
//!
//! Keyed on `(security_id, exchange_segment_code)` so that the same Dhan
//! id reused across segments (e.g. NIFTY id=13 IDX_I + an NSE_EQ id=13)
//! gets two independent rings. Required for cross-segment correctness.

use std::collections::HashMap;

/// Maximum lookback supported, in minutes. Must be `>=` the longest
/// supported `Timeframe::secs() / 60`. 15 covers Timeframe::FifteenMin.
pub const MAX_LOOKBACK_MINUTES: usize = 15;

/// Ring size = lookback + 1 to give room for the "current minute" slot
/// without overwriting the oldest readable minute.
pub const RING_SLOTS: usize = MAX_LOOKBACK_MINUTES + 1;

/// Per-security rolling price ring covering the last 16 minutes.
///
/// Each slot stores `(minute_index, ltp)` where
/// `minute_index = ist_epoch_secs / 60`. The slot for a given minute
/// is `minute_index % RING_SLOTS`, so slot reuse is automatic and no
/// explicit eviction loop is needed.
#[derive(Debug, Clone, Default)]
pub struct PriceRing {
    /// `samples[i] = Some((minute_index, ltp))` if minute_i has been
    /// recorded; otherwise `None` (warm-up state).
    samples: [Option<(i64, f32)>; RING_SLOTS],
}

impl PriceRing {
    /// Record `ltp` observed at `ist_secs`. O(1), zero-alloc.
    ///
    /// If the same minute already has a sample, overwrites with the
    /// later price (last-tick-of-minute semantics).
    #[inline]
    pub fn record(&mut self, ist_secs: i64, ltp: f32) {
        let minute = ist_secs / 60;
        // Modulo with a power-of-2 dividend (16) compiles to a bitwise AND.
        let slot = (minute.rem_euclid(RING_SLOTS as i64)) as usize;
        self.samples[slot] = Some((minute, ltp));
    }

    /// Look up the price as observed `lookback_secs` ago relative to
    /// `now_ist_secs`. Returns `None` if no sample exists for the target
    /// minute (warm-up period or older than 15 minutes).
    ///
    /// O(1) — direct slot index, no scan.
    #[inline]
    #[must_use]
    pub fn baseline_at(&self, now_ist_secs: i64, lookback_secs: u32) -> Option<f32> {
        let target_minute = (now_ist_secs - i64::from(lookback_secs)) / 60;
        let slot = (target_minute.rem_euclid(RING_SLOTS as i64)) as usize;
        match self.samples[slot] {
            Some((m, p)) if m == target_minute => Some(p),
            _ => None,
        }
    }

    /// Number of slots currently populated. Cold path — for tests + metrics.
    #[must_use]
    pub fn populated_slots(&self) -> usize {
        // O(1) EXEMPT: 16 fixed-size iterations, used only by tests + metrics.
        self.samples.iter().filter(|s| s.is_some()).count()
    }
}

/// Per-`(security_id, exchange_segment_code)` rolling baseline state.
///
/// Wraps a `HashMap` of `PriceRing`s keyed by the I-P1-11 composite
/// key. Insertion uses `entry().or_default()` so a new security is added
/// transparently on its first tick.
#[derive(Debug, Default)]
pub struct TimeframeBaselines {
    rings: HashMap<(u32, u8), PriceRing>,
}

impl TimeframeBaselines {
    /// Construct an empty baseline store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-allocate hash buckets for `n` securities. Call this at boot
    /// once the universe size is known to avoid rehash on the hot path.
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            rings: HashMap::with_capacity(n),
        }
    }

    /// Record a tick. O(1) amortised — first tick for a security pays a
    /// hashmap insertion; every subsequent tick is a hashmap get + ring
    /// slot write.
    #[inline]
    pub fn record(&mut self, security_id: u32, exchange_segment_code: u8, ist_secs: i64, ltp: f32) {
        self.rings
            .entry((security_id, exchange_segment_code))
            .or_default()
            .record(ist_secs, ltp);
    }

    /// Look up the price as-of `lookback_secs` ago for the given security.
    /// Returns `None` if the security has never been recorded, or if the
    /// target minute is in the warm-up window.
    #[inline]
    #[must_use]
    pub fn baseline_at(
        &self,
        security_id: u32,
        exchange_segment_code: u8,
        now_ist_secs: i64,
        lookback_secs: u32,
    ) -> Option<f32> {
        self.rings
            .get(&(security_id, exchange_segment_code))
            .and_then(|r| r.baseline_at(now_ist_secs, lookback_secs))
    }

    /// Count of distinct (security_id, segment) pairs being tracked.
    #[must_use]
    // TEST-EXEMPT: trivial getter — call sites covered by test_with_capacity_starts_empty + test_segment_isolation_per_i_p1_11
    pub fn len(&self) -> usize {
        self.rings.len()
    }

    /// True iff no securities have been recorded yet.
    #[must_use]
    // TEST-EXEMPT: trivial getter — call sites covered by test_with_capacity_starts_empty + test_new_constructor_starts_empty
    pub fn is_empty(&self) -> bool {
        self.rings.is_empty()
    }
}

// ===========================================================================
// Tests — Plan item E
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// E.1: a price recorded at minute M and looked up at minute M with
    /// lookback 0 returns the recorded price.
    #[test]
    fn test_record_then_lookup_zero_lookback_returns_price() {
        let mut baselines = TimeframeBaselines::new();
        baselines.record(13, 0, 60, 25_700.0); // minute 1
        let p = baselines.baseline_at(13, 0, 60, 0);
        assert_eq!(p, Some(25_700.0));
    }

    /// E.2: lookup of a security never recorded returns None.
    #[test]
    fn test_baseline_at_unknown_security_returns_none() {
        let baselines = TimeframeBaselines::new();
        assert_eq!(baselines.baseline_at(999, 1, 600, 60), None);
    }

    /// E.3: warmup — security recorded at minute 1, lookup at minute 1
    /// asking for 5-minute lookback returns None (no minute -4 sample).
    #[test]
    fn test_baseline_at_warmup_returns_none() {
        let mut baselines = TimeframeBaselines::new();
        // First tick at minute 1 (60s).
        baselines.record(13, 0, 60, 25_700.0);
        // Asking for the price 5 minutes ago when only minute 1 exists.
        let p = baselines.baseline_at(13, 0, 60, 300);
        assert_eq!(p, None);
    }

    /// E.4: rolling lookback — record across 5 minutes, then look back
    /// 1m, 2m, 3m, 4m and get back exactly the recorded prices.
    #[test]
    fn test_rolling_lookback_returns_recorded_prices() {
        let mut baselines = TimeframeBaselines::new();
        // Minute 0: 100, 1: 101, 2: 102, 3: 103, 4: 104.
        for m in 0..=4_i64 {
            baselines.record(13, 0, m * 60, 100.0 + m as f32);
        }
        let now = 4 * 60;
        assert_eq!(baselines.baseline_at(13, 0, now, 0), Some(104.0));
        assert_eq!(baselines.baseline_at(13, 0, now, 60), Some(103.0));
        assert_eq!(baselines.baseline_at(13, 0, now, 120), Some(102.0));
        assert_eq!(baselines.baseline_at(13, 0, now, 180), Some(101.0));
        assert_eq!(baselines.baseline_at(13, 0, now, 240), Some(100.0));
    }

    /// E.5: last-tick-of-minute semantics — multiple ticks within the
    /// same minute boundary overwrite each other; lookup returns the
    /// latest one.
    #[test]
    fn test_last_tick_of_minute_overwrites() {
        let mut baselines = TimeframeBaselines::new();
        // Three ticks all within minute 5 (300..360).
        baselines.record(13, 0, 300, 100.0);
        baselines.record(13, 0, 320, 101.0);
        baselines.record(13, 0, 359, 102.0);
        let now = 359;
        assert_eq!(baselines.baseline_at(13, 0, now, 0), Some(102.0));
    }

    /// E.6: 16-minute aging — a sample from 16 minutes ago has been
    /// overwritten by a same-slot newer sample, so lookup returns None.
    #[test]
    fn test_sample_older_than_15_minutes_returns_none() {
        let mut baselines = TimeframeBaselines::new();
        // Tick at minute 0.
        baselines.record(13, 0, 0, 50.0);
        // Tick at minute 16 — same slot (0 % 16 == 16 % 16 == 0),
        // overwrites the minute-0 sample.
        baselines.record(13, 0, 16 * 60, 200.0);
        let now = 16 * 60;
        // Asking for 16 minutes ago: target_minute = 0, slot = 0,
        // but slot now holds (16, 200.0). The minute-mismatch check
        // returns None.
        assert_eq!(baselines.baseline_at(13, 0, now, 16 * 60), None);
        // Current price still readable.
        assert_eq!(baselines.baseline_at(13, 0, now, 0), Some(200.0));
    }

    /// E.7 (I-P1-11): segment isolation. Same `security_id` in two
    /// different segments must have independent rings.
    /// Concrete repro: NIFTY id=13 in IDX_I (code 0) vs a stock id=13
    /// in NSE_EQ (code 1) (this scenario actually happens in production
    /// — see security-id-uniqueness.md).
    #[test]
    fn test_segment_isolation_per_i_p1_11() {
        let mut baselines = TimeframeBaselines::new();
        // Same id, different segments, different prices.
        baselines.record(13, 0, 60, 25_700.0); // NIFTY index value
        baselines.record(13, 1, 60, 1_368.10); // some stock (hypothetical)

        let p_idx = baselines.baseline_at(13, 0, 60, 0);
        let p_eq = baselines.baseline_at(13, 1, 60, 0);

        assert_eq!(p_idx, Some(25_700.0), "IDX_I segment must be its own ring");
        assert_eq!(p_eq, Some(1_368.10), "NSE_EQ segment must be its own ring");
        assert_ne!(p_idx, p_eq, "two segments must NOT share state");
        assert_eq!(
            baselines.len(),
            2,
            "two distinct (id, segment) pairs must produce two rings"
        );
    }

    /// E.8: `with_capacity` returns an empty store with the requested
    /// capacity reserved (smoke check — exact capacity is implementation
    /// detail, but it must start empty).
    #[test]
    fn test_with_capacity_starts_empty() {
        let baselines = TimeframeBaselines::with_capacity(25_000);
        assert!(baselines.is_empty());
        assert_eq!(baselines.len(), 0);
    }

    /// E.8b: `new()` (no-arg constructor) is equivalent to `default()` and
    /// produces an empty, ready-to-use store. Pub-fn coverage ratchet.
    #[test]
    fn test_new_constructor_starts_empty() {
        let baselines = TimeframeBaselines::new();
        assert!(baselines.is_empty());
        assert_eq!(baselines.len(), 0);
    }

    /// E.9: PriceRing populated_slots reports correctly during warmup.
    /// Ratchet for the warmup-progress metric we'll emit later.
    #[test]
    fn test_price_ring_populated_slots_during_warmup() {
        let mut ring = PriceRing::default();
        assert_eq!(ring.populated_slots(), 0);
        ring.record(60, 100.0);
        assert_eq!(ring.populated_slots(), 1);
        ring.record(120, 101.0);
        assert_eq!(ring.populated_slots(), 2);
        // Same minute again — overwrites, count stays at 2.
        ring.record(150, 101.5);
        assert_eq!(ring.populated_slots(), 2);
    }

    /// E.10: PriceRing within-minute overwrites do NOT advance the slot
    /// count, only minute boundary crossings do.
    #[test]
    fn test_price_ring_within_minute_does_not_advance_slot_count() {
        let mut ring = PriceRing::default();
        // 60 ticks all inside minute 1.
        for s in 60..=119 {
            ring.record(s, 100.0 + (s - 60) as f32);
        }
        assert_eq!(
            ring.populated_slots(),
            1,
            "same minute repeatedly recorded must occupy ONE slot"
        );
    }

    /// E.11: MAX_LOOKBACK_MINUTES is at least 15 (covers Timeframe::FifteenMin).
    /// If anyone reduces this constant, every 15m lookup will fail silently.
    #[test]
    fn test_max_lookback_covers_fifteen_minute_timeframe() {
        assert!(
            MAX_LOOKBACK_MINUTES >= 15,
            "MAX_LOOKBACK_MINUTES must be >= 15 to support Timeframe::FifteenMin"
        );
    }

    /// E.12: RING_SLOTS = MAX_LOOKBACK_MINUTES + 1 so the current minute
    /// slot does not overwrite the oldest readable minute.
    #[test]
    fn test_ring_slots_is_max_lookback_plus_one() {
        assert_eq!(RING_SLOTS, MAX_LOOKBACK_MINUTES + 1);
    }

    /// E.13: 14-minute lookup mid-window (right at the edge of
    /// supported lookback) returns the recorded minute price, not None.
    /// Catches off-by-one errors on the minute boundary math.
    #[test]
    fn test_fourteen_minute_lookback_at_boundary_works() {
        let mut baselines = TimeframeBaselines::new();
        // Minute 0 .. minute 14 — fully populated.
        for m in 0..=14_i64 {
            baselines.record(42, 2, m * 60, 200.0 + m as f32);
        }
        let now = 14 * 60;
        assert_eq!(
            baselines.baseline_at(42, 2, now, 14 * 60),
            Some(200.0),
            "14-minute lookback must return the minute-0 sample"
        );
    }
}
