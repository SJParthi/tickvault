//! Option-chain minute-snapshot RAM cache (PR #4a of 5 per
//! `.claude/plans/friday-may-15-mega/topic-OPTION-CHAIN-MINUTE-SNAPSHOT.md`).
//!
//! The strategy NEVER awaits the fetch. It reads the freshest snapshot
//! from this cache via O(1) papaya HashMap lookup. The fetch task
//! writes; the strategy reads. Decoupled lifecycles.
//!
//! ## The 3 staleness states (Layer 3 + Layer 5 of the 5-Layer Defense)
//!
//! | State | Cache age | Strategy behaviour |
//! |---|---|---|
//! | Fresh | ≤ ~60s typical | Read + decide normally |
//! | Stale-warn | 60s..threshold | Read + decide; counter +1; informational |
//! | Stale-halt | > threshold | REFUSE new entries; Severity::Critical Telegram |
//!
//! ## Why papaya
//!
//! Lock-free reads + sharded writes. Strategy thread + fetch task can
//! both hammer the map without lock contention. The map is keyed by
//! `(underlying_symbol, exchange_segment)` per I-P1-11 — `symbol`
//! alone would collide if BSE ever ships an underlying with the same
//! name as an NSE one.
//!
//! ## What's stored per slot
//!
//! Operator-facing: `last_price` (spot) + the strike map needed for
//! strike-selection. Greeks/IV/OI are also kept so a future strategy
//! can use them without a re-fetch.
//!
//! ## What's NOT stored here
//!
//! Disk persistence — there is none. The `option_chain_minute_snapshot`
//! QuestDB table + its persistence module were dropped 2026-06-23
//! (operator: ticks are the single source of truth). This RAM cache is
//! now the ONLY store for option-chain snapshots; the strategy reads it
//! directly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use papaya::HashMap;

use crate::option_chain::types::OptionChainResponse;

/// One slot in the cache — a full Dhan response plus the instant it
/// was received. The age check is computed on read, not cached, so a
/// strategy that reads twice in 5 seconds sees the second read as
/// slightly older than the first (correctly).
#[derive(Debug, Clone)]
pub struct CachedSnapshot {
    /// Full Dhan response — the strategy can drill into spot price,
    /// strikes, greeks, IV, OI from this one struct.
    pub response: Arc<OptionChainResponse>,
    /// Instant the response was deserialized (NOT the Dhan-stamped
    /// timestamp inside the response). Used for cache-age math.
    pub received_at: Instant,
    /// Operator-visible underlying label (denormalized for fast logging
    /// without the strategy holding a separate config handle).
    pub underlying_symbol: String,
    /// Operator-visible expiry string (e.g. "2026-05-22"). Same
    /// denormalization rationale.
    pub expiry: String,
}

impl CachedSnapshot {
    /// Cache age relative to `now`. Returns the raw `Duration`; callers
    /// decide which threshold to compare against. Cheap arithmetic —
    /// no atomic, no lock.
    #[must_use]
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.received_at)
    }

    /// Whole-second age suitable for Prometheus gauges + Telegram body.
    /// Saturates at `u32::MAX` (~136 years) so the operator never sees
    /// an integer-wrap negative.
    #[must_use]
    pub fn age_secs(&self, now: Instant) -> u32 {
        u32::try_from(self.age(now).as_secs()).unwrap_or(u32::MAX)
    }
}

/// Composite key per I-P1-11. `String` ownership keeps the map shape
/// stable across config reloads (a future feature) without forcing
/// `'static` lifetimes through the strategy.
pub type SnapshotKey = (String, String); // (underlying_symbol, exchange_segment)

/// The cache itself. Cloneable handle — internally `Arc` so cloning
/// is cheap and every clone sees the same underlying papaya map.
///
/// Construction is `new()`. Writes are `insert()`. Reads are `get()`.
/// No `clear()` — the cache lives for the process lifetime; the
/// fetch task overwrites with newer snapshots.
#[derive(Clone)]
pub struct SnapshotCache {
    inner: Arc<HashMap<SnapshotKey, CachedSnapshot>>,
}

impl SnapshotCache {
    /// Construct an empty cache. The papaya map's initial capacity is
    /// sized for the 3 operator-locked underlyings + 7 headroom = 10
    /// (matches `OPTION_CHAIN_MAX_UNDERLYINGS`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HashMap::with_capacity(10)),
        }
    }

    /// Insert a new snapshot under `(underlying, segment)`. Returns
    /// the previous value if one existed (operator can inspect for
    /// debugging; production callers ignore).
    pub fn insert(&self, key: SnapshotKey, snapshot: CachedSnapshot) {
        let guard = self.inner.pin();
        guard.insert(key, snapshot);
    }

    /// Read the snapshot for `(underlying, segment)`. Returns `None`
    /// if the slot has never been written. Cloning is cheap because
    /// the inner `response` is `Arc`.
    #[must_use]
    pub fn get(&self, key: &SnapshotKey) -> Option<CachedSnapshot> {
        let guard = self.inner.pin();
        guard.get(key).cloned()
    }

    /// Number of distinct `(underlying, segment)` slots currently in
    /// the cache. Used by health-check tooling + a planned future
    /// Grafana gauge. O(N) over slots — N ≤ 10 in practice.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.pin().len()
    }

    /// True iff `len() == 0`. Boot-time check before the first slot
    /// fetches; if true after market open + 90s grace, something is
    /// catastrophically wrong.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SnapshotCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::option_chain::types::OptionChainData;
    use std::collections::HashMap as StdHashMap;
    use std::thread::sleep;

    fn make_snapshot(spot: f64, underlying: &str, expiry: &str) -> CachedSnapshot {
        let response = OptionChainResponse {
            data: OptionChainData {
                last_price: spot,
                oc: StdHashMap::new(),
            },
            status: "success".to_string(),
        };
        CachedSnapshot {
            response: Arc::new(response),
            received_at: Instant::now(),
            underlying_symbol: underlying.to_string(),
            expiry: expiry.to_string(),
        }
    }

    #[test]
    fn test_snapshot_cache_new_is_empty() {
        let cache = SnapshotCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_snapshot_cache_insert_and_get_roundtrip() {
        let cache = SnapshotCache::new();
        let key = ("NIFTY".to_string(), "IDX_I".to_string());
        let snapshot = make_snapshot(25_650.0, "NIFTY", "2026-05-22");
        cache.insert(key.clone(), snapshot);

        let read = cache.get(&key).expect("must read what we wrote");
        assert!((read.response.data.last_price - 25_650.0).abs() < f64::EPSILON);
        assert_eq!(read.underlying_symbol, "NIFTY");
        assert_eq!(read.expiry, "2026-05-22");
    }

    #[test]
    fn test_snapshot_cache_get_missing_returns_none() {
        let cache = SnapshotCache::new();
        let key = ("MISSING".to_string(), "IDX_I".to_string());
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn test_snapshot_cache_overwrite_replaces() {
        let cache = SnapshotCache::new();
        let key = ("NIFTY".to_string(), "IDX_I".to_string());

        cache.insert(key.clone(), make_snapshot(25_000.0, "NIFTY", "2026-05-22"));
        cache.insert(key.clone(), make_snapshot(25_650.0, "NIFTY", "2026-05-22"));

        let read = cache.get(&key).unwrap();
        assert!((read.response.data.last_price - 25_650.0).abs() < f64::EPSILON);
        // Still one slot — overwrites, doesn't append.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_snapshot_cache_three_underlyings_locked_schedule() {
        // The operator-locked 3-underlying set must coexist in the
        // same cache without collisions — composite key per I-P1-11
        // ensures that.
        let cache = SnapshotCache::new();
        for (sym, spot) in [
            ("SENSEX", 79_500.0),
            ("BANKNIFTY", 48_200.0),
            ("NIFTY", 25_650.0),
        ] {
            cache.insert(
                (sym.to_string(), "IDX_I".to_string()),
                make_snapshot(spot, sym, "2026-05-22"),
            );
        }
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn test_snapshot_cache_same_underlying_different_segment_is_distinct() {
        // Defensive: per I-P1-11, the same `symbol` on a different
        // `segment` is a DIFFERENT cache slot. If future operator
        // ever adds an NSE_EQ-segmented underlying with the same
        // label as an IDX_I one, both must coexist.
        let cache = SnapshotCache::new();
        let key_idx = ("FOO".to_string(), "IDX_I".to_string());
        let key_eq = ("FOO".to_string(), "NSE_EQ".to_string());
        cache.insert(key_idx, make_snapshot(100.0, "FOO", "2026-05-22"));
        cache.insert(key_eq, make_snapshot(200.0, "FOO", "2026-05-22"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_cached_snapshot_age_zero_at_creation() {
        let snapshot = make_snapshot(100.0, "FOO", "2026-05-22");
        let age = snapshot.age(snapshot.received_at);
        assert!(age.is_zero(), "age at exact same instant must be zero");
    }

    #[test]
    fn test_cached_snapshot_age_grows() {
        let snapshot = make_snapshot(100.0, "FOO", "2026-05-22");
        // Sleep a tiny bit so wall-clock advances beyond Instant
        // resolution on the host.
        sleep(Duration::from_millis(15));
        let now = Instant::now();
        let age = snapshot.age(now);
        assert!(
            age >= Duration::from_millis(10),
            "age must grow when wall clock advances; got {age:?}"
        );
    }

    #[test]
    fn test_cached_snapshot_age_secs_saturates_no_negative() {
        // `now` BEFORE `received_at` would normally underflow; the
        // `saturating_duration_since` call clamps to zero. age_secs
        // therefore returns 0, not u32::MAX or a panic.
        let snapshot = make_snapshot(100.0, "FOO", "2026-05-22");
        let earlier = snapshot
            .received_at
            .checked_sub(Duration::from_secs(10))
            .unwrap_or(snapshot.received_at);
        let age = snapshot.age_secs(earlier);
        assert_eq!(age, 0, "age must saturate at 0 (not underflow)");
    }

    #[test]
    fn test_snapshot_cache_clone_shares_storage() {
        // The cloneable handle pattern is critical — fetch task and
        // strategy task each get their own clone, but both write/read
        // the SAME papaya map.
        let cache = SnapshotCache::new();
        let cache2 = cache.clone();
        let key = ("NIFTY".to_string(), "IDX_I".to_string());

        cache.insert(key.clone(), make_snapshot(25_650.0, "NIFTY", "2026-05-22"));
        // Reading from the clone must see the write.
        let read = cache2.get(&key).expect("clone must share storage");
        assert!((read.response.data.last_price - 25_650.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_snapshot_cache_default_equivalent_to_new() {
        let a = SnapshotCache::new();
        let b = SnapshotCache::default();
        assert!(a.is_empty() && b.is_empty());
        assert_eq!(a.len(), b.len());
    }
}
