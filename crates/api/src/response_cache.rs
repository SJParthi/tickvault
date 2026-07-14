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

use std::collections::HashMap;
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

/// Hard cap on cached quote entries. The only-200 caching policy already
/// bounds the key space to real instruments (the ~250-1200 daily universe);
/// this cap is defence-in-depth against any future policy drift.
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
    map: Mutex<HashMap<u64, (Instant, String)>>,
    ttl: Duration,
    max_entries: usize,
}

impl BoundedTtlCache {
    /// Creates an empty cache with the given TTL and entry cap.
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            ttl,
            max_entries,
        }
    }

    /// Returns the cached body for `key` when fresh. Lazily evicts an
    /// expired entry for that key so dead entries free their slot.
    pub fn get(&self, key: u64) -> Option<String> {
        let mut guard = lock_recovering(&self.map);
        match guard.get(&key) {
            Some((stored_at, body)) if stored_at.elapsed() < self.ttl => Some(body.clone()),
            Some(_) => {
                // Expired — free the slot (lazy eviction).
                guard.remove(&key);
                None
            }
            None => None,
        }
    }

    /// Stores a body for `key`. At cap, EXPIRED entries are swept first
    /// (adversarial-review 2026-07-09 fix: with daily SID churn —
    /// derivative SecurityIds are unstable per `instrument-master.md`
    /// rule 3 — dead keys would otherwise accumulate toward the cap and
    /// permanently skip-insert every new SID, silently self-disabling the
    /// cache). If still at cap after the sweep, a NEW key is skip-inserted
    /// (the response was already served fresh — only the cache write is
    /// skipped); an EXISTING key is always overwritten in place. The sweep
    /// is O(cap) on this cold path and runs only at the cap boundary.
    pub fn put(&self, key: u64, body: String) {
        let mut guard = lock_recovering(&self.map);
        if guard.len() >= self.max_entries && !guard.contains_key(&key) {
            let ttl = self.ttl;
            guard.retain(|_, (stored_at, _)| stored_at.elapsed() < ttl);
            if guard.len() >= self.max_entries {
                return;
            }
        }
        guard.insert(key, (Instant::now(), body));
    }

    /// Current entry count (tests + observability).
    pub fn len(&self) -> usize {
        lock_recovering(&self.map).len()
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
        assert!(
            QUOTE_CACHE_MAX_ENTRIES >= 1200,
            "must hold the daily universe"
        );
        assert!(
            QUOTE_CACHE_MAX_ENTRIES <= 10_000,
            "must stay memory-bounded"
        );
    }
}
