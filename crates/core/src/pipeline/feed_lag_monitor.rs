//! Per-feed DAY lag histograms — `day_lag_summary` for the 15:45 scoreboard.
//!
//! # 2026-07-17 — the Dhan exchange-lag RING/PUBLISHER half is DELETED
//!
//! This module previously ALSO carried the Dhan exchange→receive lag ring +
//! the 10 s `tv_dhan_exchange_lag_p99_seconds` publisher
//! (`run_dhan_lag_publisher` / `record_dhan_tick` / `FeedLagRing` /
//! `compute_window_p99_ns` — the 2026-07-06 silent-feed incident hardening).
//! That half lost its tick source AND its spawn sites with the Dhan live-WS
//! lane deletion (PR-C2, 2026-07-13) and the stage-2 dead-WS sweep
//! (2026-07-17 — the persist-site callers in `tick_processor.rs` died with
//! the dead tick chain), so the gauge and its companion counter
//! (`tv_dhan_lag_samples_excluded_total`) could never be published again.
//! Deleted 2026-07-17 (dashboard tidy) together with its CloudWatch alarm
//! (`dhan_exchange_lag_p99_high`, silent-feed-alarms.tf), its 2 EMF
//! allowlist entries, its Criterion bench (`feed_lag_ring.rs`), its DHAT
//! ratchet (`dhat_feed_lag_ring.rs`) and its benchmark-budget keys.
//!
//! # What remains — the per-feed DAY lag histograms (scoreboard PR-C)
//!
//! [`DailyLagHistogram`] — 96 quarter-octave log2 buckets over 1 ms…1 h
//! (fixed-size atomic arrays). The 15:45 IST scoreboard drains them into
//! the `feed_scoreboard_daily` lag columns via [`day_lag_summary`]
//! (p50/p99/max/samples); the IST midnight tasks reset them
//! ([`reset_day_lag_histogram`]). HONEST ENVELOPE (2026-07-17): with both
//! live feeds retired (Groww fold deleted 2026-07-15, Dhan fold deleted
//! 2026-07-17) the histograms currently have ZERO producers — both feed
//! slots legitimately read `None` and the scoreboard publishes its
//! documented −1 sentinels / fold-forward path, never a fabricated
//! distribution (Rule 11). The read/reset contract is kept LIVE for its
//! consumers (`feed_scoreboard_boot.rs` drain + the main.rs midnight
//! reset); the fold entry (`DailyLagHistogram::record_ns`) is retained
//! test-covered for any future producer.

use std::sync::atomic::{AtomicU64, Ordering};

use tickvault_common::feed::Feed;

/// Minimum samples in a day distribution before a summary is published.
/// Below this, publish NOTHING (Rule 11 — a thin/empty distribution must
/// not read as a real one).
const MIN_LAG_SAMPLES: usize = 50;

const NANOS_PER_MS: i64 = 1_000_000;

/// Day-histogram bucket count: 24 octaves × 4 quarter-octave sub-buckets
/// over 1 ms … [`LAG_HIST_MAX_MS`] (values above the cap clamp into the top
/// bucket). Quarter-octave resolution keeps the bucketed p50/p99 within
/// ~±9% relative error — enough to separate a 180 ms Groww median from a
/// 1.2 s Dhan one on the daily scorecard.
const LAG_HIST_BUCKETS: usize = 96;

/// Day-histogram value cap in milliseconds (1 hour). A lag above this is a
/// data anomaly, not a distribution point — it clamps into the top bucket
/// and still drives the true `fetch_max`.
const LAG_HIST_MAX_MS: u64 = 3_600_000;

// ---------------------------------------------------------------------------
// Per-feed DAY lag histograms (scoreboard PR-C)
// ---------------------------------------------------------------------------

/// Day-scoped lag histogram: 96 quarter-octave log2 buckets over
/// 1 ms…[`LAG_HIST_MAX_MS`] + a true `fetch_max`. Fixed-size atomic arrays
/// — the record path is one relaxed `fetch_add` + one relaxed `fetch_max`,
/// zero-alloc, O(1). Drain/reset are cold (O(96)). Reads during a drain are
/// relaxed snapshots: a racing tick can shift a bucketed percentile by one
/// sample out of ≥50 — statistically negligible, documented.
struct DailyLagHistogram {
    buckets: [AtomicU64; LAG_HIST_BUCKETS],
    max_ms: AtomicU64,
}

/// One feed's drained day-lag distribution (scoreboard PR-C). Values in
/// milliseconds; percentiles are quarter-octave bucket-midpoint estimates
/// clamped to the true recorded max.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayLagSummary {
    pub p50_ms: i64,
    pub p99_ms: i64,
    pub max_ms: i64,
    pub samples: i64,
}

/// Quarter-octave bucket index for a millisecond lag value: 4 sub-buckets
/// per octave keyed on the exponent + the two bits below the MSB. `v < 1`
/// folds into bucket 0; values above [`LAG_HIST_MAX_MS`] clamp into the top
/// occupied index. Pure bit math — no float, no table.
fn lag_hist_bucket_index(lag_ms: u64) -> usize {
    let v = lag_ms.clamp(1, LAG_HIST_MAX_MS);
    // v ≥ 1 → leading_zeros ≤ 63; exp = floor(log2 v) ∈ [0, 21] after clamp.
    let exp = usize::try_from(63 - v.leading_zeros()).unwrap_or(0);
    // Top-2 fraction bits below the MSB: (v << 2) is safe (v ≤ 2^22 after
    // the clamp), then shift the MSB down to bit 2.
    let frac = usize::try_from(((v << 2) >> exp) & 3).unwrap_or(0);
    (exp * 4 + frac).min(LAG_HIST_BUCKETS - 1)
}

/// Lower bound of bucket `idx` in QUARTER-milliseconds — the exact inverse
/// of [`lag_hist_bucket_index`]: `(4 + frac) · 2^exp`. Quarter-ms units
/// keep the low-exponent bounds (1, 1.25, 1.5, 1.75 ms) exact integers —
/// plain-ms integer division would degenerate them all to 1.
fn lag_hist_bucket_lower_quarter_ms(idx: usize) -> u64 {
    let exp = idx / 4;
    let frac = (idx % 4) as u64;
    // (4 + frac) << exp — exp ≤ 24 so no overflow anywhere near u64.
    (4 + frac) << exp
}

impl DailyLagHistogram {
    const fn new() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        // APPROVED: const-array initializer element — each array slot gets its own AtomicU64.
        const ZERO: AtomicU64 = AtomicU64::new(0);
        Self {
            buckets: [ZERO; LAG_HIST_BUCKETS],
            max_ms: AtomicU64::new(0),
        }
    }

    /// Hot-path fold: O(1), zero-alloc (one relaxed `fetch_add` + one
    /// relaxed `fetch_max`).
    #[cfg_attr(not(test), allow(dead_code))]
    // APPROVED: production-uncalled since 2026-07-17 (both live-feed folds
    // deleted — Groww 2026-07-15, Dhan 2026-07-17); retained test-covered as
    // the fold entry for any future producer (module doc, dashboard tidy).
    fn record_ns(&self, lag_ns: u64) {
        let lag_ms = lag_ns / (NANOS_PER_MS as u64);
        self.buckets[lag_hist_bucket_index(lag_ms)].fetch_add(1, Ordering::Relaxed);
        self.max_ms.fetch_max(lag_ms, Ordering::Relaxed);
    }

    /// IST-midnight reset (cold, O(96)).
    fn reset(&self) {
        for b in &self.buckets {
            b.store(0, Ordering::Relaxed);
        }
        self.max_ms.store(0, Ordering::Relaxed);
    }

    /// Drain the day distribution WITHOUT resetting (re-runs of the 15:45
    /// task stay idempotent; the midnight task owns the reset). Returns
    /// `None` below [`MIN_LAG_SAMPLES`] — a thin day must publish −1
    /// sentinels, never a fabricated distribution (Rule 11).
    ///
    /// # Performance
    /// **O(96) — cold path** (once per 15:45 run / forced re-run).
    fn summary(&self) -> Option<DayLagSummary> {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect(); // APPROVED: cold path — once per 15:45 scoreboard run (O(96)), never per-tick
        let total: u64 = counts.iter().sum();
        if (usize::try_from(total).unwrap_or(usize::MAX)) < MIN_LAG_SAMPLES {
            return None;
        }
        let max_ms = self.max_ms.load(Ordering::Relaxed);
        // Rank via the same 1-based ceil convention as compute_window_p99_ns.
        let rank = |pct_num: u64| -> u64 { (total * pct_num).div_ceil(100).max(1) };
        let value_at = |target_rank: u64| -> u64 {
            let mut cum: u64 = 0;
            for (idx, c) in counts.iter().enumerate() {
                cum = cum.saturating_add(*c);
                if cum >= target_rank {
                    // Bucket midpoint (quarter-ms bounds → /8 back to ms),
                    // clamped to the true recorded max (the top occupied
                    // bucket's midpoint can overshoot).
                    let lower_q = lag_hist_bucket_lower_quarter_ms(idx);
                    let upper_q = lag_hist_bucket_lower_quarter_ms(idx + 1);
                    return ((lower_q + upper_q) / 8).min(max_ms);
                }
            }
            max_ms
        };
        Some(DayLagSummary {
            p50_ms: i64::try_from(value_at(rank(50))).unwrap_or(i64::MAX),
            p99_ms: i64::try_from(value_at(rank(99))).unwrap_or(i64::MAX),
            max_ms: i64::try_from(max_ms).unwrap_or(i64::MAX),
            samples: i64::try_from(total).unwrap_or(i64::MAX),
        })
    }
}

/// Per-feed day histograms — const-initialized statics (no OnceLock needed;
/// the arrays are fixed-size atomics).
static DHAN_DAY_LAG_HIST: DailyLagHistogram = DailyLagHistogram::new();
static GROWW_DAY_LAG_HIST: DailyLagHistogram = DailyLagHistogram::new();

fn day_hist(feed: Feed) -> &'static DailyLagHistogram {
    match feed {
        Feed::Dhan => &DHAN_DAY_LAG_HIST,
        Feed::Groww => &GROWW_DAY_LAG_HIST,
    }
}

/// Drain one feed's day-lag distribution for the 15:45 IST scorecard
/// (non-destructive — re-runs read the same day data; the midnight reset
/// owns the day boundary). `None` below the 50-sample floor.
///
/// # Performance
/// **O(96) — cold path**, once per scoreboard run.
pub fn day_lag_summary(feed: Feed) -> Option<DayLagSummary> {
    day_hist(feed).summary()
}

/// IST-midnight day-histogram reset — called from BOTH existing midnight
/// force-seal tasks (the Dhan Task 3 in main.rs + the Groww task in
/// groww_bridge.rs), each resetting its OWN feed. Cold, O(96).
pub fn reset_day_lag_histogram(feed: Feed) {
    day_hist(feed).reset();
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Day lag histograms (scoreboard PR-C) ──────────────────────────────

    #[test]
    fn test_lag_hist_bucket_index_and_bounds_roundtrip() {
        // Quarter-octave bit math: every value must land in the bucket
        // whose [lower, next-lower) range contains it, and lower bounds
        // must be strictly monotonic.
        assert_eq!(lag_hist_bucket_index(0), 0); // sub-ms folds into bucket 0
        assert_eq!(lag_hist_bucket_index(1), 0);
        assert_eq!(lag_hist_bucket_index(1000), 39); // 1000 ms ∈ [896, 1024)
        assert_eq!(lag_hist_bucket_lower_quarter_ms(39), 896 * 4);
        assert_eq!(lag_hist_bucket_lower_quarter_ms(40), 1024 * 4);
        // Containment in quarter-ms units (exact at every exponent).
        for v in [1u64, 2, 3, 7, 18, 150, 999, 1000, 4096, 300_000, 3_599_999] {
            let idx = lag_hist_bucket_index(v);
            assert!(
                lag_hist_bucket_lower_quarter_ms(idx) <= 4 * v
                    && 4 * v < lag_hist_bucket_lower_quarter_ms(idx + 1),
                "v={v} idx={idx} quarter-range=[{}, {})",
                lag_hist_bucket_lower_quarter_ms(idx),
                lag_hist_bucket_lower_quarter_ms(idx + 1)
            );
        }
        // Above the cap: clamps into a valid top index, never out of bounds.
        assert!(lag_hist_bucket_index(u64::MAX) < LAG_HIST_BUCKETS);
        // Strictly monotonic lower bounds across the whole table.
        for i in 0..LAG_HIST_BUCKETS {
            assert!(lag_hist_bucket_lower_quarter_ms(i) < lag_hist_bucket_lower_quarter_ms(i + 1));
        }
    }

    #[test]
    fn test_day_histogram_percentiles_on_known_distribution() {
        let h = DailyLagHistogram::new();
        // 100 samples: 99 at 150 ms + 1 at 2.9 s → p50 AND p99 (rank 99 of
        // 100) in the 150 ms bucket, max 2900.
        for _ in 0..99 {
            h.record_ns(150 * NANOS_PER_MS as u64);
        }
        h.record_ns(2_900 * NANOS_PER_MS as u64);
        let s = h.summary().expect("100 samples clears the 50-sample floor");
        assert_eq!(s.samples, 100);
        assert_eq!(s.max_ms, 2_900);
        // 150 ms → exp 7 (128), frac (600>>7)&3 = 0 → bucket [128, 160) →
        // midpoint 144. Quarter-octave estimate (±~9% relative, documented).
        assert_eq!(s.p50_ms, 144);
        assert_eq!(s.p99_ms, 144);
        // Tail-heavy distribution: 50 at 150 ms + 50 at 2.9 s → rank(99)
        // lands in the 2.9 s bucket, clamped to the true recorded max.
        let h2 = DailyLagHistogram::new();
        for _ in 0..50 {
            h2.record_ns(150 * NANOS_PER_MS as u64);
            h2.record_ns(2_900 * NANOS_PER_MS as u64);
        }
        let s2 = h2.summary().expect("100 samples");
        assert_eq!(s2.p50_ms, 144);
        assert!(
            s2.p99_ms > 2_500 && s2.p99_ms <= 2_900,
            "p99 must land in the tail bucket, clamped to the true max: {}",
            s2.p99_ms
        );
    }

    #[test]
    fn test_day_histogram_thin_day_returns_none_and_reset_clears() {
        let h = DailyLagHistogram::new();
        for _ in 0..(MIN_LAG_SAMPLES - 1) {
            h.record_ns(1_000_000_000);
        }
        assert_eq!(
            h.summary(),
            None,
            "below the 50-sample floor the day must publish −1 sentinels, \
             never a fabricated distribution (Rule 11)"
        );
        h.record_ns(1_000_000_000);
        assert!(h.summary().is_some(), "at the floor the summary publishes");
        h.reset();
        assert_eq!(h.summary(), None, "the IST-midnight reset clears the day");
        assert_eq!(h.max_ms.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_reset_day_lag_histogram_and_day_lag_summary_route_to_the_right_feed() {
        // The pub wrappers must route Groww→Groww (and never clear Dhan).
        // Uses the process-global statics with relative assertions only —
        // sibling tests may also touch them.
        for _ in 0..MIN_LAG_SAMPLES {
            GROWW_DAY_LAG_HIST.record_ns(180 * NANOS_PER_MS as u64);
        }
        let s = day_lag_summary(Feed::Groww).expect("groww day summary");
        assert!(s.samples >= i64::try_from(MIN_LAG_SAMPLES).unwrap_or(i64::MAX));
        assert!(
            s.p50_ms > 0 && s.p50_ms < 1_000,
            "Groww ms-precision keeps a 180 ms median sub-second: {}",
            s.p50_ms
        );
        reset_day_lag_histogram(Feed::Groww);
        assert_eq!(
            day_lag_summary(Feed::Groww),
            None,
            "reset must clear the Groww day histogram"
        );
    }
}
