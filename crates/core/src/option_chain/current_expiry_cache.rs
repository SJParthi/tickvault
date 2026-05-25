//! Per-day cache of the current (nearest) expiry date for each
//! option-chain underlying.
//!
//! ## Why this exists
//!
//! Before 2026-05-25, the option-chain minute scheduler made TWO Dhan
//! REST calls per slot per underlying:
//!   1. `POST /v2/optionchain/expirylist` → list of all future expiries
//!   2. `POST /v2/optionchain` → chain for `data[0]` (nearest expiry)
//!
//! Per Dhan's docs (operator-confirmed 2026-05-25):
//!
//! > "data[] array — All expiry dates of underlying in YYYY-MM-DD.
//! > Dhan only returns ACTIVE expiries (future-dated); the first
//! > element is ALWAYS the nearest/current expiry."
//!
//! So `data[0]` is stable within a trading day. We can fetch the
//! expiry list ONCE per underlying at 09:00:30 IST (pre-market) and
//! reuse `data[0]` for every minute snapshot until the next trading
//! day's 09:00:30 IST refresh.
//!
//! This halves Dhan REST traffic: 3 calls per minute instead of 6.
//!
//! ## Crash recovery
//!
//! On boot, the cache is empty. The boot orchestrator
//! (`crates/app/src/option_chain_cache_loader.rs`) REHYDRATES the
//! current expiry per underlying from the latest QuestDB
//! `option_chain_minute_snapshot` row. If QuestDB is empty (fresh
//! deploy), the warmup task at 09:00:30 IST populates the cache
//! before the first minute-snapshot fires at 09:15:50 IST.
//!
//! ## Authority
//!
//! - operator-charter-forever.md §C "100% audit coverage" — every
//!   cache mutation writes to `option_chain_minute_snapshot` (audit
//!   trail intact via the existing scheduler).
//! - data-integrity.md "Idempotency" — same `(underlying, expiry)`
//!   call repeated within a day returns the same result.
//! - hot-path.md "papaya for concurrent hot-path lookups" — strategy
//!   reads are O(1) via papaya HashMap.

use std::sync::Arc;

use papaya::HashMap;

/// Composite key: (security_id, exchange_segment_code). Mirrors the
/// I-P1-11 composite-key rule even though option chain underlyings
/// today are all IDX_I (code 0) — future-proofs against any
/// hypothetical NSE_EQ underlying being added to the snapshot
/// schedule.
type CurrentExpiryKey = (u32, u8);

/// In-RAM cache of the current (nearest) expiry date for each
/// option-chain underlying.
///
/// Cloneable: `Arc::clone` on the inner papaya map. The scheduler
/// holds one clone; the boot rehydrator holds another; both write
/// through the same map.
///
/// # Performance
///
/// `get` totals ~30 ns (papaya lookup + Arc::clone of the cached
/// `String`). `insert` ~50 ns. Both well below any per-minute budget.
#[derive(Clone, Default)]
pub struct CurrentExpiryCache {
    inner: Arc<HashMap<CurrentExpiryKey, Arc<str>>>,
}

impl CurrentExpiryCache {
    /// Constructs an empty cache. The boot rehydrator + 09:00:30 IST
    /// warmup populate it before the first minute-snapshot fires.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores the current (nearest) expiry for a given underlying.
    ///
    /// The expiry string is stored as `Arc<str>` so subsequent `get`
    /// calls share the allocation — important because the scheduler
    /// reads this every minute per underlying.
    ///
    /// # Performance
    ///
    /// ~50 ns per call. Called at most 3 times at 09:00:30 IST, plus
    /// 3 times on Thursday expiry rollover. Not a hot path.
    pub fn insert(&self, security_id: u32, exchange_segment_code: u8, expiry: &str) {
        let pin = self.inner.pin();
        pin.insert((security_id, exchange_segment_code), Arc::from(expiry));
    }

    /// Looks up the current expiry for an underlying. Returns `None`
    /// if the cache has not yet been populated (boot race, or an
    /// underlying that was never warmed up).
    ///
    /// # Performance
    ///
    /// ~30 ns. Called once per underlying per minute by the
    /// scheduler — well within the per-minute budget.
    #[must_use]
    pub fn get(&self, security_id: u32, exchange_segment_code: u8) -> Option<Arc<str>> {
        let pin = self.inner.pin();
        pin.get(&(security_id, exchange_segment_code))
            .map(Arc::clone)
    }

    /// Number of cached entries. Used by the boot health check + the
    /// `tv_option_chain_current_expiry_cache_entries` gauge.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.pin().len()
    }

    /// True iff the cache has zero entries — i.e. neither the boot
    /// rehydrator nor the 09:00:30 IST warmup has run successfully.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NIFTY: u32 = 13;
    const BANKNIFTY: u32 = 25;
    const SENSEX: u32 = 51;

    #[test]
    fn test_len_reports_count_of_distinct_keys() {
        let cache = CurrentExpiryCache::new();
        assert_eq!(cache.len(), 0);
        cache.insert(NIFTY, 0, "2026-05-26");
        assert_eq!(cache.len(), 1);
        cache.insert(BANKNIFTY, 0, "2026-05-26");
        assert_eq!(cache.len(), 2);
        cache.insert(SENSEX, 0, "2026-05-27");
        assert_eq!(cache.len(), 3);
        // Same key overwrite — len unchanged.
        cache.insert(NIFTY, 0, "2026-06-02");
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn test_is_empty_after_insert_returns_false() {
        let cache = CurrentExpiryCache::new();
        assert!(cache.is_empty());
        cache.insert(NIFTY, 0, "2026-05-26");
        assert!(!cache.is_empty());
    }

    #[test]
    fn test_insert_via_canonical_helper_inserts_one_entry() {
        // Direct test for insert (separate from the roundtrip test)
        // so the pub-fn-test-guard sees a `test_insert_*` exactly.
        let cache = CurrentExpiryCache::new();
        cache.insert(NIFTY, 0, "2026-05-26");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_get_returns_arc_str_clone() {
        // Direct test for get (separate from the roundtrip test) so
        // the pub-fn-test-guard sees a `test_get_*` exactly.
        let cache = CurrentExpiryCache::new();
        cache.insert(NIFTY, 0, "2026-05-26");
        let arc1 = cache.get(NIFTY, 0).expect("inserted");
        let arc2 = cache.get(NIFTY, 0).expect("inserted");
        // Same string content; Arc::clone may or may not point to
        // the same allocation depending on papaya internals — content
        // is what matters.
        assert_eq!(&*arc1, "2026-05-26");
        assert_eq!(&*arc2, "2026-05-26");
    }
    const IDX_I: u8 = 0;

    #[test]
    fn test_new_cache_is_empty() {
        let cache = CurrentExpiryCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_insert_and_get_roundtrip() {
        let cache = CurrentExpiryCache::new();
        cache.insert(NIFTY, IDX_I, "2026-05-26");
        let got = cache.get(NIFTY, IDX_I).expect("inserted entry present");
        assert_eq!(&*got, "2026-05-26");
    }

    #[test]
    fn test_get_returns_none_for_missing_underlying() {
        let cache = CurrentExpiryCache::new();
        cache.insert(NIFTY, IDX_I, "2026-05-26");
        // Different SID — not cached.
        assert!(cache.get(BANKNIFTY, IDX_I).is_none());
        // Same SID but different segment — not cached.
        assert!(cache.get(NIFTY, 1).is_none());
    }

    #[test]
    fn test_insert_overwrites_previous_expiry() {
        // Thursday rollover: NIFTY's current expiry shifts from one
        // Thursday to the next. The next 09:00:30 IST warmup call
        // overwrites the cached value.
        let cache = CurrentExpiryCache::new();
        cache.insert(NIFTY, IDX_I, "2026-05-26");
        cache.insert(NIFTY, IDX_I, "2026-06-02");
        let got = cache.get(NIFTY, IDX_I).expect("present");
        assert_eq!(&*got, "2026-06-02");
        assert_eq!(cache.len(), 1, "overwrite must not grow the cache");
    }

    #[test]
    fn test_three_underlyings_independent() {
        let cache = CurrentExpiryCache::new();
        cache.insert(NIFTY, IDX_I, "2026-05-26");
        cache.insert(BANKNIFTY, IDX_I, "2026-05-26");
        cache.insert(SENSEX, IDX_I, "2026-05-27");
        assert_eq!(cache.len(), 3);
        assert_eq!(&*cache.get(NIFTY, IDX_I).unwrap(), "2026-05-26");
        assert_eq!(&*cache.get(BANKNIFTY, IDX_I).unwrap(), "2026-05-26");
        assert_eq!(&*cache.get(SENSEX, IDX_I).unwrap(), "2026-05-27");
    }

    #[test]
    fn test_cache_clone_shares_state() {
        // The scheduler + the rehydrator + the warmup task all hold
        // clones of the same Arc. Inserts via one clone must be
        // visible through every other clone.
        let cache_a = CurrentExpiryCache::new();
        let cache_b = cache_a.clone();
        cache_a.insert(NIFTY, IDX_I, "2026-05-26");
        let got_via_b = cache_b.get(NIFTY, IDX_I).expect("visible through clone");
        assert_eq!(&*got_via_b, "2026-05-26");
    }
}
