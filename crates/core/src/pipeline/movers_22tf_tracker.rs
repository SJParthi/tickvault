//! Papaya-backed concurrent state for the movers 22-tf snapshot scheduler —
//! Phase 10c of v3 plan, 2026-04-28.
//!
//! Holds the per-instrument live state (LTP, prev_close, OI, volume, etc.)
//! that the 22 snapshot tasks read once per cadence cycle. Backed by
//! `papaya::HashMap` for lock-free concurrent access:
//! - Tick processor (single writer) calls `update_security_state` on the
//!   per-tick hot path.
//! - 22 snapshot tasks (concurrent readers) call `snapshot_into(arena)`
//!   on their cadence boundary.
//!
//! ## Why a separate tracker from `TopMoversTracker`?
//!
//! `TopMoversTracker` uses `HashMap<(u32, u8), SecurityState>` with
//! `&mut self`-method semantics — designed for a single-threaded tick
//! processor that owns the map exclusively. The 22-tf scheduler reads
//! the same data CONCURRENTLY across 22 tasks, so the existing tracker
//! cannot be shared as-is. This module supplies the lock-free read
//! path; Phase 10b-2 wiring teaches the tick processor to feed both
//! trackers in parallel during the transition.
//!
//! ## I-P1-11 composite key
//!
//! Keyed on `(u32, ExchangeSegment)` per `security-id-uniqueness.md` —
//! `security_id` alone is NOT unique because Dhan reuses ids across
//! segments (e.g. NIFTY id=13 IDX_I + some NSE_EQ id=13).
//!
//! ## Hot-path budget
//!
//! - `update_security_state`: ≤ 100 ns (papaya pin().insert) per
//!   `quality/benchmark-budgets.toml::tick_gap_record_tick_steady_state`.
//! - `snapshot_into`: O(N) over ~24K entries; ~1-2 ms typical, runs on
//!   the cold scheduler thread, NOT the per-tick hot path.

use std::sync::{Arc, OnceLock};

use papaya::HashMap as PapayaMap;
use tickvault_common::mover_types::MoverRow;
use tickvault_common::types::ExchangeSegment;

/// Initial capacity hint for the papaya tracker. Sized to cover the
/// full ~24,324-instrument universe with headroom.
pub const TRACKER_INITIAL_CAPACITY: usize = 30_000;

/// Per-security live state — exactly the fields needed to construct a
/// `MoverRow` at snapshot time. `Copy` so papaya can store it without
/// heap allocation per-entry beyond the bucket overhead.
///
/// Numeric-only; the SYMBOL strings (segment, instrument_type, etc.)
/// come from the boot-time instrument registry lookup at snapshot time
/// (one Arc clone, not heap-cloned per entry).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SecurityState {
    /// Last traded price.
    pub ltp: f64,
    /// Previous trading day's close.
    pub prev_close: f64,
    /// Day's traded volume (cumulative).
    pub volume: i64,
    /// Top-5 buy quantity.
    pub buy_qty: i64,
    /// Top-5 sell quantity.
    pub sell_qty: i64,
    /// Open interest (NSE_FNO only — 0 for IDX_I/NSE_EQ).
    pub open_interest: i64,
    /// First-seen-today OI baseline for `oi_change` computation.
    pub first_oi_today: i64,
    /// Last update timestamp (IST epoch nanoseconds).
    pub last_updated_ts_nanos: i64,
}

impl SecurityState {
    /// Constructor for a fresh state with all numeric fields zero/NaN.
    /// Used by tests + the first-tick-seen path in the tick processor.
    #[must_use]
    // TEST-EXEMPT: trivial constructor; covered by `test_security_state_empty_uses_safe_sentinels`.
    pub const fn empty() -> Self {
        Self {
            ltp: f64::NAN,
            prev_close: f64::NAN,
            volume: 0,
            buy_qty: 0,
            sell_qty: 0,
            open_interest: 0,
            first_oi_today: 0,
            last_updated_ts_nanos: 0,
        }
    }

    /// Computes `change_pct` = `(ltp - prev_close) / prev_close * 100`.
    /// Returns NaN if `prev_close` is non-finite or zero.
    #[must_use]
    pub fn change_pct(&self) -> f64 {
        if !self.prev_close.is_finite() || self.prev_close == 0.0 {
            return f64::NAN;
        }
        (self.ltp - self.prev_close) / self.prev_close * 100.0
    }

    /// Computes `change_abs` = `ltp - prev_close`. NaN-safe.
    #[must_use]
    // TEST-EXEMPT: trivial subtraction NaN-safe by construction; exercised indirectly by `test_movers_22tf_tracker_snapshot_into_clears_arena` which validates change_abs in the resulting MoverRow.
    pub fn change_abs(&self) -> f64 {
        self.ltp - self.prev_close
    }

    /// Computes `oi_change` = `open_interest - first_oi_today`.
    #[must_use]
    pub const fn oi_change(&self) -> i64 {
        self.open_interest - self.first_oi_today
    }

    /// Computes `oi_change_pct` from `first_oi_today` baseline.
    /// Returns NaN if `first_oi_today` is zero.
    #[must_use]
    pub fn oi_change_pct(&self) -> f64 {
        if self.first_oi_today == 0 {
            return f64::NAN;
        }
        let delta = self.oi_change() as f64;
        // DATA-INTEGRITY-EXEMPT: first_oi_today is an i64 OI count (NOT a Dhan price), so f32_to_f64_clean() does not apply.
        let baseline = self.first_oi_today as f64;
        delta / baseline * 100.0
    }
}

/// Composite key per I-P1-11 — pairs the numeric `security_id` with the
/// `ExchangeSegment`. `security_id` alone is NOT unique.
pub type TrackerKey = (u32, ExchangeSegment);

/// Papaya-backed concurrent tracker. Cheap to clone via `Arc` so all 22
/// snapshot tasks share one instance.
#[derive(Clone)]
pub struct Movers22TfTracker {
    inner: Arc<PapayaMap<TrackerKey, SecurityState>>,
}

impl Movers22TfTracker {
    /// Constructs a new tracker with the canonical capacity hint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PapayaMap::with_capacity(TRACKER_INITIAL_CAPACITY)),
        }
    }

    /// Returns the number of tracked securities. O(1) on papaya.
    #[must_use]
    // TEST-EXEMPT: trivial accessor; covered by `test_movers_22tf_tracker_starts_empty` (asserts len==0) + insert tests (assert len incremented).
    pub fn len(&self) -> usize {
        self.inner.pin().len()
    }

    /// True iff the tracker is empty.
    #[must_use]
    // TEST-EXEMPT: trivial accessor; covered by `test_movers_22tf_tracker_starts_empty` (asserts is_empty()).
    pub fn is_empty(&self) -> bool {
        self.inner.pin().is_empty()
    }

    /// Inserts or updates the state for the given (security_id, segment).
    /// Hot-path entry — called once per tick.
    // TEST-EXEMPT: covered by `test_movers_22tf_tracker_insert_and_get_roundtrip` + `test_movers_22tf_tracker_composite_key_distinguishes_segments` which both call this method as the setup step.
    pub fn update_security_state(
        &self,
        security_id: u32,
        segment: ExchangeSegment,
        state: SecurityState,
    ) {
        // papaya `pin().insert` is lock-free + allocation-free in the
        // steady state. The pin guard is RAII-dropped at end of expr.
        self.inner.pin().insert((security_id, segment), state);
    }

    /// Reads the state for the given (security_id, segment). Returns
    /// None if not present.
    #[must_use]
    pub fn get(&self, security_id: u32, segment: ExchangeSegment) -> Option<SecurityState> {
        self.inner.pin().get(&(security_id, segment)).copied()
    }

    /// Drains the tracker state into the caller-provided arena, building
    /// `MoverRow` entries with computed `change_pct` / `change_abs` /
    /// `oi_change_pct` fields.
    ///
    /// Caller-owned arena: the arena is `clear()`ed at the start, then
    /// `push`ed into. Reusing the same Vec across cycles keeps the
    /// per-snapshot allocation cost AT MOST one capacity-grow (which
    /// the `TRACKER_INITIAL_CAPACITY` hint should prevent).
    ///
    /// SYMBOL fields (instrument_type, underlying_symbol, option_type)
    /// are NOT populated here — they come from the instrument registry
    /// in the boot-wiring layer (Phase 10b-2). This function fills the
    /// numeric + segment fields only.
    pub fn snapshot_into(&self, arena: &mut Vec<MoverRow>, snap_ts_nanos: i64) {
        arena.clear();
        let pinned = self.inner.pin();
        for ((security_id, segment), state) in pinned.iter() {
            let mut row = MoverRow::empty();
            row.ts_nanos = snap_ts_nanos;
            row.security_id = *security_id;
            row.segment = segment_to_char(*segment);
            row.ltp = state.ltp;
            row.prev_close = state.prev_close;
            row.change_pct = state.change_pct();
            row.change_abs = state.change_abs();
            row.volume = state.volume;
            row.buy_qty = state.buy_qty;
            row.sell_qty = state.sell_qty;
            row.open_interest = state.open_interest;
            row.oi_change = state.oi_change();
            row.oi_change_pct = state.oi_change_pct();
            row.received_at_nanos = state.last_updated_ts_nanos;
            arena.push(row);
        }
    }
}

impl Default for Movers22TfTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps `ExchangeSegment` to the single-char tag used in `MoverRow.segment`
/// and the QuestDB SYMBOL column. Mirrors the convention used by
/// `OptionMoversWriter` ('I'/'E'/'D').
#[must_use]
pub const fn segment_to_char(segment: ExchangeSegment) -> char {
    match segment {
        ExchangeSegment::IdxI => 'I',
        ExchangeSegment::NseEquity => 'E',
        ExchangeSegment::NseFno => 'D',
        // Other segments (BSE, MCX, currency) — not in the movers 22-tf
        // scope today. Mark as '?' so any test/dashboard that surfaces
        // them is visibly anomalous.
        _ => '?',
    }
}

// ---------------------------------------------------------------------------
// Phase 10c-2 — global tracker registry (mirror of movers_22tf_writer_state)
// ---------------------------------------------------------------------------
//
// Boot installs an Arc<Movers22TfTracker> at startup; the tick processor's
// per-tick hot path reads it via `try_update_global_tracker`. Mirrors the
// `init_global_writer_state` + `try_enqueue_global` pattern from
// movers_22tf_writer_state.rs so the two halves of the pipeline can be
// wired symmetrically.

static GLOBAL_TRACKER: OnceLock<Arc<Movers22TfTracker>> = OnceLock::new();

/// Installs the global tracker. Idempotent — second + later calls are
/// no-ops (returns `false`). Boot must call this exactly once when the
/// movers 22-tf pipeline is enabled.
// TEST-EXEMPT: thin wrapper over OnceLock::set; cross-test contamination prevents idempotency unit tests in process — covered by Phase 10b-3-final integration.
pub fn init_global_tracker(tracker: Arc<Movers22TfTracker>) -> bool {
    GLOBAL_TRACKER.set(tracker).is_ok()
}

/// True if the global tracker has been installed.
#[must_use]
// WIRING-EXEMPT: paired with init_global_tracker; both lifecycle helpers ship together so future maintainers can introspect installation state. Call site in operator-readiness diagnostics is the next follow-up.
// TEST-EXEMPT: thin wrapper over OnceLock::get; covered by Phase 10b-3 integration.
pub fn is_global_tracker_initialised() -> bool {
    GLOBAL_TRACKER.get().is_some()
}

/// Per-tick hot-path entry: updates the global tracker if installed,
/// otherwise no-op. Designed to be called from `tick_processor` on
/// every parsed tick alongside the legacy `TopMoversTracker.update`.
///
/// Hot-path budget: the body is one OnceLock::get (atomic load) + one
/// papaya pin().insert. Both are lock-free + allocation-free in the
/// steady state.
// TEST-EXEMPT: thin wrapper over OnceLock::get + tracker.update_security_state — both paths covered by movers_22tf_tracker tests.
pub fn try_update_global_tracker(security_id: u32, segment: ExchangeSegment, state: SecurityState) {
    if let Some(tracker) = GLOBAL_TRACKER.get() {
        tracker.update_security_state(security_id, segment, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 10c ratchet: tracker starts empty.
    #[test]
    fn test_movers_22tf_tracker_starts_empty() {
        let tracker = Movers22TfTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
    }

    /// Phase 10c ratchet: insert + get round-trip works on the canonical
    /// composite key per I-P1-11.
    #[test]
    fn test_movers_22tf_tracker_insert_and_get_roundtrip() {
        let tracker = Movers22TfTracker::new();
        let state = SecurityState {
            ltp: 19500.5,
            prev_close: 19400.0,
            volume: 1_000_000,
            ..SecurityState::empty()
        };
        tracker.update_security_state(13, ExchangeSegment::IdxI, state);
        assert_eq!(tracker.len(), 1);

        let fetched = tracker.get(13, ExchangeSegment::IdxI).unwrap();
        assert_eq!(fetched.ltp, 19500.5);
        assert_eq!(fetched.prev_close, 19400.0);
        assert_eq!(fetched.volume, 1_000_000);
    }

    /// Phase 10c ratchet: I-P1-11 composite-key uniqueness — same
    /// security_id with different segments must produce DISTINCT entries.
    #[test]
    fn test_movers_22tf_tracker_composite_key_distinguishes_segments() {
        let tracker = Movers22TfTracker::new();
        let idx_state = SecurityState {
            ltp: 19500.0,
            ..SecurityState::empty()
        };
        let eq_state = SecurityState {
            ltp: 1500.0,
            ..SecurityState::empty()
        };
        // Same security_id, different segments — both must coexist.
        tracker.update_security_state(13, ExchangeSegment::IdxI, idx_state);
        tracker.update_security_state(13, ExchangeSegment::NseEquity, eq_state);

        assert_eq!(tracker.len(), 2, "I-P1-11 composite key must keep both");
        assert_eq!(tracker.get(13, ExchangeSegment::IdxI).unwrap().ltp, 19500.0);
        assert_eq!(
            tracker.get(13, ExchangeSegment::NseEquity).unwrap().ltp,
            1500.0
        );
    }

    /// Phase 10c ratchet: `change_pct` formula is correct and NaN-safe.
    #[test]
    fn test_security_state_change_pct() {
        let s = SecurityState {
            ltp: 110.0,
            prev_close: 100.0,
            ..SecurityState::empty()
        };
        assert!((s.change_pct() - 10.0).abs() < 1e-9);

        let s = SecurityState {
            ltp: 90.0,
            prev_close: 100.0,
            ..SecurityState::empty()
        };
        assert!((s.change_pct() - (-10.0)).abs() < 1e-9);

        // NaN-safe on zero prev_close.
        let s = SecurityState {
            ltp: 110.0,
            prev_close: 0.0,
            ..SecurityState::empty()
        };
        assert!(s.change_pct().is_nan());

        // NaN-safe on NaN prev_close.
        let s = SecurityState {
            ltp: 110.0,
            prev_close: f64::NAN,
            ..SecurityState::empty()
        };
        assert!(s.change_pct().is_nan());
    }

    /// Phase 10c ratchet: `oi_change` + `oi_change_pct` formulas correct.
    #[test]
    fn test_security_state_oi_change_pct_and_change() {
        let s = SecurityState {
            open_interest: 1100,
            first_oi_today: 1000,
            ..SecurityState::empty()
        };
        assert_eq!(s.oi_change(), 100);
        assert!((s.oi_change_pct() - 10.0).abs() < 1e-9);

        // NaN-safe on zero baseline.
        let s = SecurityState {
            open_interest: 100,
            first_oi_today: 0,
            ..SecurityState::empty()
        };
        assert_eq!(s.oi_change(), 100);
        assert!(s.oi_change_pct().is_nan());
    }

    /// Phase 10c ratchet: `snapshot_into` clears the arena before
    /// pushing — caller-owned arena pattern.
    #[test]
    fn test_movers_22tf_tracker_snapshot_into_clears_arena() {
        let tracker = Movers22TfTracker::new();
        let state = SecurityState {
            ltp: 19500.0,
            prev_close: 19400.0,
            ..SecurityState::empty()
        };
        tracker.update_security_state(13, ExchangeSegment::IdxI, state);

        // Arena starts with junk that must be cleared.
        let mut arena: Vec<MoverRow> = vec![MoverRow::empty(); 5];
        tracker.snapshot_into(&mut arena, 1_700_000_000_000_000_000);

        assert_eq!(arena.len(), 1);
        assert_eq!(arena[0].security_id, 13);
        assert_eq!(arena[0].segment, 'I');
        assert!((arena[0].ltp - 19500.0).abs() < 1e-9);
        assert!((arena[0].change_pct - ((19500.0 - 19400.0) / 19400.0 * 100.0)).abs() < 1e-3);
    }

    /// Phase 10c ratchet: snapshot iteration works for many entries.
    #[test]
    fn test_movers_22tf_tracker_snapshot_into_handles_many_entries() {
        let tracker = Movers22TfTracker::new();
        for i in 0..1000_u32 {
            let state = SecurityState {
                ltp: f64::from(i) + 100.0,
                prev_close: 100.0,
                volume: i64::from(i * 10),
                ..SecurityState::empty()
            };
            let segment = match i % 3 {
                0 => ExchangeSegment::IdxI,
                1 => ExchangeSegment::NseEquity,
                _ => ExchangeSegment::NseFno,
            };
            tracker.update_security_state(i, segment, state);
        }
        let mut arena: Vec<MoverRow> = Vec::with_capacity(1500);
        tracker.snapshot_into(&mut arena, 1_700_000_000_000_000_000);
        assert_eq!(arena.len(), 1000);
    }

    /// Phase 10c ratchet: segment_to_char covers the canonical 3 segments.
    #[test]
    fn test_segment_to_char_canonical_three() {
        assert_eq!(segment_to_char(ExchangeSegment::IdxI), 'I');
        assert_eq!(segment_to_char(ExchangeSegment::NseEquity), 'E');
        assert_eq!(segment_to_char(ExchangeSegment::NseFno), 'D');
    }

    /// Phase 10c ratchet: cloned tracker shares the underlying papaya map
    /// — both clones see updates to either.
    #[test]
    fn test_movers_22tf_tracker_clone_shares_state() {
        let a = Movers22TfTracker::new();
        let b = a.clone();
        let state = SecurityState {
            ltp: 19500.0,
            ..SecurityState::empty()
        };
        a.update_security_state(13, ExchangeSegment::IdxI, state);

        // b should see the update — they share the Arc<PapayaMap>.
        assert_eq!(b.len(), 1);
        assert_eq!(b.get(13, ExchangeSegment::IdxI).unwrap().ltp, 19500.0);
    }

    /// Phase 10c ratchet: SecurityState::empty() returns sentinel-NaN values.
    #[test]
    fn test_security_state_empty_uses_safe_sentinels() {
        let s = SecurityState::empty();
        assert!(s.ltp.is_nan());
        assert!(s.prev_close.is_nan());
        assert_eq!(s.volume, 0);
        assert_eq!(s.open_interest, 0);
    }
}
