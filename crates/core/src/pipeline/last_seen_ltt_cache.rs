//! Phase 0 Item 8+9 — PR-D8 (2026-05-17) — last-seen LTT cache.
//!
//! Lock-free per-`(security_id, segment_code)` cache of the most
//! recent `exchange_timestamp` (LTT, IST epoch seconds) observed in
//! the live tick stream. Populated by `tick_processor` on every tick;
//! read by `gap_fill_scheduler` at disconnect-event handling time
//! to refine the `outage_start_secs` window beyond the producer's
//! 10-second conservative default.
//!
//! ## Architectural lock
//!
//! PR-D7 extended `DisconnectResolvedEvent` with a per-conn snapshot
//! (`subscribed: Arc<Vec<(u32, u8)>>`). PR-D8 uses that snapshot to
//! query the per-SID last-seen-LTT at scheduler time and refines
//! `outage_start_secs` to `min(event.outage_start_secs, conn-level
//! min(last_seen_ltt for each subscribed sid))`. The refinement is
//! scheduler-side because:
//!
//! - The producer (`WebSocketConnection`) does not own the tick
//!   stream — `tick_processor` does. Putting the cache writes in the
//!   producer would require routing every tick back through the
//!   connection's task, which doubles hot-path cost.
//! - The scheduler is a cold-path consumer (~5 disconnect events per
//!   day under healthy conditions); the lookup cost is dominated by
//!   network + REST.
//!
//! ## Cloning is cheap
//!
//! Mirror of the `PrevOiCache` pattern. Internally `Arc<papaya::HashMap>`,
//! so every clone shares state. The boot path creates one
//! `LastSeenLttCache`, `Arc`-clones into the tick_processor task and
//! into `GapFillExecutorDeps` — both see the same map.

use std::sync::Arc;

use papaya::HashMap;

/// Composite key per I-P1-11: `(security_id, exchange_segment_code)`.
type CacheKey = (u32, u8);

/// Lock-free map from `(security_id, segment_code)` → last-seen LTT
/// (IST epoch seconds). Updates monotonically increase per
/// `(sid, segment)`; concurrent writers may race on insert but
/// since LTT advances monotonically the race outcome differs by at
/// most a few seconds — well within the `outage_start_secs`
/// refinement tolerance.
#[derive(Clone, Default)]
pub struct LastSeenLttCache {
    inner: Arc<HashMap<CacheKey, i64>>,
}

impl LastSeenLttCache {
    /// Construct an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the most recent tick for `(security_id, segment_code)`.
    ///
    /// Called on the hot path from `tick_processor`. `ltt_ist_secs`
    /// is the `exchange_timestamp` field of the parsed tick — already
    /// in IST epoch seconds per `data-integrity.md` "WebSocket
    /// Timestamp Rule". O(1), lock-free.
    ///
    /// Concurrent writers may race; on race the later wall-clock
    /// arrival wins, which is the desired semantics for "most recent
    /// observed LTT".
    #[inline]
    pub fn record(&self, security_id: u32, segment_code: u8, ltt_ist_secs: i64) {
        let guard = self.inner.guard();
        self.inner
            .insert((security_id, segment_code), ltt_ist_secs, &guard);
    }

    /// Look up the last-seen LTT for `(security_id, segment_code)`.
    /// Returns `None` if no tick has been observed yet (fresh boot,
    /// or instrument not on any subscribed conn).
    ///
    /// O(1), lock-free, safe on the hot path though the only caller
    /// today is the cold-path scheduler.
    #[inline]
    #[must_use]
    pub fn get(&self, security_id: u32, segment_code: u8) -> Option<i64> {
        let guard = self.inner.guard();
        self.inner
            .get(&(security_id, segment_code), &guard)
            .copied()
    }

    /// Refine an outage-start estimate using the cache.
    ///
    /// Returns the older of `producer_estimate` (the conservative
    /// `now - 10s` figure from the WebSocket producer) and the
    /// `min` last-seen LTT across `subscribed`. If no subscribed SID
    /// has been seen yet (e.g., fresh boot with no ticks), the
    /// producer estimate is returned unchanged.
    ///
    /// Pure function over the cache + slice — no allocation, no
    /// lock contention beyond the per-lookup pin guard.
    ///
    /// # Why `min` not `max`
    ///
    /// `outage_start_secs` is the earliest moment we are NOT
    /// confident we had data. The oldest last-seen across the conn's
    /// subscribed SIDs bounds that window conservatively — if the
    /// least-active SID was last seen at T, then beyond T we cannot
    /// distinguish "no ticks because outage" from "no ticks because
    /// illiquid". Refilling from `T` ensures no gap is missed for
    /// the active SIDs at the cost of possibly redundant fetches
    /// for the illiquid ones.
    #[must_use]
    pub fn refine_outage_start(&self, producer_estimate: i64, subscribed: &[(u32, u8)]) -> i64 {
        let mut refined = producer_estimate;
        for &(sid, seg) in subscribed {
            if let Some(last_seen) = self.get(sid, seg) {
                if last_seen < refined {
                    refined = last_seen;
                }
            }
        }
        refined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_cache_returns_none_on_get() {
        let cache = LastSeenLttCache::new();
        assert!(cache.get(13, 0).is_none());
    }

    #[test]
    fn test_record_then_get_roundtrip() {
        let cache = LastSeenLttCache::new();
        cache.record(13, 0, 1_700_000_000);
        assert_eq!(cache.get(13, 0), Some(1_700_000_000));
    }

    #[test]
    fn test_composite_key_distinguishes_segments_per_i_p1_11() {
        // I-P1-11: (sid, segment) is the unique key. Two distinct
        // segments with the same sid must NOT collide.
        let cache = LastSeenLttCache::new();
        cache.record(27, 0, 1_700_000_100); // FINNIFTY IDX_I
        cache.record(27, 1, 1_700_000_200); // some NSE_EQ with same sid=27
        assert_eq!(cache.get(27, 0), Some(1_700_000_100));
        assert_eq!(cache.get(27, 1), Some(1_700_000_200));
    }

    #[test]
    fn test_record_overwrites_existing_entry() {
        let cache = LastSeenLttCache::new();
        cache.record(13, 0, 1_700_000_000);
        cache.record(13, 0, 1_700_000_120);
        assert_eq!(cache.get(13, 0), Some(1_700_000_120));
    }

    #[test]
    fn test_refine_outage_start_with_empty_subscribed_returns_producer_estimate() {
        let cache = LastSeenLttCache::new();
        let refined = cache.refine_outage_start(1_700_000_500, &[]);
        assert_eq!(refined, 1_700_000_500);
    }

    #[test]
    fn test_refine_outage_start_with_no_cache_hits_returns_producer_estimate() {
        let cache = LastSeenLttCache::new();
        let refined = cache.refine_outage_start(1_700_000_500, &[(13, 0), (25, 0)]);
        assert_eq!(refined, 1_700_000_500);
    }

    #[test]
    fn test_refine_outage_start_picks_older_last_seen() {
        // Producer estimated outage_start = 1_700_000_500.
        // SID 13 was last seen at 1_700_000_400 (100s before estimate).
        // Refined estimate must be 1_700_000_400 (older).
        let cache = LastSeenLttCache::new();
        cache.record(13, 0, 1_700_000_400);
        let refined = cache.refine_outage_start(1_700_000_500, &[(13, 0)]);
        assert_eq!(refined, 1_700_000_400);
    }

    #[test]
    fn test_refine_outage_start_picks_min_across_multiple_sids() {
        // Conn has 3 SIDs. Producer estimated 1_700_000_500.
        // Last-seen LTTs: 13=480, 25=450, 51=490. min=450.
        let cache = LastSeenLttCache::new();
        cache.record(13, 0, 1_700_000_480);
        cache.record(25, 0, 1_700_000_450);
        cache.record(51, 0, 1_700_000_490);
        let refined = cache.refine_outage_start(1_700_000_500, &[(13, 0), (25, 0), (51, 0)]);
        assert_eq!(refined, 1_700_000_450);
    }

    #[test]
    fn test_refine_outage_start_keeps_producer_estimate_when_cache_is_newer() {
        // If every subscribed SID was seen AFTER the producer estimate,
        // the producer estimate is the conservative (older) bound and
        // refinement should leave it unchanged.
        let cache = LastSeenLttCache::new();
        cache.record(13, 0, 1_700_000_600);
        cache.record(25, 0, 1_700_000_700);
        let refined = cache.refine_outage_start(1_700_000_500, &[(13, 0), (25, 0)]);
        assert_eq!(refined, 1_700_000_500);
    }

    #[test]
    fn test_refine_outage_start_mixed_cache_hits_and_misses() {
        // SID 13 was seen at 480. SID 25 was never seen. SID 51 at 490.
        // min of producer (500) + 480 + 490 = 480.
        let cache = LastSeenLttCache::new();
        cache.record(13, 0, 1_700_000_480);
        cache.record(51, 0, 1_700_000_490);
        let refined = cache.refine_outage_start(1_700_000_500, &[(13, 0), (25, 0), (51, 0)]);
        assert_eq!(refined, 1_700_000_480);
    }

    #[test]
    fn test_clone_shares_state_via_arc() {
        // Mirror of PrevOiCache test pattern — cloning the cache
        // must share underlying state so multiple Arc holders see
        // the same writes.
        let cache = LastSeenLttCache::new();
        let clone = cache.clone();
        cache.record(13, 0, 1_700_000_400);
        assert_eq!(clone.get(13, 0), Some(1_700_000_400));
    }
}
