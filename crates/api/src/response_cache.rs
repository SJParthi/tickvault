//! Tiny in-process TTL response caches for the two public QuestDB-backed
//! endpoints (2026-07-09 audit directive — DoS/DB-load hardening).
//!
//! Two shapes, both COLD-path (HTTP API only, never the tick pipeline):
//! - [`SingleSlotTtlCache`] — `/api/stats` (one JSON body, 5s TTL).
//! - [`BoundedTtlCache`] — `/api/quote/{security_id}` (per-SID JSON body,
//!   1s TTL, hard entry cap; callers cache ONLY 200 bodies so
//!   attacker-chosen garbage security_ids can never grow the map).
//!
//! std `Mutex` with `PoisonError::into_inner` recovery — a mutex is correct
//! on this cold path, and there is zero `unwrap`/`expect` in prod code.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::response::{IntoResponse, Response};

/// Cache marker header so the operator can curl-debug hit/miss behaviour.
pub(crate) const CACHE_MARKER_HEADER: &str = "x-tv-cache";

/// Builds a 200 JSON response from a pre-serialized body with the
/// `x-tv-cache: hit|miss` marker header. Shared by the stats + quote
/// handlers so cached and computed responses are byte-identical.
pub(crate) fn cached_json_response(body: String, cache_marker: &'static str) -> Response {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            ),
            (
                axum::http::HeaderName::from_static(CACHE_MARKER_HEADER),
                axum::http::HeaderValue::from_static(cache_marker),
            ),
        ],
        body,
    )
        .into_response()
}

/// Recovers the guard from a poisoned lock. Worst case one stale/partial
/// slot from the panicking thread — strictly better than panicking the API
/// task or returning 500 for a cache problem.
fn lock_recovering<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// SingleSlotTtlCache — /api/stats
// ---------------------------------------------------------------------------

/// One cached response body with a TTL. Overwritten in place on every
/// store; memory is bounded at exactly one body.
pub struct SingleSlotTtlCache {
    slot: Mutex<Option<(Instant, String)>>,
    ttl: Duration,
}

impl SingleSlotTtlCache {
    /// Creates an empty cache with the given TTL.
    pub fn new(ttl: Duration) -> Self {
        Self {
            slot: Mutex::new(None),
            ttl,
        }
    }

    /// Returns the cached body when the slot is fresh (age < TTL).
    pub fn get(&self) -> Option<String> {
        let guard = lock_recovering(&self.slot);
        match guard.as_ref() {
            Some((stored_at, body)) if stored_at.elapsed() < self.ttl => Some(body.clone()),
            _ => None,
        }
    }

    /// Stores a body, replacing whatever was in the slot.
    pub fn put(&self, body: String) {
        let mut guard = lock_recovering(&self.slot);
        *guard = Some((Instant::now(), body));
    }
}

// ---------------------------------------------------------------------------
// BoundedTtlCache — /api/quote/{security_id}
// ---------------------------------------------------------------------------

/// Hard cap on cached quote entries.
///
/// **CORRECTED 2026-09-01 — this cap is a CONCURRENCY bound, not universe
/// headroom, and the sentence that stood here said the opposite.** It read:
/// *"The only-200 caching policy already bounds the key space to real
/// instruments (the ~250-1200 daily universe); this cap is defence-in-depth
/// against any future policy drift."* The subscribed universe is now
/// ~24,600 instruments (the 2026-08-15 full-mode authorization), so 2048 is
/// roughly **8% of it**, not headroom above it. A reader trusting the old
/// text would conclude the cap can never bind. It can.
///
/// The one-second TTL limits residency but does not prevent a burst from
/// reaching this cap. At saturation, the expiry index checks the oldest entry
/// and evicts at most one expired key; fresh entries are never displaced.
pub const QUOTE_CACHE_MAX_ENTRIES: usize = 2048;

/// Per-key TTL cache with a hard entry cap. At cap, NEW keys are
/// skip-inserted (served fresh, never cached) — existing keys keep being
/// overwritten in place, so the map can never exceed the cap and never
/// evict-thrashes under attacker probing.
///
/// Key = the quote endpoint's own key (`security_id` alone).
// APPROVED: single-key map is correct by construction per I-P1-11 rule 2 —
// the /api/quote/{security_id} endpoint itself is keyed on security_id
// alone (its SQL is `WHERE security_id = X LATEST ON ts PARTITION BY
// security_id` across ALL segments/feeds), so the cache key mirrors the
// full request identity; no cross-segment entry can be dropped because no
// segment ever enters the request.
pub struct BoundedTtlCache {
    state: Mutex<BoundedCacheState>,
    ttl: Duration,
    max_entries: usize,
}

/// The index has exactly one record per resident key, including overwrites.
/// Unlike an append-only expiry heap, repeated refreshes cannot grow metadata
/// beyond the cache cap. Timestamps are insertion times, ordered with key ties.
#[derive(Default)]
struct BoundedCacheState {
    map: HashMap<u64, (Instant, String)>,
    expiry_order: BTreeSet<(Instant, u64)>,
}

impl BoundedTtlCache {
    /// Creates an empty cache with the given TTL and entry cap.
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            state: Mutex::new(BoundedCacheState::default()),
            ttl,
            max_entries,
        }
    }

    /// Returns a fresh cached body; removes an expired key from both indexes.
    /// Expected O(1) lookup plus O(body bytes) copying; expired removal is
    /// O(log cap). Mutex waiting is not a constant-time guarantee.
    pub fn get(&self, key: u64) -> Option<String> {
        self.get_at(key, None)
    }

    fn get_at(&self, key: u64, now: Option<Instant>) -> Option<String> {
        let mut guard = lock_recovering(&self.state);
        // Read time after acquiring the lock so contention cannot serve stale data.
        let now = now.unwrap_or_else(Instant::now);
        match guard.map.get(&key) {
            Some((stored_at, body)) if now.saturating_duration_since(*stored_at) < self.ttl => {
                Some(body.clone())
            }
            Some(_) => {
                if let Some((stored_at, _)) = guard.map.remove(&key) {
                    guard.expiry_order.remove(&(stored_at, key));
                }
                None
            }
            None => None,
        }
    }

    /// Stores a body, refreshing existing keys even at cap. A new key at cap
    /// replaces only the oldest EXPIRED entry; if every entry is fresh, the
    /// insertion is skipped. A zero-capacity cache never admits a key.
    ///
    /// Each mutation performs at most one eviction and O(log cap) ordered
    /// index work plus expected hash-map work. It never scans the whole map.
    /// Expired entries not needed for admission remain until lookup or later
    /// admission, but can never be served. Metadata stays O(cap), including
    /// when one key is overwritten repeatedly. Hash growth/allocation and
    /// mutex contention still preclude a strict worst-case O(1) claim.
    pub fn put(&self, key: u64, body: String) {
        self.put_at(key, body, None);
    }

    fn put_at(&self, key: u64, body: String, now: Option<Instant>) {
        if self.max_entries == 0 {
            return;
        }
        let mut guard = lock_recovering(&self.state);
        // Start TTL when the write acquires the lock, not before a queueing delay.
        let now = now.unwrap_or_else(Instant::now);
        if let Some((stored_at, _)) = guard.map.get(&key) {
            let old_expiry = (*stored_at, key);
            guard.expiry_order.remove(&old_expiry);
        } else if guard.map.len() >= self.max_entries {
            let Some(&(oldest_at, oldest_key)) = guard.expiry_order.first() else {
                // Defensive: do not exceed the cap if state was poisoned.
                return;
            };
            if now.saturating_duration_since(oldest_at) < self.ttl {
                return;
            }
            guard.expiry_order.remove(&(oldest_at, oldest_key));
            guard.map.remove(&oldest_key);
        }
        guard.map.insert(key, (now, body));
        guard.expiry_order.insert((now, key));
    }

    /// Resident entry count, including entries awaiting lazy expiration.
    pub fn len(&self) -> usize {
        lock_recovering(&self.state).map.len()
    }

    /// Whether the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_slot_put_get_roundtrip() {
        let cache = SingleSlotTtlCache::new(Duration::from_secs(5));
        assert!(cache.get().is_none(), "empty cache misses");
        cache.put("{\"a\":1}".to_string());
        assert_eq!(cache.get().as_deref(), Some("{\"a\":1}"));
    }

    #[test]
    fn test_single_slot_overwrite_replaces_body() {
        let cache = SingleSlotTtlCache::new(Duration::from_secs(5));
        cache.put("first".to_string());
        cache.put("second".to_string());
        assert_eq!(cache.get().as_deref(), Some("second"));
    }

    #[test]
    fn test_single_slot_ttl_expiry() {
        // 300ms TTL (not 10ms): under llvm-cov instrumentation a >10ms
        // preemption between put and the fresh-hit assert would flake.
        let cache = SingleSlotTtlCache::new(Duration::from_millis(300));
        cache.put("stale-soon".to_string());
        assert!(cache.get().is_some(), "fresh entry hits");
        std::thread::sleep(Duration::from_millis(400));
        assert!(cache.get().is_none(), "expired entry misses");
    }

    #[test]
    fn test_bounded_cache_put_get_roundtrip() {
        let cache = BoundedTtlCache::new(Duration::from_secs(1), 4);
        assert!(cache.get(13).is_none());
        cache.put(13, "nifty".to_string());
        assert_eq!(cache.get(13).as_deref(), Some("nifty"));
        assert!(cache.get(25).is_none(), "other key still misses");
    }

    #[test]
    fn test_bounded_cache_ttl_expiry_and_lazy_eviction() {
        let cache = BoundedTtlCache::new(Duration::from_millis(10), 4);
        cache.put(13, "nifty".to_string());
        std::thread::sleep(Duration::from_millis(25));
        assert!(cache.get(13).is_none(), "expired entry misses");
        assert!(
            cache.is_empty(),
            "expired entry is lazily evicted on lookup"
        );
    }

    /// Adversarial-review 2026-07-09 fix: at cap, EXPIRED dead keys are
    /// swept so daily SID churn can never permanently self-disable the
    /// cache for new keys.
    #[test]
    fn test_bounded_cache_at_cap_sweeps_expired_then_inserts() {
        let cache = BoundedTtlCache::new(Duration::from_millis(10), 2);
        cache.put(1, "a".to_string());
        cache.put(2, "b".to_string());
        std::thread::sleep(Duration::from_millis(25));
        // Both entries are dead. A NEW key at cap must sweep them and
        // insert instead of being skip-inserted forever.
        cache.put(3, "c".to_string());
        assert_eq!(
            cache.get(3).as_deref(),
            Some("c"),
            "new key must be cached after the expired sweep frees space"
        );
        assert!(cache.len() <= 2, "map never exceeds the cap");
    }

    #[test]
    fn test_bounded_cache_cap_skip_insert_for_new_keys() {
        let cache = BoundedTtlCache::new(Duration::from_secs(5), 2);
        cache.put(1, "a".to_string());
        cache.put(2, "b".to_string());
        // At cap: a NEW key is skip-inserted...
        cache.put(3, "c".to_string());
        assert!(cache.get(3).is_none(), "new key at cap must not be cached");
        assert_eq!(cache.len(), 2, "map never exceeds the cap");
        // ...and the existing entries are untouched (no evict-thrash).
        assert_eq!(cache.get(1).as_deref(), Some("a"));
        assert_eq!(cache.get(2).as_deref(), Some("b"));
    }

    #[test]
    fn test_bounded_cache_only_present_key_overwrites_at_cap() {
        let cache = BoundedTtlCache::new(Duration::from_secs(5), 2);
        cache.put(1, "a".to_string());
        cache.put(2, "b".to_string());
        // Existing key overwrites in place even at cap.
        cache.put(2, "b2".to_string());
        assert_eq!(cache.get(2).as_deref(), Some("b2"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn saturated_cache_preserves_fresh_entries_under_new_key_burst() {
        let at = Instant::now();
        let cache = BoundedTtlCache::new(Duration::from_secs(10), 2);
        cache.put_at(1, "a".into(), Some(at));
        cache.put_at(2, "b".into(), Some(at));
        for key in 3..10_000 {
            cache.put_at(key, "uncached".into(), Some(at));
        }
        assert_eq!(cache.get_at(1, Some(at)).as_deref(), Some("a"));
        assert_eq!(cache.get_at(2, Some(at)).as_deref(), Some("b"));
        assert_eq!(cache.len(), 2);
        assert_eq!(lock_recovering(&cache.state).expiry_order.len(), 2);
    }

    #[test]
    fn refreshed_oldest_key_survives_other_keys_expiration() {
        let at = Instant::now();
        let ttl = Duration::from_secs(10);
        let cache = BoundedTtlCache::new(ttl, 2);
        cache.put_at(1, "old".into(), Some(at));
        cache.put_at(2, "expires".into(), Some(at));
        let refreshed = at + Duration::from_secs(5);
        cache.put_at(1, "fresh".into(), Some(refreshed));
        // At the exact TTL boundary key 2 expires; refreshed key 1 does not.
        cache.put_at(3, "new".into(), Some(at + ttl));
        assert_eq!(cache.get_at(1, Some(at + ttl)).as_deref(), Some("fresh"));
        assert!(cache.get_at(2, Some(at + ttl)).is_none());
        assert_eq!(cache.get_at(3, Some(at + ttl)).as_deref(), Some("new"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn overwrite_and_lazy_eviction_keep_expiry_metadata_bounded() {
        let at = Instant::now();
        let ttl = Duration::from_secs(10);
        let cache = BoundedTtlCache::new(ttl, 1);
        for offset in 0..10_000 {
            cache.put_at(1, "refresh".into(), Some(at + Duration::from_nanos(offset)));
        }
        assert_eq!(lock_recovering(&cache.state).expiry_order.len(), 1);
        assert!(
            cache
                .get_at(1, Some(at + ttl + Duration::from_secs(1)))
                .is_none()
        );
        assert_eq!(lock_recovering(&cache.state).expiry_order.len(), 0);
        cache.put_at(
            2,
            "replacement".into(),
            Some(at + ttl + Duration::from_secs(1)),
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn zero_capacity_and_zero_ttl_never_serve_a_body() {
        let at = Instant::now();
        let disabled = BoundedTtlCache::new(Duration::from_secs(10), 0);
        disabled.put_at(1, "a".into(), Some(at));
        assert!(disabled.is_empty());
        assert!(lock_recovering(&disabled.state).expiry_order.is_empty());
        let expired = BoundedTtlCache::new(Duration::ZERO, 1);
        expired.put_at(1, "a".into(), Some(at));
        expired.put_at(2, "b".into(), Some(at));
        assert!(expired.get_at(2, Some(at)).is_none());
        assert!(expired.is_empty());
    }

    #[test]
    fn test_poisoned_lock_recovers() {
        use std::sync::Arc;
        // 3600s TTL: sanitizer builds (-Z build-std) stall ~22s symbolizing the
        // poisoner's panic backtrace, which blew a 5s TTL (safety run
        // 29230855037). TTL expiry is NOT what this test verifies — that is
        // pinned by test_single_slot_ttl_expiry with its own short local TTL.
        let cache = Arc::new(SingleSlotTtlCache::new(Duration::from_secs(3600)));
        cache.put("pre-poison".to_string());
        let poisoner = Arc::clone(&cache);
        // Panic while holding the lock to poison it.
        let handle = std::thread::spawn(move || {
            let _guard = poisoner.slot.lock();
            panic!("poison the cache lock (test)");
        });
        assert!(handle.join().is_err(), "poisoner thread must panic");
        // The cache still works — no panic, no 500-class failure.
        assert_eq!(cache.get().as_deref(), Some("pre-poison"));
        cache.put("post-poison".to_string());
        assert_eq!(cache.get().as_deref(), Some("post-poison"));
    }

    #[test]
    fn test_cached_json_response_headers_and_body() {
        let response = cached_json_response("{\"a\":1}".to_string(), "hit");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json"),
        );
        assert_eq!(
            response
                .headers()
                .get(CACHE_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("hit"),
        );
    }

    #[test]
    fn test_quote_cache_cap_constant_is_bounded() {
        // Memory envelope pin: cap x ~300B body ≈ well under 1 MiB.
        // CORRECTED 2026-09-01: this floor used to say "must hold the daily
        // universe", which stopped being true when the universe went to
        // ~24,600. The cap is a per-second CONCURRENCY bound, so the floor
        // is now justified as such — 1,200 concurrent distinct quotes inside
        // one second is already far above anything an operator surface does.
        assert!(
            QUOTE_CACHE_MAX_ENTRIES >= 1200,
            "must absorb a realistic burst of distinct quotes within one TTL"
        );
        assert!(
            QUOTE_CACHE_MAX_ENTRIES <= 10_000,
            "must stay memory-bounded"
        );
    }
}
