//! Wave 7-A4 sub-PR #3 (W7-A4.3) — `BarCache`: today + yesterday
//! sealed bar RAM cache for the trading hot path.
//!
//! Per `.claude/rules/project/aws-budget.md` § "RAM-First Architecture":
//!
//! > Tick → strategy decision must read indicator state, today's sealed
//! > bars, yesterday's sealed bars, and prev_day_OI cache from RAM only.
//! > QuestDB is for: persistence, audit, cross-verify (cold path), boot
//! > rehydration.
//!
//! Wave 6 PRODUCES the bars (multi-TF aggregator → seal_writer →
//! shadow tables); Wave 7-A4 CONSUMES via this cache. Banned-pattern
//! Category 10 (PR #597) prevents indicator/strategy code from issuing
//! SELECT against QuestDB on the hot path; this module is the
//! in-RAM source those paths read instead.
//!
//! ## Compact representation
//!
//! 56 bytes per bar (vs `Bar` struct's ~136 B). Drops the 5 fields
//! that the indicator/strategy hot path does NOT need (sealed flag,
//! prev-day refs, % stamps — those live on the source `Bar` and are
//! accessed via `PrevDayCache` instead):
//!
//! | Field | Type | Bytes |
//! |---|---|---|
//! | `bucket_start_ist_secs` | u32 | 4 |
//! | `open` | f64 | 8 |
//! | `high` | f64 | 8 |
//! | `low` | f64 | 8 |
//! | `close` | f64 | 8 |
//! | `volume` | i64 | 8 |
//! | `oi` | i64 | 8 |
//! | `tick_count` | u32 | 4 |
//! | **Total** | | **56 B** |
//!
//! Memory footprint at 11K instruments × 9 TFs (averaged ~500 bars
//! per (instrument, TF) across the trading day) = ~5.5M bars × 56 B
//! = ~308 MB per day × 2 (today + yesterday) = **~616 MB**. Sits
//! comfortably inside the 2 GB app allocation per
//! `aws-budget.md` § "Tickvault App (2.0 GB) Memory Breakdown".
//!
//! The `aws-budget.md` original 176 MB target assumed a 32 B compact
//! format (f32 prices). f64 was kept here so the cache values
//! match the source `Bar` byte-for-byte and the existing
//! `f32_to_f64_clean` precision invariant per
//! `data-integrity.md` is not broken at the read path.
//!
//! ## Concurrency
//!
//! `papaya::HashMap` for O(1) lock-free reads on the seal hot path.
//! `Arc<HashMap>` clones are cheap atomic refcount bumps. Multiple
//! consumers (indicator engine, strategy evaluator, OMS risk check)
//! each hold a clone.

use std::sync::Arc;

use papaya::HashMap;

use crate::candles::TfIndex;

/// Composite key per I-P1-11 + timeframe + bucket-start.
///
/// `(security_id, exchange_segment_code, tf, bucket_start_ist_secs)`
/// where `bucket_start_ist_secs` is the IST seconds-of-day at which
/// the bar opened. Two bars on the same (security_id, segment, tf)
/// are distinct iff their bucket_start differs — which is the
/// natural sealing invariant the multi-TF aggregator already enforces.
type BarKey = (u64, u8, TfIndex, u32);

/// Compact 56-byte representation of a sealed `Bar` for the
/// indicator/strategy RAM-first read path.
///
/// Cheap to copy (`Copy + Clone`). Lookup hot path returns owned
/// `Option<CompactBar>` (no borrow lifetimes) so callers can use the
/// value across `await` points if needed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactBar {
    /// IST seconds-of-day at which the bar opened. Pinned by the
    /// aggregator's seal-time computation (see
    /// `crates/trading/src/candles/boundary_calc.rs`).
    pub bucket_start_ist_secs: u32,
    /// First trade price in the bucket.
    pub open: f64,
    /// Highest price during the bucket.
    pub high: f64,
    /// Lowest price during the bucket.
    pub low: f64,
    /// Last trade price in the bucket (the field every momentum
    /// indicator reads).
    pub close: f64,
    /// Sum of trade quantities inside the bucket (NOT cumulative-day
    /// total — that lives on the source `Bar.volume_cum_day_at_end`).
    pub volume: i64,
    /// Open interest snapshot at the bucket close (F&O only).
    pub oi: i64,
    /// Number of ticks the aggregator folded into this bar.
    pub tick_count: u32,
}

/// In-RAM cache of today's + yesterday's sealed bars across all
/// timeframes per `(security_id, exchange_segment_code)`.
///
/// Cloneable: `Arc::clone` on the inner papaya map. Multiple
/// consumers (indicator engine, strategy evaluator, OMS risk check
/// in W7-A4.4 follow-up) hold cheap clones.
#[derive(Clone, Default)]
pub struct BarCache {
    inner: Arc<HashMap<BarKey, CompactBar>>,
}

impl BarCache {
    /// Build an empty cache. The boot-time loader (Wave 7-A4 sub-PR #4)
    /// will read today's sealed bars from the shadow tables and
    /// populate via [`BarCache::insert`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert / overwrite a sealed bar.
    ///
    /// Cold path — called by:
    /// - boot-time rehydration loader (W7-A4.4)
    /// - aggregator seal closure (Wave 6 master switch in `app/main.rs`,
    ///   item 1.4d follow-up to also write to the cache)
    ///
    /// O(1) papaya pin + insert. ~50 ns per call. Bounded by total
    /// bar count (~5.5M today + 5.5M yesterday = 11M) so insert
    /// throughput at boot is ~200K/sec — full rehydrate takes ~30 sec.
    pub fn insert(
        &self,
        security_id: u64,
        exchange_segment_code: u8,
        tf: TfIndex,
        bar: CompactBar,
    ) {
        let key = (
            security_id,
            exchange_segment_code,
            tf,
            bar.bucket_start_ist_secs,
        );
        let pin = self.inner.pin();
        pin.insert(key, bar);
    }

    /// O(1) lock-free lookup. Returns `None` if the bar has not been
    /// produced yet (e.g. ahead of the current minute boundary, or
    /// the (security_id, segment, tf) tuple was never subscribed).
    ///
    /// Hot path on the indicator/strategy read site. ~30 ns per
    /// `papaya::HashMap::pin` + `get` + `Copy`.
    #[must_use]
    pub fn lookup(
        &self,
        security_id: u64,
        exchange_segment_code: u8,
        tf: TfIndex,
        bucket_start_ist_secs: u32,
    ) -> Option<CompactBar> {
        let pin = self.inner.pin();
        pin.get(&(
            security_id,
            exchange_segment_code,
            tf,
            bucket_start_ist_secs,
        ))
        .copied()
    }

    /// Drop all entries whose `bucket_start_ist_secs < threshold`.
    ///
    /// Called by the IST midnight rollover task: today becomes
    /// yesterday, and the day-before-yesterday bars age out of the
    /// cache. The threshold is the IST seconds-of-day for the start
    /// of yesterday (i.e. ~24h in the past from current IST midnight).
    ///
    /// Cold path. O(N) over all entries; expected to drop ~5.5M
    /// bars in <100 ms.
    pub fn clear_before(&self, bucket_start_threshold: u32) {
        let pin = self.inner.pin();
        // papaya does not expose bulk-remove; collect keys first then
        // remove. Bounded by total cache size.
        let stale_keys: Vec<BarKey> = pin
            .iter()
            .filter(|(k, _)| k.3 < bucket_start_threshold)
            .map(|(k, _)| *k)
            .collect(); // APPROVED: cold-path daily-rollover cleanup
        for k in stale_keys {
            pin.remove(&k);
        }
    }

    /// Total number of sealed bars in the cache (today + yesterday
    /// combined). Cold-path read.
    #[must_use]
    pub fn len_bars(&self) -> usize {
        let pin = self.inner.pin();
        pin.len()
    }

    /// Estimated bytes for the
    /// `tv_subsystem_memory_estimated_bytes{component="bar_cache"}`
    /// gauge. Reports `len_bars() × size_of::<CompactBar>()`.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        let len = self.len_bars() as u64;
        let per_entry = std::mem::size_of::<CompactBar>() as u64;
        len.saturating_mul(per_entry)
    }
}

/// Compute the `clear_before` threshold for the daily IST-midnight
/// BarCache rollover.
///
/// Given the current IST epoch second, returns the IST epoch second
/// at which "yesterday" begins. Bars with `bucket_start_ist_secs <
/// threshold` get evicted by [`BarCache::clear_before`], preserving
/// **today + yesterday** in the cache.
///
/// ## Example
///
/// At IST midnight on day-N:
/// - `now_ist_epoch_secs` = start of day-N (e.g. 1715040000)
/// - `today_midnight` = `now / 86400 * 86400` = day-N start
/// - `threshold` = `today_midnight - 86400` = day-(N-1) start
/// - Bars with `bucket_start < day-(N-1) start` get evicted
/// - Day-(N-1) (yesterday) + day-N (today, empty so far) preserved
///
/// ## Pure function
///
/// No I/O, no allocation, `const fn`-eligible (but kept `fn` for
/// `SECONDS_PER_DAY` import readability). Safe for use inside the
/// `tokio::spawn`-able daily rollover task without `await` points.
///
/// ## Status
///
/// Wave 7-A4.10 ships this helper as the pure-logic primitive. The
/// `tokio::spawn` task that calls `BarCache::clear_before(threshold)`
/// at IST midnight is a separate wiring follow-up (mirrors
/// `reset_scheduler::run_tick_storage_daily_reset`).
#[must_use]
pub fn bar_cache_clear_before_threshold(now_ist_epoch_secs: u32) -> u32 {
    use tickvault_common::constants::SECONDS_PER_DAY;
    let today_midnight = (now_ist_epoch_secs / SECONDS_PER_DAY) * SECONDS_PER_DAY;
    today_midnight.saturating_sub(SECONDS_PER_DAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bar(start: u32, close: f64, oi: i64) -> CompactBar {
        CompactBar {
            bucket_start_ist_secs: start,
            open: 100.0,
            high: 105.0,
            low: 95.0,
            close,
            volume: 1_000,
            oi,
            tick_count: 42,
        }
    }

    #[test]
    fn test_bar_cache_new_is_empty() {
        let cache = BarCache::new();
        assert_eq!(cache.len_bars(), 0);
        assert_eq!(cache.estimated_bytes(), 0);
    }

    #[test]
    fn test_compact_bar_size_is_56_bytes() {
        // Locks the compact-format size invariant cited in the
        // module docstring + aws-budget.md memory footprint math.
        // If this regresses (e.g. someone adds an f64 field), the
        // cache memory footprint at 11M bars goes up by 88 MB per
        // 8-byte field — operator should re-evaluate the budget.
        assert_eq!(std::mem::size_of::<CompactBar>(), 56);
    }

    #[test]
    fn test_insert_and_lookup_round_trip() {
        let cache = BarCache::new();
        let bar = make_bar(540, 200.0, 1_000_000);
        cache.insert(13, 0, TfIndex::M1, bar);
        let got = cache
            .lookup(13, 0, TfIndex::M1, 540)
            .expect("inserted bar must be retrievable");
        assert_eq!(got, bar);
    }

    #[test]
    fn test_lookup_returns_none_for_unknown_key() {
        let cache = BarCache::new();
        cache.insert(13, 0, TfIndex::M1, make_bar(540, 200.0, 0));
        // Different security_id
        assert!(cache.lookup(99, 0, TfIndex::M1, 540).is_none());
        // Different segment
        assert!(cache.lookup(13, 1, TfIndex::M1, 540).is_none());
        // Different tf
        assert!(cache.lookup(13, 0, TfIndex::M5, 540).is_none());
        // Different bucket_start
        assert!(cache.lookup(13, 0, TfIndex::M1, 600).is_none());
    }

    /// I-P1-11 ratchet: same `security_id`, different segment must
    /// resolve to distinct cache entries. Pre-segment-aware bug class
    /// would have collapsed both into one cache entry.
    #[test]
    fn test_bar_cache_isolates_cross_segment_collisions_per_i_p1_11() {
        let cache = BarCache::new();
        cache.insert(27, 0, TfIndex::M1, make_bar(540, 18000.0, 0)); // FINNIFTY IDX_I
        cache.insert(27, 1, TfIndex::M1, make_bar(540, 555.0, 0)); // hypothetical NSE_EQ id=27
        let idx = cache.lookup(27, 0, TfIndex::M1, 540).expect("IDX_I bar");
        let eq = cache.lookup(27, 1, TfIndex::M1, 540).expect("NSE_EQ bar");
        assert_eq!(idx.close, 18000.0);
        assert_eq!(eq.close, 555.0);
        assert_eq!(cache.len_bars(), 2);
    }

    #[test]
    fn test_insert_overwrites_existing_entry() {
        let cache = BarCache::new();
        cache.insert(13, 0, TfIndex::M1, make_bar(540, 100.0, 0));
        cache.insert(13, 0, TfIndex::M1, make_bar(540, 200.0, 0));
        let got = cache.lookup(13, 0, TfIndex::M1, 540).expect("bar");
        assert_eq!(got.close, 200.0);
        assert_eq!(cache.len_bars(), 1);
    }

    #[test]
    fn test_clear_before_drops_stale_bars() {
        let cache = BarCache::new();
        // Yesterday's bars (start < 540 IST seconds = 09:00)
        cache.insert(13, 0, TfIndex::M1, make_bar(120, 100.0, 0));
        cache.insert(13, 0, TfIndex::M1, make_bar(300, 110.0, 0));
        // Today's bars (start >= 540)
        cache.insert(13, 0, TfIndex::M1, make_bar(540, 200.0, 0));
        cache.insert(13, 0, TfIndex::M1, make_bar(600, 210.0, 0));
        assert_eq!(cache.len_bars(), 4);
        cache.clear_before(540);
        assert_eq!(cache.len_bars(), 2);
        // Surviving bars are the >= 540 ones.
        assert!(cache.lookup(13, 0, TfIndex::M1, 540).is_some());
        assert!(cache.lookup(13, 0, TfIndex::M1, 600).is_some());
        assert!(cache.lookup(13, 0, TfIndex::M1, 120).is_none());
        assert!(cache.lookup(13, 0, TfIndex::M1, 300).is_none());
    }

    #[test]
    fn test_clear_before_zero_drops_nothing() {
        let cache = BarCache::new();
        cache.insert(13, 0, TfIndex::M1, make_bar(540, 200.0, 0));
        cache.clear_before(0);
        assert_eq!(cache.len_bars(), 1);
    }

    #[test]
    fn test_clone_shares_state_via_arc() {
        let writer = BarCache::new();
        let reader = writer.clone();
        writer.insert(13, 0, TfIndex::M1, make_bar(540, 200.0, 0));
        // Reader sees writer's insert via shared Arc.
        assert_eq!(reader.len_bars(), 1);
        assert!(reader.lookup(13, 0, TfIndex::M1, 540).is_some());
    }

    #[test]
    fn test_estimated_bytes_scales_with_entry_count() {
        let cache = BarCache::new();
        let per_entry = std::mem::size_of::<CompactBar>() as u64;
        assert_eq!(cache.estimated_bytes(), 0);
        cache.insert(13, 0, TfIndex::M1, make_bar(540, 200.0, 0));
        assert_eq!(cache.estimated_bytes(), per_entry);
        cache.insert(13, 0, TfIndex::M5, make_bar(540, 200.0, 0));
        assert_eq!(cache.estimated_bytes(), per_entry * 2);
    }

    #[test]
    fn test_estimated_bytes_drops_after_clear_before() {
        let cache = BarCache::new();
        cache.insert(13, 0, TfIndex::M1, make_bar(120, 100.0, 0));
        cache.insert(13, 0, TfIndex::M1, make_bar(540, 200.0, 0));
        let before = cache.estimated_bytes();
        cache.clear_before(540);
        let after = cache.estimated_bytes();
        assert!(after < before);
        assert_eq!(after, std::mem::size_of::<CompactBar>() as u64);
    }

    /// Concurrent inserts from multiple threads must not lose entries
    /// — papaya is wait-free for inserts, but the test pins the
    /// invariant against future regression (e.g. wrapping in a
    /// std::sync::Mutex would silently lose throughput).
    #[test]
    fn test_concurrent_insert_does_not_lose_entries() {
        use std::thread;
        const THREADS: u32 = 4;
        const PER_THREAD: u32 = 250;
        let cache = BarCache::new();
        let mut handles = Vec::new();
        for thread_idx in 0..THREADS {
            let cc = cache.clone();
            handles.push(thread::spawn(move || {
                for i in 0..PER_THREAD {
                    cc.insert(
                        u64::from(thread_idx),
                        0,
                        TfIndex::M1,
                        make_bar(i, f64::from(i), 0),
                    );
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }
        // 4 threads × 250 distinct bucket_starts = 1,000 entries
        // (each thread uses its own thread_idx as security_id so no
        // cross-thread overwrites).
        assert_eq!(cache.len_bars(), (THREADS * PER_THREAD) as usize);
    }

    /// Multiple TFs for the same (security_id, segment) must coexist
    /// — the key tuple includes TfIndex so M1/M5/M15/H1 bars on the
    /// same instrument do not collide.
    #[test]
    fn test_multiple_timeframes_coexist() {
        let cache = BarCache::new();
        for tf in [TfIndex::M1, TfIndex::M5, TfIndex::M15, TfIndex::H1] {
            cache.insert(13, 0, tf, make_bar(540, 200.0, 0));
        }
        assert_eq!(cache.len_bars(), 4);
        assert!(cache.lookup(13, 0, TfIndex::M1, 540).is_some());
        assert!(cache.lookup(13, 0, TfIndex::M5, 540).is_some());
        assert!(cache.lookup(13, 0, TfIndex::M15, 540).is_some());
        assert!(cache.lookup(13, 0, TfIndex::H1, 540).is_some());
    }

    /// Total memory math sanity check: the cache's estimated_bytes
    /// must match `len() × size_of::<CompactBar>()` exactly so the
    /// L18 / #504a subsystem_memory sampler reports an honest gauge.
    #[test]
    fn test_estimated_bytes_matches_compact_bar_size() {
        let cache = BarCache::new();
        for i in 0..100u32 {
            cache.insert(u64::from(i), 0, TfIndex::M1, make_bar(540, 200.0, 0));
        }
        let expected = 100 * std::mem::size_of::<CompactBar>() as u64;
        assert_eq!(cache.estimated_bytes(), expected);
    }

    // Wave 7-A4 sub-PR #10 (W7-A4.10) — daily IST-midnight rollover
    // threshold helper.

    #[test]
    fn test_threshold_at_exact_ist_midnight_returns_yesterday_start() {
        // now = exact IST midnight today (multiple of 86400).
        let today_midnight: u32 = 1_715_040_000; // arbitrary aligned value
        let yesterday_start = today_midnight - 86_400;
        assert_eq!(
            bar_cache_clear_before_threshold(today_midnight),
            yesterday_start
        );
    }

    #[test]
    fn test_threshold_at_noon_returns_yesterday_start() {
        // now = noon (12:00:00) on today. Threshold = start of
        // yesterday, NOT start of today (we want to keep today + yesterday).
        let today_midnight: u32 = 1_715_040_000;
        let noon = today_midnight + 12 * 3600;
        let yesterday_start = today_midnight - 86_400;
        assert_eq!(bar_cache_clear_before_threshold(noon), yesterday_start);
    }

    #[test]
    fn test_threshold_at_one_second_before_next_midnight() {
        // now = 23:59:59 on today. Threshold still = yesterday_start.
        let today_midnight: u32 = 1_715_040_000;
        let last_sec = today_midnight + 86_399;
        let yesterday_start = today_midnight - 86_400;
        assert_eq!(bar_cache_clear_before_threshold(last_sec), yesterday_start);
    }

    #[test]
    fn test_threshold_at_one_second_after_next_midnight() {
        // now = 00:00:01 on tomorrow. Threshold ROLLS to today's start.
        let today_midnight: u32 = 1_715_040_000;
        let tomorrow_first_sec = today_midnight + 86_401;
        // After rollover: "today" is what was tomorrow; "yesterday" is what was today.
        // threshold = (tomorrow_midnight) - 86400 = today_midnight.
        let expected_threshold = today_midnight;
        assert_eq!(
            bar_cache_clear_before_threshold(tomorrow_first_sec),
            expected_threshold
        );
    }

    #[test]
    fn test_threshold_saturates_at_epoch_zero() {
        // Defensive: if now < 86400 (impossibly early in IST epoch),
        // saturating_sub prevents underflow.
        assert_eq!(bar_cache_clear_before_threshold(0), 0);
        assert_eq!(bar_cache_clear_before_threshold(100), 0);
        assert_eq!(bar_cache_clear_before_threshold(86_399), 0);
    }

    #[test]
    fn test_threshold_used_with_clear_before_keeps_yesterday_and_today() {
        let cache = BarCache::new();
        let today_midnight: u32 = 1_715_040_000;
        let day_before_yesterday_start = today_midnight - 2 * 86_400;
        let yesterday_start = today_midnight - 86_400;
        let today_noon = today_midnight + 12 * 3600;
        // Insert bars from 3 different days.
        cache.insert(
            13,
            0,
            TfIndex::M1,
            make_bar(day_before_yesterday_start, 100.0, 0),
        );
        cache.insert(13, 0, TfIndex::M1, make_bar(yesterday_start, 110.0, 0));
        cache.insert(13, 0, TfIndex::M1, make_bar(today_noon, 120.0, 0));
        assert_eq!(cache.len_bars(), 3);
        // Apply rollover threshold "as of now = today noon".
        let threshold = bar_cache_clear_before_threshold(today_noon);
        cache.clear_before(threshold);
        // Day-before-yesterday is dropped; yesterday + today survive.
        assert_eq!(cache.len_bars(), 2);
        assert!(
            cache
                .lookup(13, 0, TfIndex::M1, day_before_yesterday_start)
                .is_none()
        );
        assert!(cache.lookup(13, 0, TfIndex::M1, yesterday_start).is_some());
        assert!(cache.lookup(13, 0, TfIndex::M1, today_noon).is_some());
    }
}
