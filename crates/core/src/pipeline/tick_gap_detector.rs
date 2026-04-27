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

/// Composite key per I-P1-11. Wave 2 Item 8.
pub type TickGapKey = (u32, ExchangeSegment);

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
