//! Wave 2 Item 8 — tick-gap detector (G4 + G19 + I-P1-11).
//!
//! Tracks the last-seen wall-clock instant per `(security_id, exchange_segment)`
//! composite key — composite per the I-P1-11 uniqueness rule. When a tick
//! has been silent for ≥ `threshold_secs` it is reported in the next
//! coalesced summary (G4 — never per-instrument Telegram spam).
//!
//! Resets the entire map at 15:35 IST daily (G19) so that overnight
//! quietness does NOT count as a tick gap on next day's market open.

use std::sync::Arc;
use std::time::Instant;

use papaya::HashMap as PapayaHashMap;
use tickvault_common::types::ExchangeSegment;

/// Default per-instrument silence threshold before the gap is flagged.
pub const TICK_GAP_THRESHOLD_SECS_DEFAULT: u64 = 30;
/// Default coalescing window for Telegram summary emission.
pub const TICK_GAP_COALESCE_WINDOW_SECS_DEFAULT: u64 = 60;

/// Wave-2-D — bounded cap for the coalesce-task Telegram digest. Even
/// when the entire universe is silent, the top-N filter only allocates
/// for this many entries.
pub const TICK_GAP_TOP_N_DEFAULT: usize = 64;

/// Composite key per I-P1-11. Wave 2 Item 8.
pub type TickGapKey = (u32, ExchangeSegment);

// Wave-2-D Fix 4 — hot-path `Copy` compile-time ratchet. The
// `record_tick` call is on the tick-processor hot path and builds
// `TickGapKey` by value (no allocation). If `ExchangeSegment` ever
// loses `Copy` (e.g. someone changes it from an enum to a `String`),
// every `record_tick` call would silently start allocating. This
// const block is a compile-time guard: the closure body fails to
// type-check if either type is not `Copy`. Hand-rolled because
// `static_assertions` is not in workspace Cargo.toml.
const _: fn() = || {
    const fn assert_copy<T: Copy>() {}
    assert_copy::<ExchangeSegment>();
    assert_copy::<TickGapKey>();
};

/// O(1) tick-gap detector.
///
/// `record_tick` is the hot-path call (≤ 50ns p99 target — pure papaya
/// pin + insert; no allocation). `scan_gaps` is the cold-path call
/// (every 60s from a coalescing background task) that walks the map and
/// returns the list of currently-silent instruments.
#[derive(Debug, Default)]
pub struct TickGapDetector {
    /// Last-seen wall-clock instant per `(security_id, segment)` — the
    /// composite key satisfies I-P1-11.
    last_seen: PapayaHashMap<TickGapKey, Instant>,
    /// Per-instrument silence threshold in seconds. Configurable via
    /// `config/base.toml` `tick_gap_threshold_seconds`.
    threshold_secs: u64,
}

impl TickGapDetector {
    /// Constructs a detector with the given silence threshold (seconds).
    #[must_use]
    pub fn new(threshold_secs: u64) -> Self {
        Self {
            last_seen: PapayaHashMap::new(),
            threshold_secs,
        }
    }

    /// Records a tick observation. Hot-path — must remain O(1).
    ///
    /// Wave 2 Item 8: this is the ONLY method that should be called
    /// from the tick-processor hot loop. `scan_gaps` is the cold path.
    pub fn record_tick(&self, security_id: u32, segment: ExchangeSegment, now: Instant) {
        let pin = self.last_seen.pin();
        // O(1) EXEMPT: papaya insert is amortised constant; the map is
        // bounded by the universe size (~25K entries) and never grows
        // beyond that during a single trading day.
        pin.insert((security_id, segment), now);
    }

    /// Returns the list of `(security_id, segment, gap_secs)` for every
    /// instrument silent ≥ `threshold_secs`. Cold path — called once
    /// per coalesce window from a background task.
    ///
    /// Sorted by `gap_secs` descending (largest gap first) for
    /// operator-friendly Telegram digest.
    ///
    /// **Allocation note (Wave-2-D adversarial review finding (e)):**
    /// when the entire universe (~25 K instruments) is silent
    /// (Dhan-side outage, network blip), this returns a Vec with up to
    /// 25 K entries (~600 KB once per 60s). For pure summary purposes
    /// the coalesce task only needs the top-N — see `scan_gaps_top_n`
    /// for the bounded variant.
    #[must_use]
    pub fn scan_gaps(&self, now: Instant) -> Vec<(u32, ExchangeSegment, u64)> {
        let pin = self.last_seen.pin();
        let mut out: Vec<(u32, ExchangeSegment, u64)> = pin
            .iter()
            .filter_map(|(key, last)| {
                let gap = now.saturating_duration_since(*last).as_secs();
                if gap >= self.threshold_secs {
                    Some((key.0, key.1, gap))
                } else {
                    None
                }
            })
            .collect();
        // O(n log n) where n = gap count (NOT total universe). Always
        // small in practice (< 100). Sort largest-gap-first.
        out.sort_by(|a, b| b.2.cmp(&a.2));
        out
    }

    /// Wave-2-D Fix 4 — bounded variant of `scan_gaps` that returns at
    /// most `cap` entries (the top-`cap` largest gaps) plus the total
    /// silent count.
    ///
    /// Bounded allocation: O(`cap`) regardless of how many instruments
    /// are silent. Used by the 60s coalesce task so a universe-wide
    /// silence event cannot allocate 600 KB on every tick of the
    /// coalesce timer.
    ///
    /// Returns `(top_cap_entries, total_silent_count)`. The full count
    /// drives the `tv_tick_gap_instruments_silent` gauge; the top-N
    /// entries drive the Telegram digest.
    #[must_use]
    pub fn scan_gaps_top_n(
        &self,
        now: Instant,
        cap: usize,
    ) -> (Vec<(u32, ExchangeSegment, u64)>, usize) {
        if cap == 0 {
            // Still need the total count for the gauge — walk without
            // collecting.
            let pin = self.last_seen.pin();
            let total = pin
                .iter()
                .filter(|(_, last)| {
                    now.saturating_duration_since(**last).as_secs() >= self.threshold_secs
                })
                .count();
            return (Vec::new(), total);
        }
        let pin = self.last_seen.pin();
        // Single-pass top-K via a min-heap pruned to size `cap`. O(n
        // log cap) time, O(cap) space. Bounded regardless of universe
        // size. The segment is stored as a `u8` (binary_code) so the
        // heap key is fully `Ord` without requiring Ord on
        // ExchangeSegment itself.
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut heap: BinaryHeap<Reverse<(u64, u32, u8)>> =
            BinaryHeap::with_capacity(cap.saturating_add(1));
        let mut total: usize = 0;
        for (key, last) in pin.iter() {
            let gap = now.saturating_duration_since(*last).as_secs();
            if gap < self.threshold_secs {
                continue;
            }
            total = total.saturating_add(1);
            let seg_code = key.1.binary_code();
            if heap.len() < cap {
                heap.push(Reverse((gap, key.0, seg_code)));
            } else if let Some(Reverse((min_gap, _, _))) = heap.peek() {
                if gap > *min_gap {
                    heap.pop();
                    heap.push(Reverse((gap, key.0, seg_code)));
                }
            }
        }
        // Drain into a sorted (largest-first) Vec. `into_sorted_vec` on
        // a `BinaryHeap<Reverse<T>>` returns elements in ascending
        // `Reverse<T>` order — i.e. largest underlying T first. So the
        // resulting Vec is already largest-gap-first; no reverse
        // needed. Decode the u8 segment back to ExchangeSegment via
        // `from_byte`; on the (impossible) None path we skip the entry
        // rather than panic per `dhan-annexure-enums.md` Rule 15.
        let out: Vec<(u32, ExchangeSegment, u64)> = heap
            .into_sorted_vec()
            .into_iter()
            .filter_map(|Reverse((gap, id, seg_code))| {
                ExchangeSegment::from_byte(seg_code).map(|seg| (id, seg, gap))
            })
            .collect();
        (out, total)
    }

    /// Daily reset (G19). Called from a 15:35 IST scheduled task so that
    /// overnight silence (16:00 → next 09:15) does NOT register as a
    /// tick-gap when the market reopens.
    pub fn reset_daily(&self) {
        let pin = self.last_seen.pin();
        // O(n) where n = universe size. Only runs once per day.
        // O(1) EXEMPT: cold-path daily housekeeping.
        pin.clear();
    }

    /// Number of entries currently tracked. For the
    /// `tv_tick_gap_map_entries` Prometheus gauge.
    #[must_use]
    pub fn len(&self) -> usize {
        self.last_seen.pin().len()
    }

    /// True when no instruments have been recorded since boot/reset.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Shared handle for the tick-gap detector (zero-copy clone via `Arc`).
pub type SharedTickGapDetector = Arc<TickGapDetector>;

/// Wave 2 Item 8 — global tick-gap detector handle. Set once at boot
/// via `set_global_tick_gap_detector()`. The hot-path call site
/// `record_tick_global()` is a no-op when no detector is installed
/// (preserves test behaviour and avoids paying for the lookup if the
/// feature is disabled).
static GLOBAL_TICK_GAP_DETECTOR: std::sync::OnceLock<SharedTickGapDetector> =
    std::sync::OnceLock::new();

/// Install the global tick-gap detector. Idempotent — second call is a
/// no-op. Returns `true` on first install, `false` otherwise. Call once
/// at boot from `main.rs` BEFORE the tick processor starts.
pub fn set_global_tick_gap_detector(detector: SharedTickGapDetector) -> bool {
    GLOBAL_TICK_GAP_DETECTOR.set(detector).is_ok()
}

/// Hot-path entry point. Records a tick observation against the global
/// detector if one is installed; otherwise, no-op.
///
/// O(1) — single OnceLock read + one papaya insert. Safe to call from
/// the tick processor's hot loop.
#[inline]
pub fn record_tick_global(security_id: u32, segment: ExchangeSegment, now: Instant) {
    if let Some(d) = GLOBAL_TICK_GAP_DETECTOR.get() {
        d.record_tick(security_id, segment, now);
    }
}

/// Read-only accessor for the global detector. Returns `None` if no
/// detector has been installed yet. Used by the 60s coalescing task.
#[must_use]
pub fn global_tick_gap_detector() -> Option<&'static SharedTickGapDetector> {
    GLOBAL_TICK_GAP_DETECTOR.get()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tickvault_common::types::ExchangeSegment;

    #[test]
    fn test_detector_new_default_threshold() {
        let d = TickGapDetector::new(TICK_GAP_THRESHOLD_SECS_DEFAULT);
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn test_record_tick_then_scan_returns_no_gaps_when_fresh() {
        let d = TickGapDetector::new(30);
        let now = Instant::now();
        d.record_tick(13, ExchangeSegment::IdxI, now);
        d.record_tick(25, ExchangeSegment::IdxI, now);
        // Same instant → 0s gap → no entries returned (gap < threshold).
        let gaps = d.scan_gaps(now);
        assert!(gaps.is_empty());
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn test_scan_gaps_returns_silent_instruments_after_threshold() {
        let d = TickGapDetector::new(30);
        let t0 = Instant::now();
        d.record_tick(13, ExchangeSegment::IdxI, t0);
        // Advance "now" by 31s past last-seen.
        let t1 = t0 + Duration::from_secs(31);
        let gaps = d.scan_gaps(t1);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].0, 13);
        assert_eq!(gaps[0].1, ExchangeSegment::IdxI);
        assert!(gaps[0].2 >= 31);
    }

    #[test]
    fn test_scan_gaps_sorted_largest_gap_first() {
        let d = TickGapDetector::new(30);
        let t_base = Instant::now();
        d.record_tick(13, ExchangeSegment::IdxI, t_base);
        d.record_tick(25, ExchangeSegment::IdxI, t_base + Duration::from_secs(30));
        d.record_tick(51, ExchangeSegment::IdxI, t_base + Duration::from_secs(60));
        // Now scan at +90s — gaps are 90, 60, 30.
        let now = t_base + Duration::from_secs(90);
        let gaps = d.scan_gaps(now);
        assert_eq!(gaps.len(), 3);
        // Sorted largest-first
        assert!(gaps[0].2 >= gaps[1].2);
        assert!(gaps[1].2 >= gaps[2].2);
        assert_eq!(gaps[0].0, 13); // 90s gap
        assert_eq!(gaps[2].0, 51); // 30s gap
    }

    #[test]
    fn test_composite_key_distinguishes_same_id_different_segments() {
        // I-P1-11: security_id 27 may appear in both IDX_I (FINNIFTY) and
        // NSE_EQ — the map MUST keep both as distinct entries.
        let d = TickGapDetector::new(30);
        let now = Instant::now();
        d.record_tick(27, ExchangeSegment::IdxI, now);
        d.record_tick(27, ExchangeSegment::NseEquity, now);
        assert_eq!(d.len(), 2, "composite key must keep both segments");
    }

    #[test]
    fn test_reset_daily_clears_all_entries() {
        let d = TickGapDetector::new(30);
        let now = Instant::now();
        d.record_tick(13, ExchangeSegment::IdxI, now);
        d.record_tick(25, ExchangeSegment::IdxI, now);
        assert_eq!(d.len(), 2);
        d.reset_daily();
        assert!(d.is_empty());
    }

    #[test]
    fn test_set_global_tick_gap_detector_is_idempotent() {
        // First call may or may not succeed (other tests in the same
        // binary may have already installed). Either way the second
        // call must not also succeed.
        let d = Arc::new(TickGapDetector::new(30));
        let first = set_global_tick_gap_detector(d.clone());
        let second = set_global_tick_gap_detector(d);
        assert!(
            !(first && second),
            "set_global_tick_gap_detector must be idempotent"
        );
    }

    #[test]
    fn test_record_tick_global_is_safe_when_no_detector_installed() {
        // The hot-path call site must NOT panic when no detector is
        // installed (test binaries that don't call set_*).
        record_tick_global(13, ExchangeSegment::IdxI, Instant::now());
    }

    #[test]
    fn test_global_tick_gap_detector_accessor_returns_option() {
        // Type-check the accessor signature.
        let _: Option<&'static SharedTickGapDetector> = global_tick_gap_detector();
    }

    #[test]
    fn test_scan_gaps_top_n_zero_cap_returns_empty_vec_with_total_count() {
        let d = TickGapDetector::new(30);
        let t0 = Instant::now();
        for id in 0u32..5 {
            d.record_tick(id, ExchangeSegment::IdxI, t0);
        }
        // Advance 60s — all 5 entries are silent.
        let now = t0 + Duration::from_secs(60);
        let (entries, total) = d.scan_gaps_top_n(now, 0);
        assert!(entries.is_empty());
        assert_eq!(total, 5);
    }

    #[test]
    fn test_scan_gaps_top_n_caps_returned_entries() {
        let d = TickGapDetector::new(30);
        let t0 = Instant::now();
        // Record 100 instruments silent for varying durations.
        for id in 0u32..100 {
            d.record_tick(id, ExchangeSegment::IdxI, t0);
        }
        let now = t0 + Duration::from_secs(60);
        let (entries, total) = d.scan_gaps_top_n(now, 10);
        // total must reflect the FULL silent count even when capped.
        assert_eq!(total, 100);
        // entries must be capped at 10.
        assert!(entries.len() <= 10);
    }

    #[test]
    fn test_scan_gaps_top_n_returns_largest_gaps_first() {
        let d = TickGapDetector::new(30);
        let t_base = Instant::now();
        // 5 entries with different last-seen times → different gaps.
        d.record_tick(1, ExchangeSegment::IdxI, t_base);
        d.record_tick(2, ExchangeSegment::IdxI, t_base + Duration::from_secs(30));
        d.record_tick(3, ExchangeSegment::IdxI, t_base + Duration::from_secs(60));
        d.record_tick(4, ExchangeSegment::IdxI, t_base + Duration::from_secs(90));
        d.record_tick(5, ExchangeSegment::IdxI, t_base + Duration::from_secs(120));
        // Now scan at t_base + 150s. Gaps: 150, 120, 90, 60, 30.
        let now = t_base + Duration::from_secs(150);
        let (entries, total) = d.scan_gaps_top_n(now, 3);
        assert_eq!(total, 5);
        assert_eq!(entries.len(), 3);
        // Largest-first order.
        assert!(entries[0].2 >= entries[1].2);
        assert!(entries[1].2 >= entries[2].2);
        // Top-3 should be ids 1, 2, 3 (gaps 150, 120, 90).
        assert_eq!(entries[0].0, 1);
    }

    #[test]
    fn test_scan_gaps_top_n_bounded_when_universe_silent() {
        // Wave-2-D Fix 4 — the killer test: 5000 silent instruments,
        // cap=64. Returned Vec must be at most 64. This is the
        // operator-visible "universe-wide silence" scenario.
        let d = TickGapDetector::new(30);
        let t0 = Instant::now();
        for id in 0u32..5000 {
            d.record_tick(id, ExchangeSegment::IdxI, t0);
        }
        let now = t0 + Duration::from_secs(60);
        let (entries, total) = d.scan_gaps_top_n(now, TICK_GAP_TOP_N_DEFAULT);
        assert_eq!(total, 5000);
        assert!(entries.len() <= TICK_GAP_TOP_N_DEFAULT);
    }

    #[test]
    fn test_scan_gaps_top_n_skips_fresh_instruments_below_threshold() {
        let d = TickGapDetector::new(30);
        let t0 = Instant::now();
        d.record_tick(1, ExchangeSegment::IdxI, t0);
        d.record_tick(2, ExchangeSegment::IdxI, t0);
        // 10s later — no gaps yet.
        let now = t0 + Duration::from_secs(10);
        let (entries, total) = d.scan_gaps_top_n(now, 10);
        assert!(entries.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn test_tick_gap_top_n_default_is_64() {
        // Pin the constant — a future "let's bump to 1024" PR is
        // forced to update tests + dashboards together.
        assert_eq!(TICK_GAP_TOP_N_DEFAULT, 64);
    }

    #[test]
    fn test_record_tick_updates_existing_entry_in_place() {
        // Re-recording the same key replaces the previous Instant —
        // the map size stays at 1.
        let d = TickGapDetector::new(30);
        let t0 = Instant::now();
        d.record_tick(13, ExchangeSegment::IdxI, t0);
        let t1 = t0 + Duration::from_secs(10);
        d.record_tick(13, ExchangeSegment::IdxI, t1);
        assert_eq!(d.len(), 1);
        // Scan at t0+45 — gap from t1 is only 35s, so still flagged
        // (>=30); from t0 it would be 45.
        let now = t0 + Duration::from_secs(45);
        let gaps = d.scan_gaps(now);
        assert_eq!(gaps.len(), 1);
        assert!(gaps[0].2 >= 30);
        assert!(gaps[0].2 <= 45);
    }
}
