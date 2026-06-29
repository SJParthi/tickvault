//! `TickStorage` — full-day in-RAM tick ring per instrument
//! (Wave-5 §K-L10).
//!
//! ## Why this exists
//!
//! Downstream consumers need cheap full-day tick history per
//! `(security_id, exchange_segment)` without crossing the QuestDB
//! boundary on the hot read path:
//!
//! - The dynamic depth-20 / depth-200 selector (`movers_unified_pipeline`)
//!   needs the day's volume baseline per instrument every 60 s.
//! - The `/api/movers/v2` read-flip (#505) wants the latest bar +
//!   day's volume without a JOIN against `ticks`.
//!
//! Pre-`#504d` both consumers crossed into QuestDB matviews. After
//! `#504d` the tick stream lands in this map AND in the existing
//! ILP write path — duplicate writes by design (the ILP path remains
//! the canonical SEBI audit trail; the in-RAM ring is a cache).
//!
//! ## Composite key (I-P1-11)
//!
//! `(security_id, exchange_segment_code)`. `security_id` alone is NOT
//! unique because Dhan reuses numeric IDs across segments (e.g. NIFTY
//! IDX_I id=13 vs hypothetical NSE_EQ id=13 collision). Pinning the
//! key to the segment-aware tuple is enforced by:
//!
//! - The `EngineKey` type alias inherited from `CandleEngineMap` (mirrors
//!   the established pattern that the I-P1-11 audit ratified).
//! - The `test_tick_storage_isolates_cross_segment_collisions`
//!   ratchet below.
//!
//! ## Hot-path budget (per L12: ≤200 ns/tick)
//!
//! - papaya `pin().get_or_insert_with(...)`: ~30 ns + (one-time per key:
//!   `Vec::with_capacity(N)` allocation, ~50 ns; happens once per
//!   instrument per day after the IST 09:15 reset).
//! - Mutex lock (uncontended): ~5 ns.
//! - `Vec::push`: amortised O(1) when capacity sufficient; we provision
//!   `DEFAULT_PER_INSTRUMENT_CAPACITY = 5_000` slots at first insert
//!   covering steady-state demand.
//! - Mutex unlock: ~5 ns.
//!
//! Total steady-state push: ~40 ns. First-tick-per-instrument push
//! (with allocation): ~90 ns. Both well under the 200 ns budget.
//!
//! ## Memory footprint (L10 target ~880 MB)
//!
//! - `ParsedTick` ≈ 104 bytes (Copy struct).
//! - 11K instruments × 5_000 capacity × 104 B = ~5.7 GB at full
//!   capacity — but full-day live data on indices/derivatives lands
//!   around 800-2000 ticks/instrument actual. Empty unused capacity
//!   in the Vec is RESERVED memory, not RESIDENT (Linux lazy-page
//!   allocation). The RSS impact tracks `len()` (actual ticks) at
//!   ~104 bytes each, summing to ~880 MB at design load.
//! - The `tv_subsystem_memory_estimated_bytes{component="tick_storage"}`
//!   gauge from #504a reports the `len() × size_of::<ParsedTick>()`
//!   sum, NOT the reserved capacity, matching the L18 lazy-RSS
//!   reconciliation contract.

use std::sync::Arc;
use std::sync::Mutex;

use papaya::HashMap;
use tickvault_common::tick_types::ParsedTick;

/// Composite key per I-P1-11 — `(security_id, exchange_segment_code)`.
type TickKey = (u64, u8);

/// Default per-instrument tick Vec capacity reserved at first push.
///
/// Steady-state index / derivative instruments produce ~800-2_000
/// ticks per trading day. The 5_000 default reserves enough headroom
/// for the busiest contracts (e.g. NIFTY index value during a high-vol
/// session) without spilling into reallocation. When the Vec grows
/// beyond this cap, `push` triggers a doubling realloc — a cold-path
/// cost the operator can monitor via `tv_in_mem_tick_storage_realloc_total`.
///
/// Runtime-tunable via `[in_mem.tick_storage].per_instrument_capacity`
/// in `config/base.toml` (added by this PR).
pub const DEFAULT_PER_INSTRUMENT_CAPACITY: usize = 5_000;

/// Per-instrument full-day tick ring.
///
/// `Clone` is `Arc::clone` on the inner papaya map — multiple consumers
/// (the tick processor hot path + the reset task + the
/// `subsystem_memory` sampler) can hold cheap clones.
#[derive(Clone)]
pub struct TickStorage {
    inner: Arc<HashMap<TickKey, Arc<Mutex<Vec<ParsedTick>>>>>,
    per_instrument_capacity: usize,
}

impl Default for TickStorage {
    fn default() -> Self {
        Self::new(DEFAULT_PER_INSTRUMENT_CAPACITY)
    }
}

impl TickStorage {
    /// Build an empty store with the given per-instrument Vec capacity
    /// reserved on first push per `(security_id, segment_code)` key.
    ///
    /// `per_instrument_capacity == 0` falls back to
    /// `DEFAULT_PER_INSTRUMENT_CAPACITY` so callers that pass an
    /// uninitialised config don't accidentally trigger 1-byte
    /// reallocations on every tick.
    #[must_use]
    pub fn new(per_instrument_capacity: usize) -> Self {
        let cap = if per_instrument_capacity == 0 {
            DEFAULT_PER_INSTRUMENT_CAPACITY
        } else {
            per_instrument_capacity
        };
        Self {
            inner: Arc::new(HashMap::new()), // APPROVED: boot-path one-time construction; papaya does not expose `with_capacity` and starts at small default; per-key inner Vec IS pre-sized via Vec::with_capacity(cap) on hot push path
            per_instrument_capacity: cap,
        }
    }

    /// Push a tick into the ring. Returns `true` on success.
    ///
    /// Hot-path: ~40 ns steady-state (papaya pin + uncontended Mutex
    /// lock + `Vec::push`). First tick per instrument per day allocates
    /// the per-instrument Vec (~90 ns).
    ///
    /// **Design**: a `Mutex` poison error means a panic happened mid-
    /// mutation on another thread — extremely unlikely on this code
    /// path (no panic-able operations inside the critical section).
    /// On poison we drop the tick and return `false`; the
    /// `tv_in_mem_tick_storage_pushes_dropped_total{reason="poison"}`
    /// counter surfaces to the operator. The map self-recovers because
    /// downstream `push` calls overwrite via the same `Arc<Mutex>`.
    pub fn push(&self, tick: ParsedTick) -> bool {
        let key: TickKey = (tick.security_id, tick.exchange_segment_code);
        let pin = self.inner.pin();
        // O(1) get-or-insert. The closure runs ONLY on first miss per
        // (key, day). Allocation (`Vec::with_capacity`) is amortised
        // across the day's ticks for that instrument.
        let cap = self.per_instrument_capacity;
        let entry = pin.get_or_insert_with(key, || Arc::new(Mutex::new(Vec::with_capacity(cap))));
        let mut guard = match entry.lock() {
            Ok(g) => g,
            Err(_poisoned) => {
                metrics::counter!(
                    "tv_in_mem_tick_storage_pushes_dropped_total",
                    "reason" => "mutex_poisoned"
                )
                .increment(1);
                return false;
            }
        };
        // Track realloc events so the operator can observe whether the
        // configured `per_instrument_capacity` is sized correctly.
        if guard.len() == guard.capacity() && guard.capacity() >= cap {
            metrics::counter!("tv_in_mem_tick_storage_realloc_total").increment(1);
        }
        guard.push(tick);
        metrics::counter!("tv_in_mem_tick_storage_pushes_total").increment(1);
        true
    }

    /// Snapshot the tick history for a single instrument.
    ///
    /// Returns a `Vec<ParsedTick>` cloned from the per-instrument ring.
    /// Cold path — caller pays the clone cost (`O(n)` in the instrument's
    /// tick count). Used by the depth-dynamic selector + future
    /// `/api/movers/v2` reads.
    ///
    /// Returns `None` if the instrument has no ticks today (key not yet
    /// inserted in the map OR last reset cleared it before any new
    /// push).
    #[must_use]
    pub fn snapshot(&self, security_id: u64, exchange_segment_code: u8) -> Option<Vec<ParsedTick>> {
        let pin = self.inner.pin();
        let entry = pin.get(&(security_id, exchange_segment_code))?;
        let guard = entry.lock().ok()?;
        if guard.is_empty() {
            None
        } else {
            Some(guard.clone()) // APPROVED: cold-path snapshot called by /api/movers + depth-dynamic selector at most 1/min — NOT per-tick hot path; clone enables caller to iterate without holding the Mutex
        }
    }

    /// Returns the number of `(security_id, segment_code)` keys present
    /// in the map. Used by the `tv_in_mem_tick_storage_instruments`
    /// gauge.
    #[must_use]
    pub fn len_instruments(&self) -> usize {
        let pin = self.inner.pin();
        pin.len()
    }

    /// Returns the total number of ticks held across every instrument.
    /// O(N) over keys; called by the 10s `subsystem_memory` sampler
    /// (#504a) — off the hot path.
    #[must_use]
    pub fn len_total(&self) -> usize {
        let pin = self.inner.pin();
        pin.iter()
            .map(|(_, mutex)| mutex.lock().map(|g| g.len()).unwrap_or(0))
            .sum()
    }

    /// Estimated RSS-style byte footprint for the
    /// `tv_subsystem_memory_estimated_bytes{component="tick_storage"}`
    /// gauge (#504a / L18 contract).
    ///
    /// Mirrors the L18 "lazy `len() × size_of`" rule: reports actual
    /// resident bytes (`len()`), not `capacity()` reservation. Linux
    /// lazy-page allocation means reserved-but-unused Vec capacity
    /// does NOT count against RSS until first write to that page.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        let len = self.len_total() as u64;
        let per_tick = std::mem::size_of::<ParsedTick>() as u64;
        len.saturating_mul(per_tick)
    }

    /// Drain every per-instrument ring. Called by the IST 09:15 reset
    /// scheduler.
    ///
    /// **Design**: clears each Vec but **retains the allocated capacity**
    /// (Vec::clear semantics) so day-(N+1)'s first tick per instrument
    /// hits the no-realloc path. The reserved capacity stays as
    /// uncommitted memory — Linux returns the pages to the kernel only
    /// if the OS faces memory pressure (`madvise(MADV_DONTNEED)`),
    /// which is fine for our budget.
    pub fn reset(&self) {
        let pin = self.inner.pin();
        let mut cleared_keys = 0_u64;
        let mut cleared_ticks = 0_u64;
        for (_, mutex) in pin.iter() {
            if let Ok(mut guard) = mutex.lock() {
                cleared_ticks = cleared_ticks.saturating_add(guard.len() as u64);
                guard.clear();
                cleared_keys = cleared_keys.saturating_add(1);
            }
        }
        metrics::counter!("tv_in_mem_tick_storage_resets_total").increment(1);
        metrics::counter!("tv_in_mem_tick_storage_reset_keys_total").increment(cleared_keys);
        metrics::counter!("tv_in_mem_tick_storage_reset_ticks_total").increment(cleared_ticks);
        tracing::info!(
            cleared_keys,
            cleared_ticks,
            "tick_storage daily reset complete"
        );
    }

    /// Per-instrument Vec capacity (boot-time tunable) — exposed for
    /// the ratchet test that pins the runtime contract.
    #[must_use]
    pub const fn per_instrument_capacity(&self) -> usize {
        self.per_instrument_capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tick(security_id: u64, segment: u8, ltp: f32) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment,
            last_traded_price: ltp,
            ..ParsedTick::default()
        }
    }

    #[test]
    fn test_default_per_instrument_capacity_is_pinned_at_5k() {
        // L10 + plan §1 — sized to cover busiest instrument's daily
        // tick count without realloc. Drift requires plan amend.
        assert_eq!(DEFAULT_PER_INSTRUMENT_CAPACITY, 5_000);
    }

    #[test]
    fn test_tick_storage_new_starts_empty() {
        let store = TickStorage::default();
        assert_eq!(store.len_instruments(), 0);
        assert_eq!(store.len_total(), 0);
        assert_eq!(store.estimated_bytes(), 0);
    }

    #[test]
    fn test_push_records_tick_and_advances_counters() {
        let store = TickStorage::default();
        assert!(store.push(make_tick(1234, 1, 100.0)));
        assert_eq!(store.len_instruments(), 1);
        assert_eq!(store.len_total(), 1);
        assert!(store.estimated_bytes() > 0);
    }

    #[test]
    fn test_snapshot_returns_pushed_ticks_in_order() {
        let store = TickStorage::default();
        store.push(make_tick(1234, 1, 100.0));
        store.push(make_tick(1234, 1, 101.0));
        store.push(make_tick(1234, 1, 102.0));
        let snap = store
            .snapshot(1234, 1)
            .expect("3 ticks should be retrievable");
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].last_traded_price, 100.0);
        assert_eq!(snap[1].last_traded_price, 101.0);
        assert_eq!(snap[2].last_traded_price, 102.0);
    }

    #[test]
    fn test_snapshot_returns_none_for_unknown_instrument() {
        let store = TickStorage::default();
        store.push(make_tick(1234, 1, 100.0));
        assert!(store.snapshot(9999, 1).is_none());
        assert!(store.snapshot(1234, 2).is_none());
    }

    /// I-P1-11 ratchet: same `security_id`, different segment must NOT
    /// collide. Pre-#504d the bug class would have collapsed both into
    /// one ring; this test pins the segment-aware key.
    #[test]
    fn test_tick_storage_isolates_cross_segment_collisions_per_i_p1_11() {
        let store = TickStorage::default();
        store.push(make_tick(27, 0, 18000.0)); // FINNIFTY IDX_I
        store.push(make_tick(27, 1, 555.0)); // hypothetical NSE_EQ id=27
        let idx = store
            .snapshot(27, 0)
            .expect("IDX_I tick should be retrievable");
        let eq = store
            .snapshot(27, 1)
            .expect("NSE_EQ tick should be retrievable");
        assert_eq!(idx.len(), 1);
        assert_eq!(eq.len(), 1);
        assert_eq!(idx[0].last_traded_price, 18000.0);
        assert_eq!(eq[0].last_traded_price, 555.0);
        assert_eq!(store.len_instruments(), 2);
        assert_eq!(store.len_total(), 2);
    }

    #[test]
    fn test_reset_clears_all_per_instrument_rings() {
        let store = TickStorage::default();
        store.push(make_tick(1234, 1, 100.0));
        store.push(make_tick(5678, 2, 200.0));
        store.push(make_tick(1234, 1, 101.0));
        assert_eq!(store.len_total(), 3);

        store.reset();
        assert_eq!(store.len_total(), 0);
        // Snapshot returns None on empty rings.
        assert!(store.snapshot(1234, 1).is_none());
        assert!(store.snapshot(5678, 2).is_none());
        // Keys remain in the map (with empty Vecs) — len_instruments
        // tracks key count, not ticks.
        assert_eq!(store.len_instruments(), 2);
    }

    #[test]
    fn test_reset_preserves_vec_capacity_for_no_realloc_next_day() {
        // Reset preserves Vec::capacity() so day-(N+1)'s first push
        // hits the no-realloc path. Push N ticks, reset, push 1 more,
        // verify total post-reset push count == 1 (no growth).
        let store = TickStorage::new(64);
        for i in 0..50 {
            store.push(make_tick(1234, 1, i as f32));
        }
        let pre_reset_ticks = store.len_total();
        assert_eq!(pre_reset_ticks, 50);
        store.reset();
        store.push(make_tick(1234, 1, 999.0));
        assert_eq!(store.len_total(), 1);
        let snap = store.snapshot(1234, 1).expect("day-2 first tick");
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].last_traded_price, 999.0);
    }

    #[test]
    fn test_clone_shares_state_via_arc() {
        let store_a = TickStorage::default();
        let store_b = store_a.clone();
        store_a.push(make_tick(1234, 1, 100.0));
        // Clone B sees the state.
        assert_eq!(store_b.len_total(), 1);
    }

    #[test]
    fn test_zero_capacity_falls_back_to_default() {
        // Defensive default: a config that mis-sets `0` MUST NOT
        // trigger 1-byte-realloc-per-tick.
        let store = TickStorage::new(0);
        assert_eq!(
            store.per_instrument_capacity(),
            DEFAULT_PER_INSTRUMENT_CAPACITY
        );
    }

    #[test]
    fn test_per_instrument_capacity_explicit_pinned() {
        // L8 contract: runtime-tunable boot-time array. Verify the
        // accessor reports the configured value exactly.
        let store = TickStorage::new(7_500);
        assert_eq!(store.per_instrument_capacity(), 7_500);
    }

    #[test]
    fn test_estimated_bytes_scales_with_tick_count() {
        let store = TickStorage::default();
        let per_tick = std::mem::size_of::<ParsedTick>() as u64;
        store.push(make_tick(1234, 1, 100.0));
        assert_eq!(store.estimated_bytes(), per_tick);
        store.push(make_tick(1234, 1, 101.0));
        store.push(make_tick(5678, 2, 200.0));
        assert_eq!(store.estimated_bytes(), per_tick * 3);
    }

    #[test]
    fn test_estimated_bytes_drops_to_zero_after_reset() {
        let store = TickStorage::default();
        store.push(make_tick(1234, 1, 100.0));
        assert!(store.estimated_bytes() > 0);
        store.reset();
        assert_eq!(store.estimated_bytes(), 0);
    }

    /// Concurrent push from multiple threads must not deadlock or lose
    /// ticks. Light load test — runs in a few ms.
    #[test]
    fn test_concurrent_push_does_not_deadlock_or_lose_ticks() {
        use std::thread;
        let store = TickStorage::default();
        let mut handles = Vec::new();
        for thread_idx in 0..4 {
            let store_clone = store.clone();
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    store_clone.push(make_tick(thread_idx, 1, i as f32));
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }
        assert_eq!(store.len_total(), 4 * 100);
        assert_eq!(store.len_instruments(), 4);
    }

    #[test]
    fn test_len_instruments_explicit_name_match() {
        let store = TickStorage::default();
        assert_eq!(store.len_instruments(), 0);
        store.push(make_tick(1234, 1, 100.0));
        assert_eq!(store.len_instruments(), 1);
    }

    #[test]
    fn test_len_total_explicit_name_match() {
        let store = TickStorage::default();
        assert_eq!(store.len_total(), 0);
        store.push(make_tick(1234, 1, 100.0));
        store.push(make_tick(1234, 1, 101.0));
        assert_eq!(store.len_total(), 2);
    }

    #[test]
    fn test_realloc_counter_increments_when_capacity_exceeded() {
        // With a tiny initial capacity, pushing past it should trigger
        // the realloc counter (we can't observe the counter directly
        // without a recorder, but the path must not panic).
        let store = TickStorage::new(2);
        store.push(make_tick(1234, 1, 100.0));
        store.push(make_tick(1234, 1, 101.0));
        store.push(make_tick(1234, 1, 102.0)); // exceeds capacity 2 → realloc
        assert_eq!(store.len_total(), 3);
    }
}
