//! Wave-5 §K-L13 — frozen-per-day reference cache for the
//! seal-time pct stamping path.
//!
//! Holds [`PrevDayRefs`] keyed on `(security_id, exchange_segment)`
//! per I-P1-11. Loaded once at boot from:
//!
//! - `previous_close` cache (PR #466) → `prev_day_close` +
//!   `prev_day_total_volume`
//! - `prev_oi_loader` cache (PR #454) → `prev_day_oi`
//!
//! Frozen for the entire trading session. The IST 09:15 reset task
//! [`crate::in_mem::run_tick_storage_daily_reset`] already drains the
//! tick ring; the prev-day cache is **NOT** reset at the same time
//! because the cache values are loaded BEFORE 09:15 and represent
//! day-(N-1)'s closing values (the source of truth for today's % computations).
//! A fresh boot the next morning re-loads the cache from the
//! refreshed bhavcopy + option-chain feeds.
//!
//! ## Concurrency
//!
//! `papaya` for O(1) lock-free reads on the seal hot path. Insert /
//! clear are cold-path operations called only at boot + on the daily
//! refresh hook. `Arc` clones are cheap atomic refcount bumps.
//!
//! ## Memory footprint
//!
//! `PrevDayRefs` is 24 bytes (1 × f64 + 2 × i64). At 11K instruments
//! the cache holds ~264 KB of value bytes plus papaya overhead — far
//! below the L18 component budget (`registry` component is sized for
//! ~5 MB total per the §AA design).
//!
//! ## Why a separate cache (not a method on InstrumentRegistry)?
//!
//! `InstrumentRegistry` carries instrument metadata (lot size,
//! expiry, etc.) that's static for the trading session. PrevDayRefs
//! are similarly day-scoped but come from a DIFFERENT data source
//! (bhavcopy / option-chain REST instead of CSV). Co-mingling them
//! would couple two source-of-truth pipelines that should remain
//! independent (the bhavcopy loader fails independently of the CSV
//! refresh, and the operator should see distinct alerts).

use std::sync::Arc;

use papaya::HashMap;

use crate::candles::pct_stamping::PrevDayRefs;

/// Composite key per I-P1-11 — `(security_id, exchange_segment_code)`.
type PrevDayKey = (u64, u8);

/// In-RAM cache of frozen-per-day reference values per instrument.
///
/// Cloneable: `Arc::clone` on the inner papaya map. Multiple
/// consumers (the cascade seal-stamping site + the
/// `subsystem_memory` sampler) hold cheap clones.
#[derive(Clone, Default)]
pub struct PrevDayCache {
    inner: Arc<HashMap<PrevDayKey, PrevDayRefs>>,
}

impl PrevDayCache {
    /// Build an empty cache. The boot-time loader (lives in the app
    /// crate) calls [`PrevDayCache::insert`] for every (security_id,
    /// segment) pair before the cascade starts emitting sealed bars.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert / overwrite the prev-day refs for one instrument.
    ///
    /// Cold path — called by the boot-time loader once per instrument
    /// (typically ~11K calls, all before market open). The papaya
    /// pin / insert costs ~50 ns per call → ~550 µs total at boot,
    /// well within the 8s pre-market budget.
    pub fn insert(&self, security_id: u64, exchange_segment_code: u8, refs: PrevDayRefs) {
        let pin = self.inner.pin();
        pin.insert((security_id, exchange_segment_code), refs);
    }

    /// O(1) lock-free lookup. Returns `None` if the instrument was
    /// not present in the bhavcopy + option-chain feeds (newly listed
    /// contract, T+0 listing day).
    ///
    /// Hot path on the seal-stamping site — `papaya::HashMap::pin` +
    /// `get` totals ~30 ns. Acceptable on the SEAL path (called once
    /// per timeframe boundary, not per tick).
    #[must_use]
    pub fn lookup(&self, security_id: u64, exchange_segment_code: u8) -> Option<PrevDayRefs> {
        let pin = self.inner.pin();
        pin.get(&(security_id, exchange_segment_code)).copied()
    }

    /// Clear every entry. Used by the daily refresh hook (called once
    /// per IST midnight rollover when the bhavcopy loader re-publishes
    /// the cache for the new trading day).
    ///
    /// Cold path. Iterates the keys + removes each entry. `O(N)`.
    pub fn clear(&self) {
        let pin = self.inner.pin();
        // papaya does not expose a bulk clear — iterate + remove.
        // Collect keys first to avoid mutating during iteration.
        let keys: Vec<PrevDayKey> = pin.iter().map(|(k, _)| *k).collect(); // APPROVED: cold-path daily-refresh — clear() runs once per IST midnight rollover, NOT per-tick; bounded by len_instruments() ~11K
        for k in keys {
            pin.remove(&k);
        }
    }

    /// Number of `(security_id, segment_code)` entries in the cache.
    /// Cold-path read.
    #[must_use]
    pub fn len_instruments(&self) -> usize {
        let pin = self.inner.pin();
        pin.len()
    }

    /// Estimated bytes for the
    /// `tv_subsystem_memory_estimated_bytes{component="registry"}`
    /// gauge (#504a / L18). Reports `len() × size_of::<PrevDayRefs>()`,
    /// matching the lazy-RSS reconciliation contract.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        let len = self.len_instruments() as u64;
        let per_entry = std::mem::size_of::<PrevDayRefs>() as u64;
        len.saturating_mul(per_entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_refs(close: f64, oi: i64, vol: i64) -> PrevDayRefs {
        PrevDayRefs {
            prev_day_close: close,
            prev_day_oi: oi,
            prev_day_total_volume: vol,
        }
    }

    #[test]
    fn test_prev_day_cache_new_is_empty() {
        let cache = PrevDayCache::new();
        assert_eq!(cache.len_instruments(), 0);
        assert_eq!(cache.estimated_bytes(), 0);
    }

    #[test]
    fn test_insert_and_lookup_round_trip() {
        let cache = PrevDayCache::new();
        let refs = make_refs(100.0, 1_000_000, 10_000_000);
        cache.insert(1234, 1, refs);
        let got = cache
            .lookup(1234, 1)
            .expect("inserted entry should be retrievable");
        assert_eq!(got, refs);
    }

    #[test]
    fn test_lookup_returns_none_for_unknown_instrument() {
        let cache = PrevDayCache::new();
        cache.insert(1234, 1, make_refs(100.0, 0, 0));
        assert!(cache.lookup(9999, 1).is_none());
        assert!(cache.lookup(1234, 2).is_none());
    }

    /// I-P1-11 ratchet: same `security_id`, different segment must
    /// resolve to distinct cache entries. Pre-segment-aware bug class
    /// would have collapsed both into one cache entry.
    #[test]
    fn test_prev_day_cache_isolates_cross_segment_collisions_per_i_p1_11() {
        let cache = PrevDayCache::new();
        cache.insert(27, 0, make_refs(18000.0, 0, 0)); // FINNIFTY IDX_I
        cache.insert(27, 1, make_refs(555.0, 0, 0)); // hypothetical NSE_EQ id=27
        let idx = cache.lookup(27, 0).expect("IDX_I entry");
        let eq = cache.lookup(27, 1).expect("NSE_EQ entry");
        assert_eq!(idx.prev_day_close, 18000.0);
        assert_eq!(eq.prev_day_close, 555.0);
        assert_eq!(cache.len_instruments(), 2);
    }

    #[test]
    fn test_insert_overwrites_existing_entry() {
        let cache = PrevDayCache::new();
        cache.insert(1234, 1, make_refs(100.0, 1_000_000, 10_000_000));
        cache.insert(1234, 1, make_refs(105.0, 1_500_000, 20_000_000));
        let got = cache.lookup(1234, 1).expect("entry");
        assert_eq!(got.prev_day_close, 105.0);
        assert_eq!(got.prev_day_oi, 1_500_000);
        assert_eq!(got.prev_day_total_volume, 20_000_000);
    }

    #[test]
    fn test_clear_removes_all_entries() {
        let cache = PrevDayCache::new();
        cache.insert(1234, 1, make_refs(100.0, 0, 0));
        cache.insert(5678, 2, make_refs(200.0, 0, 0));
        cache.insert(27, 0, make_refs(18000.0, 0, 0));
        assert_eq!(cache.len_instruments(), 3);
        cache.clear();
        assert_eq!(cache.len_instruments(), 0);
        assert!(cache.lookup(1234, 1).is_none());
        assert!(cache.lookup(5678, 2).is_none());
    }

    #[test]
    fn test_clone_shares_state_via_arc() {
        let cache_a = PrevDayCache::new();
        let cache_b = cache_a.clone();
        cache_a.insert(1234, 1, make_refs(100.0, 0, 0));
        // Clone B sees the state.
        assert!(cache_b.lookup(1234, 1).is_some());
        assert_eq!(cache_b.len_instruments(), 1);
    }

    #[test]
    fn test_estimated_bytes_scales_with_entry_count() {
        let cache = PrevDayCache::new();
        let per_entry = std::mem::size_of::<PrevDayRefs>() as u64;
        assert_eq!(cache.estimated_bytes(), 0);
        cache.insert(1234, 1, make_refs(100.0, 0, 0));
        assert_eq!(cache.estimated_bytes(), per_entry);
        cache.insert(5678, 2, make_refs(200.0, 0, 0));
        assert_eq!(cache.estimated_bytes(), per_entry * 2);
    }

    #[test]
    fn test_estimated_bytes_drops_to_zero_after_clear() {
        let cache = PrevDayCache::new();
        cache.insert(1234, 1, make_refs(100.0, 0, 0));
        assert!(cache.estimated_bytes() > 0);
        cache.clear();
        assert_eq!(cache.estimated_bytes(), 0);
    }

    #[test]
    fn test_concurrent_insert_does_not_lose_entries() {
        use std::thread;
        let cache = PrevDayCache::new();
        let mut handles = Vec::new();
        for thread_idx in 0..4 {
            let cache_clone = cache.clone();
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    cache_clone.insert(thread_idx, i as u8, make_refs(f64::from(i), 0, 0));
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }
        // 4 threads × 50 entries each = 200 entries (segment_code is the
        // tiebreaker so we don't collide on (thread_idx, ...)).
        assert_eq!(cache.len_instruments(), 4 * 50);
    }
}
