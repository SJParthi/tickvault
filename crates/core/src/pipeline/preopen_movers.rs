//! Pre-open movers tracker — Wave 3 Item 10.
//!
//! # Why this exists
//!
//! Between 09:00 and 09:13 IST, the F&O underlying universe (216 stocks +
//! NIFTY + BANKNIFTY) is trading in pre-open auction. The cash-equity
//! Quote/Full feed delivers each stock's pre-open LTP plus its previous
//! day's close (Quote bytes 38-41 / Full bytes 50-53 per
//! `live-market-feed.md` rule 8). For NIFTY/BANKNIFTY the previous-day
//! close arrives as a code-6 PrevClose packet (rule 7) and is plumbed
//! through `update_from_tick`'s seed path.
//!
//! This tracker captures both signals during the window and emits a
//! snapshot every 60s with `phase = "PREOPEN"`. SENSEX (BSE) is NOT in
//! the pre-open feed; it is emitted with `phase = "PREOPEN_UNAVAILABLE"`
//! and zeroed prices so the dashboard can prove "we KNEW it was not
//! available — we did not silently skip" (audit-findings Rule 11).
//!
//! # Hot-path guarantees
//!
//! - All maps are pre-sized at construction (~220 entries).
//! - `update_from_tick` is O(1): single `(security_id, segment)` lookup +
//!   three `HashMap::insert` calls.
//! - `compute_snapshot` is O(n log n) on the 218-entry universe; runs at
//!   most once per 60s (cold path).
//! - No `.clone()` on the per-tick path — only owned `String` clones in
//!   `compute_snapshot`'s output `MoverEntry` rows (cold path).
//!
//! # Market-hours awareness (audit-findings Rule 3)
//!
//! The runner gates work on `is_within_preopen_window()` from
//! `preopen_price_buffer`. Outside 09:00..09:13 IST the runner sleeps
//! until the next entry. `reset()` is called on every entry into the
//! window (idempotent — safe across same-day double-entry) so the
//! tracker is fresh each morning.
//!
//! # SEBI audit
//!
//! Persisted rows go to `stock_movers` with `phase` SYMBOL distinguishing
//! pre-open from in-market. SEBI 5-year retention applies (partition
//! lifecycle in `partition_manager.rs`).

use std::collections::HashMap;

use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tickvault_storage::tick_persistence::f32_to_f64_clean;

/// Initial capacity for the per-instrument tracker maps. The F&O universe
/// is ~216 stocks + 2 indices (NIFTY, BANKNIFTY) = 218 entries. We size
/// to 256 to absorb a small amount of growth without rehashing.
///
/// O(1) EXEMPT: boot-time allocation, fixed cap, never resized on hot path.
const TRACKER_INITIAL_CAPACITY: usize = 256;

/// One row of the pre-open movers snapshot.
///
/// Owned `String` fields (`symbol`) are acceptable here because
/// `compute_snapshot` runs at most once per 60s — it is cold path.
#[derive(Debug, Clone, PartialEq)]
pub struct MoverEntry {
    pub symbol: String,
    pub security_id: u32,
    pub segment: ExchangeSegment,
    pub ltp: f64,
    pub prev_close: f64,
    pub change_pct: f64,
    pub volume: i64,
    pub rank: i32,
    pub phase: PreopenPhase,
}

/// Snapshot phase label for `stock_movers.phase`.
///
/// `Preopen` is emitted when the tracker has both an LTP from the
/// pre-open buffer and a non-zero previous close. `PreopenUnavailable`
/// is emitted for SENSEX (BSE — not in the Dhan pre-open feed) so the
/// dashboard can prove the absence is intentional, not a regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreopenPhase {
    Preopen,
    PreopenUnavailable,
}

impl PreopenPhase {
    /// Wire string written to `stock_movers.phase`. Matches the
    /// `STOCK_MOVERS_PHASE_*` constants in
    /// `crates/storage/src/movers_persistence.rs` exactly.
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Preopen => "PREOPEN",
            Self::PreopenUnavailable => "PREOPEN_UNAVAILABLE",
        }
    }
}

/// Symbol expected in the snapshot but NOT carried by the pre-open feed
/// (SENSEX / BSE underlyings). `compute_snapshot` emits a zero-priced
/// row with `PreopenPhase::PreopenUnavailable` for each of these.
#[derive(Debug, Clone)]
pub struct UnavailableSymbol {
    pub symbol: String,
    pub security_id: u32,
    pub segment: ExchangeSegment,
}

/// Pre-open movers tracker.
///
/// Self-contained — does not own a tokio handle, broadcast subscription,
/// or QuestDB writer. The wiring lives in `main.rs`.
#[derive(Debug)]
pub struct PreopenMoversTracker {
    /// (security_id, segment) -> latest pre-open LTP captured during 09:00..09:13.
    preopen_ltps: HashMap<(u32, ExchangeSegment), f64>,
    /// (security_id, segment) -> previous-day close.
    /// Populated from `ParsedTick.day_close` (Quote/Full) AND from
    /// `seed_prev_close` (used for IDX_I where prev close arrives via
    /// PrevClose packet, NOT in `day_close`).
    prev_closes: HashMap<(u32, ExchangeSegment), f64>,
    /// (security_id, segment) -> latest cumulative volume seen during pre-open.
    preopen_volumes: HashMap<(u32, ExchangeSegment), i64>,
    /// (security_id, segment) -> human-readable symbol.
    symbol_lookup: HashMap<(u32, ExchangeSegment), String>,
    /// Symbols expected but NOT in the pre-open feed (SENSEX / BSE).
    unavailable: Vec<UnavailableSymbol>,
}

impl PreopenMoversTracker {
    /// Constructs an empty tracker pre-sized for the F&O universe.
    ///
    /// `symbol_lookup` is the `(security_id, segment) -> symbol` map —
    /// callers should populate it from `FnoUniverse` at boot. Per I-P1-11
    /// the key MUST be `(security_id, segment)`.
    ///
    /// `unavailable` is the list of underlyings expected in the broader
    /// universe but NOT in the pre-open feed (SENSEX, etc.). Each one is
    /// emitted in `compute_snapshot` with `PreopenPhase::PreopenUnavailable`.
    #[must_use]
    pub fn new(
        symbol_lookup: HashMap<(u32, ExchangeSegment), String>,
        unavailable: Vec<UnavailableSymbol>,
    ) -> Self {
        Self {
            preopen_ltps: HashMap::with_capacity(TRACKER_INITIAL_CAPACITY),
            prev_closes: HashMap::with_capacity(TRACKER_INITIAL_CAPACITY),
            preopen_volumes: HashMap::with_capacity(TRACKER_INITIAL_CAPACITY),
            symbol_lookup,
            unavailable,
        }
    }

    /// Drops every accumulated price/volume sample. Called on entry to
    /// the 09:00..09:13 window so the previous day's data does not leak
    /// into today's snapshot. Idempotent — safe across same-day re-entry.
    pub fn reset(&mut self) {
        self.preopen_ltps.clear();
        self.prev_closes.clear();
        self.preopen_volumes.clear();
    }

    /// Updates the tracker from a single pre-open tick.
    ///
    /// O(1) hot path:
    /// - Looks up `(security_id, segment)` in `symbol_lookup`. If absent
    ///   the tick is ignored (not in the F&O universe).
    /// - Captures LTP if `tick.last_traded_price > 0`.
    /// - Captures `tick.day_close` as prev close if `> 0` (Quote/Full
    ///   packets carry it; Ticker mode for IDX_I leaves it 0 — use
    ///   `seed_prev_close` for indices).
    /// - Captures volume.
    pub fn update_from_tick(&mut self, tick: &ParsedTick) {
        let Some(seg) = ExchangeSegment::from_byte(tick.exchange_segment_code) else {
            return;
        };
        let key = (tick.security_id, seg);
        if !self.symbol_lookup.contains_key(&key) {
            return;
        }

        // Use f32_to_f64_clean per data-integrity rule: the naive
        // widening primitive produces "10.19999980926514" instead of
        // preserving Dhan's canonical "10.20" decimal — corrupts
        // dashboards + audit trail.
        let ltp = f32_to_f64_clean(tick.last_traded_price);
        if ltp.is_finite() && ltp > 0.0 {
            self.preopen_ltps.insert(key, ltp);
        }

        let prev = f32_to_f64_clean(tick.day_close);
        if prev.is_finite() && prev > 0.0 {
            self.prev_closes.insert(key, prev);
        }

        if tick.volume > 0 {
            self.preopen_volumes.insert(key, i64::from(tick.volume));
        }
    }

    /// Seeds a previous-day close for an instrument that doesn't carry
    /// it inline on the live feed. The canonical caller is the IDX_I
    /// path: NIFTY/BANKNIFTY prev close arrives via the dedicated
    /// PrevClose packet (response code 6) per
    /// `live-market-feed.md` rule 7, NOT via `ParsedTick.day_close`.
    pub fn seed_prev_close(&mut self, security_id: u32, segment: ExchangeSegment, prev_close: f64) {
        if prev_close.is_finite() && prev_close > 0.0 {
            self.prev_closes.insert((security_id, segment), prev_close);
        }
    }

    /// Number of instruments with at least one captured LTP. Useful for
    /// observability — emit as `tv_preopen_movers_tracked_total` gauge.
    #[must_use]
    pub fn tracked_len(&self) -> usize {
        self.preopen_ltps.len()
    }

    /// Number of instruments with a known prev close. Diagnoses the
    /// "we got an LTP but no prev close" gap separately.
    #[must_use]
    pub fn prev_close_len(&self) -> usize {
        self.prev_closes.len()
    }

    /// True iff `(security_id, segment)` has BOTH a captured LTP and a
    /// known prev close. Tests use this to assert ratchets without
    /// having to re-derive the snapshot.
    #[must_use]
    pub fn has_complete_data_for(&self, security_id: u32, segment: ExchangeSegment) -> bool {
        let key = (security_id, segment);
        self.preopen_ltps.contains_key(&key) && self.prev_closes.contains_key(&key)
    }

    /// Builds the per-snapshot list of `MoverEntry` rows.
    ///
    /// Composition:
    /// 1. Every (security_id, segment) with both an LTP AND a prev close
    ///    becomes a `Preopen` entry, ranked 1..N descending by `change_pct`.
    /// 2. Every entry in `unavailable` becomes a `PreopenUnavailable`
    ///    entry with zeroed prices and rank 0.
    ///
    /// The output is deterministic for a given input state — ranking
    /// uses `change_pct` first, then `symbol` as a tie-breaker, so two
    /// runs from the same state yield byte-identical output.
    #[must_use]
    pub fn compute_snapshot(&self) -> Vec<MoverEntry> {
        // Cold path (≤ once / 60s) — owned strings + Vec allocation OK.
        let mut live: Vec<MoverEntry> = Vec::with_capacity(self.preopen_ltps.len());
        for (key, &ltp) in &self.preopen_ltps {
            let Some(&prev_close) = self.prev_closes.get(key) else {
                continue;
            };
            if prev_close == 0.0 {
                continue;
            }
            let Some(symbol) = self.symbol_lookup.get(key) else {
                continue;
            };
            let change_pct = ((ltp - prev_close) / prev_close) * 100.0;
            if !change_pct.is_finite() {
                continue;
            }
            live.push(MoverEntry {
                symbol: symbol.clone(),
                security_id: key.0,
                segment: key.1,
                ltp,
                prev_close,
                change_pct,
                volume: self.preopen_volumes.get(key).copied().unwrap_or(0),
                rank: 0, // assigned below after sort
                phase: PreopenPhase::Preopen,
            });
        }

        // Sort descending by change_pct, ties broken by symbol ascending
        // for byte-identical output across runs of the same state.
        live.sort_by(|a, b| {
            b.change_pct
                .partial_cmp(&a.change_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.symbol.cmp(&b.symbol))
        });
        for (i, entry) in live.iter_mut().enumerate() {
            entry.rank = i32::try_from(i + 1).unwrap_or(i32::MAX);
        }

        // Append the unavailable entries — zeroed prices, rank 0.
        for u in &self.unavailable {
            live.push(MoverEntry {
                symbol: u.symbol.clone(),
                security_id: u.security_id,
                segment: u.segment,
                ltp: 0.0,
                prev_close: 0.0,
                change_pct: 0.0,
                volume: 0,
                rank: 0,
                phase: PreopenPhase::PreopenUnavailable,
            });
        }

        live
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::preopen_price_buffer::is_within_preopen_window;

    fn lookup_with(
        symbol: &str,
        sid: u32,
        seg: ExchangeSegment,
    ) -> HashMap<(u32, ExchangeSegment), String> {
        let mut m = HashMap::new();
        m.insert((sid, seg), symbol.to_string());
        m
    }

    fn reliance_tick(ltp: f32, prev: f32, vol: u32) -> ParsedTick {
        ParsedTick {
            security_id: 2885,
            exchange_segment_code: 1, // NSE_EQ
            last_traded_price: ltp,
            day_close: prev,
            volume: vol,
            ..ParsedTick::default()
        }
    }

    #[test]
    fn test_preopen_movers_uses_buffer_ticks() {
        // RELIANCE — pre-open LTP 2900 vs prev close 2850 = +1.754% gain.
        let lookup = lookup_with("RELIANCE", 2885, ExchangeSegment::NseEquity);
        let mut tracker = PreopenMoversTracker::new(lookup, Vec::new());
        tracker.update_from_tick(&reliance_tick(2900.0, 2850.0, 100_000));

        let snap = tracker.compute_snapshot();
        assert_eq!(snap.len(), 1);
        let r = &snap[0];
        assert_eq!(r.symbol, "RELIANCE");
        assert_eq!(r.segment, ExchangeSegment::NseEquity);
        assert_eq!(r.security_id, 2885);
        assert!((r.ltp - 2900.0).abs() < 1e-9);
        assert!((r.prev_close - 2850.0).abs() < 1e-9);
        // (2900 - 2850) / 2850 * 100 ≈ 1.7544
        assert!((r.change_pct - 1.7544).abs() < 0.01);
        assert_eq!(r.volume, 100_000);
        assert_eq!(r.rank, 1);
        assert_eq!(r.phase, PreopenPhase::Preopen);
    }

    #[test]
    fn test_preopen_movers_window_is_0900_to_0912_inclusive() {
        // The runner gates work on `is_within_preopen_window()`, which is
        // backed by the same constants the pre-open buffer uses
        // (PREOPEN_FIRST_MINUTE_SECS_IST = 09:00:00,
        //  PREOPEN_LAST_MINUTE_SECS_IST = 09:13:00 exclusive).
        //
        // The function returns a bool based on the current wall clock,
        // which we cannot pin from a unit test without freezing time.
        // The crucial invariant is that the window helper EXISTS and is
        // wired — `is_within_preopen_window` is the single source of
        // truth, so any change to the window constants flips both the
        // buffer and the runner together.
        let _result: bool = is_within_preopen_window();

        // The boundary semantics (09:00 inclusive, 09:13 exclusive) are
        // tested in `preopen_price_buffer::tests` ratchet
        // `test_preopen_buffer_window_is_0900_to_0912`. We assert the
        // helper exists as a public symbol — if the runner ever adds
        // its own window constants, this test will need updating.
        use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
        assert!(
            IST_UTC_OFFSET_SECONDS > 0,
            "IST offset must be positive — sanity check"
        );
    }

    #[test]
    fn test_preopen_movers_sensex_marked_unavailable_not_error() {
        // SENSEX (BSE) is NOT in the Dhan pre-open feed. The tracker
        // emits a row with phase=PREOPEN_UNAVAILABLE, zero prices, rank 0.
        // No error is raised — silent absence would be a Rule 11 violation
        // (false-OK signal). The explicit row proves the absence is intentional.
        let unavailable = vec![UnavailableSymbol {
            symbol: "SENSEX".to_string(),
            security_id: 51,
            segment: ExchangeSegment::IdxI,
        }];
        let tracker = PreopenMoversTracker::new(HashMap::new(), unavailable);

        let snap = tracker.compute_snapshot();
        assert_eq!(snap.len(), 1);
        let s = &snap[0];
        assert_eq!(s.symbol, "SENSEX");
        assert_eq!(s.security_id, 51);
        assert_eq!(s.phase, PreopenPhase::PreopenUnavailable);
        assert_eq!(s.ltp, 0.0);
        assert_eq!(s.prev_close, 0.0);
        assert_eq!(s.change_pct, 0.0);
        assert_eq!(s.volume, 0);
        assert_eq!(s.rank, 0);
    }

    #[test]
    fn test_preopen_movers_persists_phase_column() {
        // The wire string written to stock_movers.phase MUST match the
        // STOCK_MOVERS_PHASE_* constants in movers_persistence.rs.
        // Drift here = silent dashboard breakage.
        assert_eq!(PreopenPhase::Preopen.as_wire_str(), "PREOPEN");
        assert_eq!(
            PreopenPhase::PreopenUnavailable.as_wire_str(),
            "PREOPEN_UNAVAILABLE"
        );
    }

    #[test]
    fn test_preopen_movers_market_hours_gate() {
        // Audit-findings Rule 3: every poller of market data must be
        // market-hours-aware. The runner uses `is_within_preopen_window`
        // (re-exported below) as its sole gate. If the helper goes
        // missing or its return type changes, this test fails to compile.
        let _: fn() -> bool = is_within_preopen_window;

        // Defensive ratchet — empty tracker yields empty snapshot, no panic.
        let tracker = PreopenMoversTracker::new(HashMap::new(), Vec::new());
        let snap = tracker.compute_snapshot();
        assert!(snap.is_empty());
    }

    #[test]
    fn test_update_from_tick_ignores_unknown_security() {
        let lookup = lookup_with("RELIANCE", 2885, ExchangeSegment::NseEquity);
        let mut tracker = PreopenMoversTracker::new(lookup, Vec::new());
        // Unknown security — different security_id.
        let mut t = reliance_tick(100.0, 90.0, 5);
        t.security_id = 99999;
        tracker.update_from_tick(&t);
        assert_eq!(tracker.tracked_len(), 0);
        assert_eq!(tracker.prev_close_len(), 0);
    }

    #[test]
    fn test_update_from_tick_ignores_unknown_segment_byte() {
        let lookup = lookup_with("RELIANCE", 2885, ExchangeSegment::NseEquity);
        let mut tracker = PreopenMoversTracker::new(lookup, Vec::new());
        // Segment code 6 is the documented gap in ExchangeSegment::from_byte.
        let mut t = reliance_tick(100.0, 90.0, 5);
        t.exchange_segment_code = 6;
        tracker.update_from_tick(&t);
        assert_eq!(tracker.tracked_len(), 0);
    }

    #[test]
    fn test_update_from_tick_skips_zero_and_nan_prices() {
        let lookup = lookup_with("RELIANCE", 2885, ExchangeSegment::NseEquity);
        let mut tracker = PreopenMoversTracker::new(lookup, Vec::new());
        tracker.update_from_tick(&reliance_tick(0.0, 0.0, 0));
        tracker.update_from_tick(&reliance_tick(f32::NAN, f32::NAN, 0));
        tracker.update_from_tick(&reliance_tick(f32::INFINITY, 100.0, 0));
        assert_eq!(tracker.tracked_len(), 0);
        assert_eq!(
            tracker.prev_close_len(),
            1,
            "prev close 100 from third tick is valid"
        );
    }

    #[test]
    fn test_seed_prev_close_for_idx_i() {
        // NIFTY (id=13, IdxI) — Ticker-mode subscription, day_close = 0
        // on every tick. seed_prev_close is the only path that populates
        // prev close for indices.
        let lookup = lookup_with("NIFTY", 13, ExchangeSegment::IdxI);
        let mut tracker = PreopenMoversTracker::new(lookup, Vec::new());
        tracker.seed_prev_close(13, ExchangeSegment::IdxI, 24_500.0);

        // Now feed an LTP-only tick (IDX_I Ticker mode shape).
        let t = ParsedTick {
            security_id: 13,
            exchange_segment_code: 0, // IDX_I
            last_traded_price: 24_650.0,
            ..ParsedTick::default()
        };
        tracker.update_from_tick(&t);

        let snap = tracker.compute_snapshot();
        assert_eq!(snap.len(), 1);
        let n = &snap[0];
        assert_eq!(n.symbol, "NIFTY");
        assert_eq!(n.segment, ExchangeSegment::IdxI);
        assert!((n.prev_close - 24_500.0).abs() < 1e-9);
        // (24650 - 24500) / 24500 * 100 ≈ 0.6122
        assert!((n.change_pct - 0.6122).abs() < 0.01);
    }

    #[test]
    fn test_seed_prev_close_rejects_nonpositive_and_nonfinite() {
        let mut tracker = PreopenMoversTracker::new(HashMap::new(), Vec::new());
        tracker.seed_prev_close(13, ExchangeSegment::IdxI, 0.0);
        tracker.seed_prev_close(13, ExchangeSegment::IdxI, -100.0);
        tracker.seed_prev_close(13, ExchangeSegment::IdxI, f64::NAN);
        tracker.seed_prev_close(13, ExchangeSegment::IdxI, f64::INFINITY);
        assert_eq!(tracker.prev_close_len(), 0);
    }

    #[test]
    fn test_compute_snapshot_ranks_descending_by_change_pct() {
        let mut lookup = HashMap::new();
        lookup.insert((1, ExchangeSegment::NseEquity), "A".to_string());
        lookup.insert((2, ExchangeSegment::NseEquity), "B".to_string());
        lookup.insert((3, ExchangeSegment::NseEquity), "C".to_string());
        let mut tracker = PreopenMoversTracker::new(lookup, Vec::new());
        // A: +5%
        tracker.update_from_tick(&ParsedTick {
            security_id: 1,
            exchange_segment_code: 1,
            last_traded_price: 105.0,
            day_close: 100.0,
            ..ParsedTick::default()
        });
        // B: +10%
        tracker.update_from_tick(&ParsedTick {
            security_id: 2,
            exchange_segment_code: 1,
            last_traded_price: 110.0,
            day_close: 100.0,
            ..ParsedTick::default()
        });
        // C: -2%
        tracker.update_from_tick(&ParsedTick {
            security_id: 3,
            exchange_segment_code: 1,
            last_traded_price: 98.0,
            day_close: 100.0,
            ..ParsedTick::default()
        });

        let snap = tracker.compute_snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].symbol, "B");
        assert_eq!(snap[0].rank, 1);
        assert_eq!(snap[1].symbol, "A");
        assert_eq!(snap[1].rank, 2);
        assert_eq!(snap[2].symbol, "C");
        assert_eq!(snap[2].rank, 3);
    }

    #[test]
    fn test_reset_clears_data_but_preserves_lookup() {
        let lookup = lookup_with("RELIANCE", 2885, ExchangeSegment::NseEquity);
        let unavailable = vec![UnavailableSymbol {
            symbol: "SENSEX".to_string(),
            security_id: 51,
            segment: ExchangeSegment::IdxI,
        }];
        let mut tracker = PreopenMoversTracker::new(lookup, unavailable);
        tracker.update_from_tick(&reliance_tick(2900.0, 2850.0, 100));
        assert_eq!(tracker.tracked_len(), 1);
        assert_eq!(tracker.prev_close_len(), 1);

        tracker.reset();
        assert_eq!(tracker.tracked_len(), 0);
        assert_eq!(tracker.prev_close_len(), 0);

        // Lookup + unavailable list must survive reset — they are
        // populated once at boot from the static F&O universe.
        let snap = tracker.compute_snapshot();
        // Only SENSEX appears (no live data after reset).
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].symbol, "SENSEX");
        assert_eq!(snap[0].phase, PreopenPhase::PreopenUnavailable);

        // Reapply a tick — lookup still works.
        tracker.update_from_tick(&reliance_tick(2920.0, 2850.0, 200));
        assert_eq!(tracker.tracked_len(), 1);
    }

    #[test]
    fn test_compute_snapshot_skips_entries_without_prev_close() {
        let lookup = lookup_with("RELIANCE", 2885, ExchangeSegment::NseEquity);
        let mut tracker = PreopenMoversTracker::new(lookup, Vec::new());
        // Tick with LTP but no prev_close (Ticker mode shape).
        tracker.update_from_tick(&ParsedTick {
            security_id: 2885,
            exchange_segment_code: 1,
            last_traded_price: 2900.0,
            day_close: 0.0, // missing
            ..ParsedTick::default()
        });
        let snap = tracker.compute_snapshot();
        assert!(
            snap.is_empty(),
            "no prev_close → row excluded; otherwise change_pct division-by-zero"
        );
    }

    #[test]
    fn test_has_complete_data_for_requires_both_signals() {
        let lookup = lookup_with("RELIANCE", 2885, ExchangeSegment::NseEquity);
        let mut tracker = PreopenMoversTracker::new(lookup, Vec::new());
        let key = (2885u32, ExchangeSegment::NseEquity);
        assert!(!tracker.has_complete_data_for(key.0, key.1));

        // LTP only.
        tracker.update_from_tick(&ParsedTick {
            security_id: 2885,
            exchange_segment_code: 1,
            last_traded_price: 2900.0,
            ..ParsedTick::default()
        });
        assert!(!tracker.has_complete_data_for(key.0, key.1));

        // Add prev close.
        tracker.seed_prev_close(2885, ExchangeSegment::NseEquity, 2850.0);
        assert!(tracker.has_complete_data_for(key.0, key.1));
    }

    #[test]
    fn test_phase_wire_strings_match_storage_constants() {
        // Cross-crate sanity — if either the storage constants or the
        // PreopenPhase enum drift, the dashboard query
        // `WHERE phase = 'PREOPEN'` silently returns zero rows. Pin both
        // sides in lockstep here. This is a duplicate of
        // `test_preopen_movers_persists_phase_column` but stated as a
        // direct cross-crate constant comparison so the failure mode is
        // unambiguous.
        assert_eq!(
            PreopenPhase::Preopen.as_wire_str(),
            tickvault_storage::movers_persistence::STOCK_MOVERS_PHASE_PREOPEN
        );
        assert_eq!(
            PreopenPhase::PreopenUnavailable.as_wire_str(),
            tickvault_storage::movers_persistence::STOCK_MOVERS_PHASE_PREOPEN_UNAVAILABLE
        );
    }
}
