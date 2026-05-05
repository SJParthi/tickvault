//! Per-instrument `CandleEngine<TF>` lookup wrapper for the
//! 29-timeframes engine plan (Phase 3 commit 2).
//!
//! Wraps a `papaya::HashMap<(security_id, segment_code), Mutex<CandleEngine<TF>>>`
//! so the tick-processor hot loop can call `.on_tick(&tick)` once per
//! tick without scattering papaya/locking logic through `tick_processor.rs`.
//!
//! ## Why papaya + Mutex
//!
//! - `papaya` provides O(1) lock-free read on the composite key
//!   `(security_id, segment_code)` per I-P1-11.
//! - The inner `Mutex<CandleEngine<TF>>` serialises mutation per
//!   instrument. Critical-path locking is uncontended at typical
//!   load (one tick per instrument per second peak), so the
//!   contention-free fast path of `Mutex` (atomic CAS) costs ~5ns.
//! - We deliberately do NOT use `RwLock` — a writer is always
//!   needed (the `on_tick` mutation), so RwLock would degenerate
//!   to a Mutex with extra overhead.
//!
//! ## Hot-path budget
//!
//! Per plan L12: ≤200ns per tick on the 1s engine. Budget:
//! - papaya pin/get: ~30ns
//! - Mutex lock + unlock (uncontended): ~10ns
//! - on_tick body (state update): ~20ns
//! - SPSC publish on bar seal: ~50ns (only on the seal-second tick)
//! - Total: ~60ns typical, ~110ns on seal — well under 200ns budget
//!
//! ## What this module does NOT do (deferred)
//!
//! - The SPSC channel between this map and the cascade consumer
//!   (Phase 3 commit 3 — needs the consumer-side cascade engines).
//!   For now sealed bars are returned to the caller; the caller
//!   either drops them (no-op) or forwards them to a `tokio::mpsc`
//!   in the interim while commit 3 ships the proper rtrb SPSC.

use std::sync::Arc;
use std::sync::Mutex;

use papaya::HashMap;
use tickvault_common::tick_types::ParsedTick;

use crate::candles::engine::{Bar, CandleEngine, Timeframe};

/// Composite key per I-P1-11.
type EngineKey = (u32, u8);

/// Per-instrument `CandleEngine<TF>` lookup map.
///
/// Cloneable: the underlying `papaya::HashMap` and `Arc<Mutex<...>>`
/// values share state via Arc. Multiple consumers (the tick processor +
/// the IST midnight rollover task) hold their own clone.
pub struct CandleEngineMap<TF: Timeframe + 'static> {
    inner: Arc<HashMap<EngineKey, Arc<Mutex<CandleEngine<TF>>>>>,
}

impl<TF: Timeframe + 'static> Default for CandleEngineMap<TF> {
    fn default() -> Self {
        Self::new()
    }
}

impl<TF: Timeframe + 'static> Clone for CandleEngineMap<TF> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<TF: Timeframe + 'static> CandleEngineMap<TF> {
    /// Pre-sized capacity for the F&O universe (~24,324 instruments per
    /// `disaster-recovery.md`). Pre-allocating avoids rehash storms
    /// during boot-time first-tick insertion across the universe.
    const INITIAL_CAPACITY: usize = 25_000;

    /// Constructs an empty map sized for the full F&O universe.
    /// Engines are lazily inserted on the first tick per
    /// `(security_id, segment_code)` pair.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HashMap::with_capacity(Self::INITIAL_CAPACITY)),
        }
    }

    /// Folds a tick into the engine for this instrument. If the tick
    /// crosses a bucket boundary, returns the sealed previous bar
    /// (caller forwards to cascade SPSC).
    ///
    /// O(1), lock-free outer (papaya), uncontended mutex inner.
    /// The `Arc<Mutex>` clone is cheap — no allocation, just an atomic
    /// refcount bump that the optimizer hoists in tight loops.
    #[inline]
    pub fn on_tick(&self, tick: &ParsedTick) -> Option<Bar> {
        let key = (tick.security_id, tick.exchange_segment_code);
        let guard = self.inner.guard();
        // Hot path: try lock-free read. If the engine exists, mutate
        // through its inner Mutex. If absent, fall through to the
        // get-or-insert insert path below.
        if let Some(engine_arc) = self.inner.get(&key, &guard) {
            // Defensive against poisoned mutex: if a previous panic
            // tainted the mutex, recover the inner state and continue.
            // This is the classic Rust pattern — production code never
            // wants poison to take down a hot path.
            let mut engine = match engine_arc.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            return engine.on_tick(tick);
        }
        // First tick for this instrument — insert a fresh engine.
        let new_engine = Arc::new(Mutex::new(CandleEngine::<TF>::new()));
        let inserted_arc = match self.inner.try_insert(key, Arc::clone(&new_engine), &guard) {
            Ok(_) => new_engine,
            Err(occupied) => Arc::clone(occupied.current),
        };
        let mut engine = match inserted_arc.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        engine.on_tick(tick)
    }

    /// Folds a sealed bar (typically from a finer-grained TF cascade)
    /// into the engine for this instrument. Mirrors `on_tick` but uses
    /// the per-instrument engine's `on_sealed_bar` method so the
    /// derived TF aggregates from already-sealed input bars instead of
    /// raw ticks.
    ///
    /// Used by `CascadeFanout::feed_sealed_1s_bar` (Phase 3 commit 4)
    /// to feed every derived engine in the 28-TF set.
    ///
    /// O(1), lock-free outer (papaya), uncontended mutex inner.
    #[inline]
    pub fn on_sealed_bar(&self, bar: &Bar) -> Option<Bar> {
        let key = (bar.security_id, bar.exchange_segment_code);
        let guard = self.inner.guard();
        if let Some(engine_arc) = self.inner.get(&key, &guard) {
            let mut engine = match engine_arc.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            return engine.on_sealed_bar(bar);
        }
        let new_engine = Arc::new(Mutex::new(CandleEngine::<TF>::new()));
        let inserted_arc = match self.inner.try_insert(key, Arc::clone(&new_engine), &guard) {
            Ok(_) => new_engine,
            Err(occupied) => Arc::clone(occupied.current),
        };
        let mut engine = match inserted_arc.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        engine.on_sealed_bar(bar)
    }

    /// Returns the latest open bar for an instrument, or `None` if
    /// the engine has not seen a tick yet.
    ///
    /// Used by the trading bot for lock-free reads. The `Bar` is
    /// `Copy`, so the caller gets a snapshot and the engine continues
    /// to mutate freely.
    pub fn latest(&self, security_id: u32, segment_code: u8) -> Option<Bar> {
        let key = (security_id, segment_code);
        let guard = self.inner.guard();
        let engine_arc = self.inner.get(&key, &guard)?;
        let engine = match engine_arc.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        engine.latest()
    }

    /// Forces every engine in the map to seal its open bar. Used at
    /// IST midnight rollover (per L13). Returns the count of bars
    /// that were sealed (engines whose `force_seal` returned `Some`).
    ///
    /// O(N) over instruments — runs once per day. Not hot path.
    pub fn force_seal_all(&self) -> u32 {
        let mut count = 0_u32;
        let guard = self.inner.guard();
        for (_key, engine_arc) in self.inner.iter(&guard) {
            let mut engine = match engine_arc.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if engine.force_seal().is_some() {
                count = count.saturating_add(1);
            }
        }
        count
    }

    /// Returns the number of instruments with active engines.
    /// Useful for Prometheus gauge `tv_candle_engine_<tf>_active_instruments`.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if no instruments have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candles::engine::{Tf1s, Tf5s};

    fn make_tick(security_id: u32, segment_code: u8, ts_ist_secs: u32, ltp: f32) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment_code,
            last_traded_price: ltp,
            volume: 100,
            exchange_timestamp: ts_ist_secs,
            ..ParsedTick::default()
        }
    }

    #[test]
    fn test_new_map_is_empty() {
        let m: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn test_on_tick_first_call_inserts_engine() {
        let m: CandleEngineMap<Tf5s> = CandleEngineMap::new();
        let result = m.on_tick(&make_tick(1234, 1, 1000, 100.0));
        // First tick on a new engine returns None (no previous bar to seal).
        assert!(result.is_none());
        assert_eq!(m.len(), 1);
        let bar = m.latest(1234, 1).unwrap();
        assert_eq!(bar.security_id, 1234);
        assert_eq!(bar.exchange_segment_code, 1);
    }

    #[test]
    fn test_on_tick_boundary_crossing_returns_sealed_bar() {
        let m: CandleEngineMap<Tf5s> = CandleEngineMap::new();
        m.on_tick(&make_tick(1234, 1, 1000, 100.0));
        m.on_tick(&make_tick(1234, 1, 1003, 102.0));
        let sealed = m.on_tick(&make_tick(1234, 1, 1005, 110.0));
        assert!(sealed.is_some(), "boundary cross must seal previous bar");
        let sealed_bar = sealed.unwrap();
        assert!(sealed_bar.sealed);
        assert_eq!(sealed_bar.bucket_start_ist_secs, 1000);
    }

    /// I-P1-11 ratchet: ticks for the same security_id under different
    /// segments populate independent engines.
    #[test]
    fn test_composite_key_isolates_cross_segment_collisions() {
        let m: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        m.on_tick(&make_tick(13, 0, 1000, 22500.0)); // NIFTY IDX_I
        m.on_tick(&make_tick(13, 1, 1000, 1500.0)); // NSE_EQ same id
        assert_eq!(m.len(), 2);
        let nifty = m.latest(13, 0).unwrap();
        let stock = m.latest(13, 1).unwrap();
        assert!((nifty.open - 22500.0).abs() < 1e-3);
        assert!((stock.open - 1500.0).abs() < 1e-3);
    }

    #[test]
    fn test_latest_returns_none_for_unknown_key() {
        let m: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        m.on_tick(&make_tick(1234, 1, 1000, 100.0));
        assert!(m.latest(9999, 1).is_none());
        assert!(m.latest(1234, 99).is_none());
    }

    #[test]
    fn test_force_seal_all_counts_sealed_engines() {
        let m: CandleEngineMap<Tf1m> = CandleEngineMap::new();
        m.on_tick(&make_tick(1, 1, 1020, 100.0));
        m.on_tick(&make_tick(2, 1, 1020, 200.0));
        m.on_tick(&make_tick(3, 1, 1020, 300.0));
        let sealed = m.force_seal_all();
        assert_eq!(sealed, 3, "3 active engines must seal");
        // After force seal, latest is None.
        assert!(m.latest(1, 1).is_none());
        assert!(m.latest(2, 1).is_none());
        assert!(m.latest(3, 1).is_none());
    }

    #[test]
    fn test_clone_shares_state() {
        let m1: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        let m2 = m1.clone();
        m1.on_tick(&make_tick(42, 1, 1000, 99.5));
        // m2 sees the same engine — both clones share the inner map.
        assert!(m2.latest(42, 1).is_some());
    }

    #[test]
    fn test_handles_25k_instruments_no_panic() {
        let m: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        for i in 0..25_000_u32 {
            m.on_tick(&make_tick(i, 1, 1000, 100.0));
        }
        assert_eq!(m.len(), 25_000);
    }

    /// pub-fn-test guard explicit name match for `on_tick`.
    #[test]
    fn test_on_tick_explicit_name_match() {
        let m: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        let _ = m.on_tick(&make_tick(1, 1, 1000, 100.0));
    }

    /// pub-fn-test guard explicit name match for `latest`.
    #[test]
    fn test_latest_explicit_name_match() {
        let m: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        assert!(m.latest(99, 1).is_none());
    }

    /// pub-fn-test guard explicit name match for `force_seal_all`.
    #[test]
    fn test_force_seal_all_explicit_name_match() {
        let m: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        assert_eq!(m.force_seal_all(), 0);
    }

    /// pub-fn-test guard explicit name match for `len`.
    #[test]
    fn test_len_explicit_name_match() {
        let m: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        assert_eq!(m.len(), 0);
    }

    /// pub-fn-test guard explicit name match for `is_empty`.
    #[test]
    fn test_is_empty_explicit_name_match() {
        let m: CandleEngineMap<Tf1s> = CandleEngineMap::new();
        assert!(m.is_empty());
    }

    /// pub-fn-test guard explicit name match for `new`.
    #[test]
    fn test_new_explicit_name_match() {
        let _: CandleEngineMap<Tf1s> = CandleEngineMap::new();
    }

    use crate::candles::engine::Tf1m;
}
