//! Wave 5 Item 26 L1 — Volume Semantic Guarantee Layer (in-memory L1).
//!
//! Per Dhan Ticket #5525125 + the Cowork-verified evidence (Track 1 + 3 + 4
//! strongly suggesting cumulative; Track 2 to be confirmed by the Mon May 4
//! 09:45 IST monotonicity SELECT — see
//! `docs/operator/track-2-monotonicity-select.md`), the `volume` field at
//! bytes 22-25 of the Quote packet (and bytes 22-25 of the Full packet) is
//! a **cumulative-day total** since session open, not per-tick incremental.
//!
//! L1 invariant: within a single IST trading day, for each
//! `(security_id, exchange_segment)` pair, `volume[i] >= volume[i-1]` —
//! cumulative volume can only go up (or stay flat) until session close.
//!
//! When the invariant breaks, this guard:
//! - Emits ERROR with `code = ErrorCode::Volume01MonotonicityBreach.code_str()`
//!   so the tag-guard meta-test routes it to Telegram via Loki.
//! - Increments `tv_volume_monotonicity_breaches_total{segment}`.
//! - Returns `Breach { previous, current, delta }` so the caller can
//!   record the offending tick into an audit table for the Item 26 L3
//!   Dhan support email.
//!
//! Hot-path constraints:
//! - O(1) per tick: single HashMap lookup + integer compare.
//! - Composite key `(u32, u8)` per I-P1-11 — DO NOT regress.
//! - Self-resets at IST midnight via `clear()` (caller schedules).
//!
//! What L1 does NOT do (deferred to follow-up sub-PR):
//! - L2 NSE bhavcopy daily cross-check (`docs/operator/track-2-monotonicity-select.md`
//!   describes the runbook; the bhavcopy fetcher + crosscheck persistence
//!   are not yet implemented).
//! - L3 Dhan support escalation email — already drafted via PR #414;
//!   operator must Gmail-send the GitHub link.

use std::collections::HashMap;

use tickvault_common::error_code::ErrorCode;
use tracing::error;

/// Verdict returned by `VolumeMonotonicityGuard::observe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeObservation {
    /// First tick this session for the (security_id, segment) — baseline
    /// stored, no comparison possible.
    BaselineSet { volume: u32 },
    /// Tick volume is monotonically non-decreasing — invariant holds.
    Monotonic { previous: u32, current: u32 },
    /// Tick volume DECREASED vs the last observed value — VOLUME-MONO-01.
    Breach { previous: u32, current: u32 },
}

/// In-memory per-(security_id, segment) last-seen cumulative volume tracker.
/// Caller must `clear()` at IST midnight (typically wired to the same
/// scheduled task that drains other midnight resets).
pub struct VolumeMonotonicityGuard {
    /// Composite key (I-P1-11) — DO NOT regress to single-u32 key.
    last_seen: HashMap<(u64, u8), u32>,
    /// Cumulative breaches observed since the last `clear()`. Exposed as
    /// the canonical Prom counter `tv_volume_monotonicity_breaches_total`
    /// in production (the in-memory copy here is for tests + diagnostics).
    breach_count: u64,
}

impl Default for VolumeMonotonicityGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl VolumeMonotonicityGuard {
    /// Pre-allocates space for the typical Wave 5 indices-only universe
    /// (~11,018 instruments). Slight over-allocation is fine — bounded
    /// memory, no growth on the hot path.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_seen: HashMap::with_capacity(12_000),
            breach_count: 0,
        }
    }

    /// Observes a new cumulative-volume reading for the given instrument.
    ///
    /// Returns the verdict:
    /// - `BaselineSet` on the first tick this session
    /// - `Monotonic` when `volume >= previous`
    /// - `Breach` when `volume < previous` — also emits ERROR + bumps
    ///   the internal breach counter.
    ///
    /// O(1). Hot-path safe.
    #[inline]
    pub fn observe(
        &mut self,
        security_id: u64,
        exchange_segment_code: u8,
        volume: u32,
    ) -> VolumeObservation {
        let key = (security_id, exchange_segment_code);
        match self.last_seen.get(&key) {
            None => {
                self.last_seen.insert(key, volume);
                VolumeObservation::BaselineSet { volume }
            }
            Some(&previous) if volume >= previous => {
                self.last_seen.insert(key, volume);
                VolumeObservation::Monotonic {
                    previous,
                    current: volume,
                }
            }
            Some(&previous) => {
                // Breach: volume went DOWN within the trading day.
                self.breach_count = self.breach_count.saturating_add(1);
                error!(
                    code = ErrorCode::Volume01MonotonicityBreach.code_str(),
                    security_id,
                    segment = exchange_segment_code,
                    previous,
                    current = volume,
                    delta = (previous as i64).saturating_sub(volume as i64),
                    "VOLUME-MONO-01: cumulative volume decreased mid-session — \
                     either Dhan changed semantic or parser regressed on byte \
                     offset. See docs/operator/track-2-monotonicity-select.md."
                );
                // Keep `last_seen` UNCHANGED so subsequent ticks compare
                // against the previous high-water mark, not the breach
                // value. Otherwise a single decrease would silently reset
                // the baseline.
                VolumeObservation::Breach {
                    previous,
                    current: volume,
                }
            }
        }
    }

    /// Resets the per-instrument map. Call at IST midnight.
    pub fn clear(&mut self) {
        self.last_seen.clear();
        // breach_count carries forward across days — operators want the
        // historical total for audit. Reset only via `reset_breach_count`.
    }

    /// Total breaches observed since last `reset_breach_count`.
    #[must_use]
    pub const fn breach_count(&self) -> u64 {
        self.breach_count
    }

    /// Resets the breach counter. Used by the periodic exporter that
    /// reads the value into a Prom counter (the counter has its own
    /// monotonic semantics — this struct is the in-memory mirror).
    // TEST-EXEMPT: covered by `test_clear_resets_per_instrument_state_but_preserves_breach_history` which exercises the call.
    pub fn reset_breach_count(&mut self) {
        self.breach_count = 0;
    }

    /// Number of (security_id, segment) pairs currently tracked. Useful
    /// for sanity checks (should match the subscribed universe size).
    #[must_use]
    // TEST-EXEMPT: covered by `test_first_tick_sets_baseline_no_breach` and `test_segment_isolation_per_i_p1_11` which assert tracked_count.
    pub fn tracked_count(&self) -> usize {
        self.last_seen.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_tick_sets_baseline_no_breach() {
        let mut g = VolumeMonotonicityGuard::new();
        let v = g.observe(13, 0, 1_000);
        assert_eq!(v, VolumeObservation::BaselineSet { volume: 1_000 });
        assert_eq!(g.breach_count(), 0);
        assert_eq!(g.tracked_count(), 1);
    }

    #[test]
    fn test_monotonic_increase_passes() {
        let mut g = VolumeMonotonicityGuard::new();
        let _ = g.observe(13, 0, 1_000);
        let v = g.observe(13, 0, 1_500);
        assert_eq!(
            v,
            VolumeObservation::Monotonic {
                previous: 1_000,
                current: 1_500
            }
        );
        assert_eq!(g.breach_count(), 0);
    }

    #[test]
    fn test_flat_volume_passes_as_monotonic() {
        // Cumulative volume staying the same (no new trades) is monotonic.
        let mut g = VolumeMonotonicityGuard::new();
        let _ = g.observe(13, 0, 1_000);
        let v = g.observe(13, 0, 1_000);
        assert_eq!(
            v,
            VolumeObservation::Monotonic {
                previous: 1_000,
                current: 1_000
            }
        );
        assert_eq!(g.breach_count(), 0);
    }

    #[test]
    fn test_decrease_emits_breach_and_increments_counter() {
        let mut g = VolumeMonotonicityGuard::new();
        let _ = g.observe(49_001, 2, 5_000);
        let v = g.observe(49_001, 2, 4_000);
        assert_eq!(
            v,
            VolumeObservation::Breach {
                previous: 5_000,
                current: 4_000
            }
        );
        assert_eq!(g.breach_count(), 1);

        // Subsequent ticks compare against the previous HIGH (5,000), not
        // the breach value (4,000) — so another tick at 4,500 is still
        // a breach (4,500 < 5,000).
        let v2 = g.observe(49_001, 2, 4_500);
        assert_eq!(
            v2,
            VolumeObservation::Breach {
                previous: 5_000,
                current: 4_500
            }
        );
        assert_eq!(g.breach_count(), 2);

        // A tick at 6,000 (above the high) recovers — back to monotonic.
        let v3 = g.observe(49_001, 2, 6_000);
        assert_eq!(
            v3,
            VolumeObservation::Monotonic {
                previous: 5_000,
                current: 6_000
            }
        );
        assert_eq!(g.breach_count(), 2);
    }

    #[test]
    fn test_clear_resets_per_instrument_state_but_preserves_breach_history() {
        let mut g = VolumeMonotonicityGuard::new();
        let _ = g.observe(13, 0, 1_000);
        let _ = g.observe(13, 0, 500); // breach
        assert_eq!(g.breach_count(), 1);
        g.clear();
        // After clear, the next tick is a new baseline.
        let v = g.observe(13, 0, 200);
        assert_eq!(v, VolumeObservation::BaselineSet { volume: 200 });
        // Breach history persists for audit.
        assert_eq!(g.breach_count(), 1);
        // Can be reset explicitly.
        g.reset_breach_count();
        assert_eq!(g.breach_count(), 0);
    }

    /// I-P1-11 ratchet: composite (security_id, segment) key keeps SENSEX
    /// (BSE_FNO=8) ticks isolated from NIFTY (NSE_FNO=2) under the
    /// indices-only Wave 5 universe.
    #[test]
    fn test_segment_isolation_per_i_p1_11() {
        let mut g = VolumeMonotonicityGuard::new();
        // NSE_FNO id=49001 baseline at 1,000.
        let _ = g.observe(49_001, 2, 1_000);
        // BSE_FNO id=49001 — SAME numeric id, DIFFERENT segment. Treated
        // as a separate instrument, so first observation is BaselineSet.
        let v = g.observe(49_001, 8, 100);
        assert_eq!(v, VolumeObservation::BaselineSet { volume: 100 });
        // NSE_FNO 49001 still tracked at 1,000; tick at 800 is a breach.
        let v2 = g.observe(49_001, 2, 800);
        assert_eq!(
            v2,
            VolumeObservation::Breach {
                previous: 1_000,
                current: 800
            }
        );
        assert_eq!(g.tracked_count(), 2);
    }
}
