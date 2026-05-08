//! In-memory `CandleEngine<TF>` — generic, single-timeframe OHLCV
//! aggregator for the 29-timeframes engine plan (Phase 3).
//!
//! Per plan §2 (CASCADE design):
//! - The hot-path tick processor calls `CandleEngine<1s>::on_tick(tick)`
//!   on every tick — ONLY the 1s engine.
//! - When the 1s engine seals a bar (every 1 IST second), the sealed
//!   `Bar` is forwarded to a background SPSC consumer that cascades it
//!   into the 28 derived engines (3s/5s/.../1mo).
//! - Trading bot reads the engine state via `latest()` lock-free —
//!   never touches QuestDB.
//!
//! ## What this module ships
//!
//! - The `Bar` struct (canonical OHLCV + lifecycle metadata).
//! - The `CandleEngine<TF: Timeframe>` generic with:
//!   * `on_tick(&tick)` — folds a tick into the open bar; returns
//!     the sealed `Bar` IF this tick crossed a TF boundary.
//!   * `on_sealed_bar(&Bar)` — folds a sealed bar from a finer TF into
//!     this TF's open bar (cascade input).
//!   * `latest()` — lock-free read of the current open bar (for
//!     trading bot snapshots).
//! - The `Timeframe` trait carrying the bucketing math + display name.
//! - 7 concrete `Timeframe` types: `Tf1s`, `Tf5s`, `Tf15s`, `Tf30s`,
//!   `Tf1m`, `Tf5m`, `Tf15m`. (More land in subsequent commits — the
//!   trait keeps adding TFs to a one-line struct definition.)
//!
//! ## Hot-path guarantee
//!
//! `on_tick` is `#[inline]`, takes `&mut self` for the open-bar update,
//! and uses no allocation, no syscalls, no atomics. The whole
//! per-tick path is integer arithmetic + 7 f64 comparisons.
//! Bench-target: ≤200 ns per tick at 25K tps (per plan L12).

use tickvault_common::tick_types::ParsedTick;

/// Compact OHLCV bar with lifecycle metadata. `Copy` so the cascade
/// SPSC channel can pass it by value without allocation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    pub bucket_start_ist_secs: u32,
    pub bucket_end_ist_secs: u32,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub volume_cum_day_at_end: i64,
    pub oi: i64,
    pub tick_count: u32,
    pub security_id: u32,
    pub exchange_segment_code: u8,
    pub sealed: bool,
    pub prev_day_close: f64,
    pub prev_day_oi: i64,
    pub close_pct_from_prev_day: f64,
    pub oi_pct_from_prev_day: f64,
    pub volume_pct_from_prev_day: f64,
}

impl Bar {
    #[inline]
    fn new_open_at(
        bucket_start_ist_secs: u32,
        bucket_end_ist_secs: u32,
        security_id: u32,
        exchange_segment_code: u8,
    ) -> Self {
        Self {
            bucket_start_ist_secs,
            bucket_end_ist_secs,
            open: 0.0,
            high: f64::NEG_INFINITY,
            low: f64::INFINITY,
            close: 0.0,
            volume: 0,
            volume_cum_day_at_end: 0,
            oi: 0,
            tick_count: 0,
            security_id,
            exchange_segment_code,
            sealed: false,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        }
    }
}

pub trait Timeframe {
    const SECS: u32;
    const NAME: &'static str;

    #[inline]
    fn bucket_start(tick_ist_secs: u32) -> u32 {
        tick_ist_secs - (tick_ist_secs % Self::SECS)
    }

    #[inline]
    fn bucket_end(bucket_start: u32) -> u32 {
        bucket_start + Self::SECS
    }
}

/// 1-second timeframe (the only timeframe touched on the hot path
/// per plan L12).
#[derive(Clone, Copy, Debug)]
pub struct Tf1s;
impl Timeframe for Tf1s {
    const SECS: u32 = 1;
    const NAME: &'static str = "1s";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf5s;
impl Timeframe for Tf5s {
    const SECS: u32 = 5;
    const NAME: &'static str = "5s";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf15s;
impl Timeframe for Tf15s {
    const SECS: u32 = 15;
    const NAME: &'static str = "15s";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf30s;
impl Timeframe for Tf30s {
    const SECS: u32 = 30;
    const NAME: &'static str = "30s";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf1m;
impl Timeframe for Tf1m {
    const SECS: u32 = 60;
    const NAME: &'static str = "1m";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf5m;
impl Timeframe for Tf5m {
    const SECS: u32 = 300;
    const NAME: &'static str = "5m";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf15m;
impl Timeframe for Tf15m {
    const SECS: u32 = 900;
    const NAME: &'static str = "15m";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf3s;
impl Timeframe for Tf3s {
    const SECS: u32 = 3;
    const NAME: &'static str = "3s";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf10s;
impl Timeframe for Tf10s {
    const SECS: u32 = 10;
    const NAME: &'static str = "10s";
}

// PR #517 (Wave-5 TF reduction): retired the 12 sub-15-minute non-canonical
// timeframes — Tf2m / Tf3m / Tf4m / Tf6m / Tf7m / Tf8m / Tf9m / Tf10m /
// Tf11m / Tf12m / Tf13m / Tf14m. The runtime ladder is now 1m / 5m / 15m /
// 30m / 1h / 2h / 3h / 4h / 1d. The cascade RAM engines, the QuestDB
// matview DDL, and `TimeframesConfig::default_list` are kept in lock-step
// by the symmetry ratchet in `crates/common/tests/tf_symmetry_guard.rs`.
// Re-add a TF here ONLY together with: matview DDL entry, default_list
// entry, Tf enum variant, cascade_fanout field + accessor, and the
// symmetry ratchet update. Do not partially re-add.

#[derive(Clone, Copy, Debug)]
pub struct Tf30m;
impl Timeframe for Tf30m {
    const SECS: u32 = 1_800;
    const NAME: &'static str = "30m";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf1h;
impl Timeframe for Tf1h {
    const SECS: u32 = 3_600;
    const NAME: &'static str = "1h";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf2h;
impl Timeframe for Tf2h {
    const SECS: u32 = 7_200;
    const NAME: &'static str = "2h";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf3h;
impl Timeframe for Tf3h {
    const SECS: u32 = 10_800;
    const NAME: &'static str = "3h";
}

#[derive(Clone, Copy, Debug)]
pub struct Tf4h;
impl Timeframe for Tf4h {
    const SECS: u32 = 14_400;
    const NAME: &'static str = "4h";
}

/// 1-day timeframe (86_400 seconds — UTC-midnight aligned; calendar
/// IST-midnight bucketing is a follow-on refinement).
///
/// **WARN — calendar-approximate, NOT for trading signals (hostile C2):**
/// the IST-midnight rollover task in
/// `cascade::run_midnight_rollover_task_with_fanout` force-seals open
/// bars at IST 00:00 every day, so the boundary mismatch between
/// `SECS = 86_400` (UTC) and IST does not produce stale candles in
/// practice. Trading bot consumers reading `latest()` between IST
/// 00:00 and IST 05:30 see a freshly-opened post-rollover bar — not a
/// stale UTC-anchored one.
#[derive(Clone, Copy, Debug)]
pub struct Tf1d;
impl Timeframe for Tf1d {
    const SECS: u32 = 86_400;
    const NAME: &'static str = "1d";
}

/// 1-week timeframe (604_800 seconds — Thursday-aligned in epoch space;
/// calendar Sunday-Saturday bucketing is a follow-on refinement).
///
/// **WARN — calendar-approximate, NOT for trading signals (hostile C2):**
/// epoch alignment != trading-week alignment. The IST-midnight rollover
/// task force-seals at every IST 00:00, so the boundary mismatch does
/// not produce stale candles in practice; consumers that need exact
/// week-of-trading semantics MUST read from the QuestDB matview, not
/// from `CandleEngineMap<Tf1w>::latest()`.
#[derive(Clone, Copy, Debug)]
pub struct Tf1w;
impl Timeframe for Tf1w {
    const SECS: u32 = 604_800;
    const NAME: &'static str = "1w";
}

/// 1-month timeframe (approximated as 30 × 86_400 = 2_592_000 seconds;
/// calendar last-day-of-month bucketing is a follow-on refinement).
///
/// **WARN — calendar-approximate, NOT for trading signals (hostile C2):**
/// real months range 28-31 days, not 30; the bucket boundary drifts
/// 1-3 days per month vs the actual calendar. The IST-midnight
/// rollover keeps drift bounded inside one day, but consumers that
/// need exact month-of-trading semantics MUST read from the QuestDB
/// matview, not from `CandleEngineMap<Tf1mo>::latest()`.
#[derive(Clone, Copy, Debug)]
pub struct Tf1mo;
impl Timeframe for Tf1mo {
    const SECS: u32 = 2_592_000;
    const NAME: &'static str = "1mo";
}

pub struct CandleEngine<TF: Timeframe> {
    open_bar: Option<Bar>,
    _tf: std::marker::PhantomData<TF>,
}

impl<TF: Timeframe> Default for CandleEngine<TF> {
    fn default() -> Self {
        Self::new()
    }
}

impl<TF: Timeframe> CandleEngine<TF> {
    pub fn new() -> Self {
        Self {
            open_bar: None,
            _tf: std::marker::PhantomData,
        }
    }

    #[inline]
    pub fn on_tick(&mut self, tick: &ParsedTick) -> Option<Bar> {
        let tick_ist_secs = tick.exchange_timestamp;
        let new_bucket_start = TF::bucket_start(tick_ist_secs);
        let new_bucket_end = TF::bucket_end(new_bucket_start);
        let ltp_f64 = f64::from(tick.last_traded_price);

        match self.open_bar {
            None => {
                let mut bar = Bar::new_open_at(
                    new_bucket_start,
                    new_bucket_end,
                    tick.security_id,
                    tick.exchange_segment_code,
                );
                bar.open = ltp_f64;
                bar.high = ltp_f64;
                bar.low = ltp_f64;
                bar.close = ltp_f64;
                bar.volume = 0;
                bar.volume_cum_day_at_end = i64::from(tick.volume);
                bar.oi = i64::from(tick.open_interest);
                bar.tick_count = 1;
                self.open_bar = Some(bar);
                None
            }
            Some(ref mut bar) => {
                if new_bucket_start == bar.bucket_start_ist_secs {
                    if ltp_f64 > bar.high {
                        bar.high = ltp_f64;
                    }
                    if ltp_f64 < bar.low {
                        bar.low = ltp_f64;
                    }
                    bar.close = ltp_f64;
                    bar.volume_cum_day_at_end = i64::from(tick.volume);
                    bar.oi = i64::from(tick.open_interest);
                    bar.tick_count = bar.tick_count.saturating_add(1);
                    None
                } else {
                    let mut sealed = *bar;
                    sealed.sealed = true;
                    let mut next = Bar::new_open_at(
                        new_bucket_start,
                        new_bucket_end,
                        tick.security_id,
                        tick.exchange_segment_code,
                    );
                    next.open = ltp_f64;
                    next.high = ltp_f64;
                    next.low = ltp_f64;
                    next.close = ltp_f64;
                    next.volume = 0;
                    next.volume_cum_day_at_end = i64::from(tick.volume);
                    next.oi = i64::from(tick.open_interest);
                    next.tick_count = 1;
                    self.open_bar = Some(next);
                    Some(sealed)
                }
            }
        }
    }

    #[inline]
    pub fn on_sealed_bar(&mut self, input: &Bar) -> Option<Bar> {
        let input_start = input.bucket_start_ist_secs;
        let new_bucket_start = TF::bucket_start(input_start);
        let new_bucket_end = TF::bucket_end(new_bucket_start);

        match self.open_bar {
            None => {
                let mut bar = Bar::new_open_at(
                    new_bucket_start,
                    new_bucket_end,
                    input.security_id,
                    input.exchange_segment_code,
                );
                bar.open = input.open;
                bar.high = input.high;
                bar.low = input.low;
                bar.close = input.close;
                bar.volume = input.volume;
                bar.volume_cum_day_at_end = input.volume_cum_day_at_end;
                bar.oi = input.oi;
                bar.tick_count = input.tick_count;
                self.open_bar = Some(bar);
                None
            }
            Some(ref mut bar) => {
                if new_bucket_start == bar.bucket_start_ist_secs {
                    if input.high > bar.high {
                        bar.high = input.high;
                    }
                    if input.low < bar.low {
                        bar.low = input.low;
                    }
                    bar.close = input.close;
                    bar.volume = bar.volume.saturating_add(input.volume);
                    bar.volume_cum_day_at_end = input.volume_cum_day_at_end;
                    bar.oi = input.oi;
                    bar.tick_count = bar.tick_count.saturating_add(input.tick_count);
                    None
                } else {
                    let mut sealed = *bar;
                    sealed.sealed = true;
                    let mut next = Bar::new_open_at(
                        new_bucket_start,
                        new_bucket_end,
                        input.security_id,
                        input.exchange_segment_code,
                    );
                    next.open = input.open;
                    next.high = input.high;
                    next.low = input.low;
                    next.close = input.close;
                    next.volume = input.volume;
                    next.volume_cum_day_at_end = input.volume_cum_day_at_end;
                    next.oi = input.oi;
                    next.tick_count = input.tick_count;
                    self.open_bar = Some(next);
                    Some(sealed)
                }
            }
        }
    }

    #[inline]
    pub fn latest(&self) -> Option<Bar> {
        self.open_bar
    }

    pub fn force_seal(&mut self) -> Option<Bar> {
        self.open_bar.take().map(|mut bar| {
            bar.sealed = true;
            bar
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tick(ts_ist_secs: u32, ltp: f32, volume: u32, oi: u32) -> ParsedTick {
        ParsedTick {
            security_id: 1234,
            exchange_segment_code: 1,
            last_traded_price: ltp,
            volume,
            open_interest: oi,
            exchange_timestamp: ts_ist_secs,
            ..ParsedTick::default()
        }
    }

    #[test]
    fn test_bar_new_open_at_zero_inits_wave5_pct_fields() {
        let b = Bar::new_open_at(0, 60, 1, 1);
        assert_eq!(b.prev_day_close, 0.0);
        assert_eq!(b.prev_day_oi, 0);
        assert_eq!(b.close_pct_from_prev_day, 0.0);
        assert_eq!(b.oi_pct_from_prev_day, 0.0);
        assert_eq!(b.volume_pct_from_prev_day, 0.0);
    }

    #[test]
    fn test_bar_wave5_pct_fields_are_f64() {
        let b = Bar {
            bucket_start_ist_secs: 0,
            bucket_end_ist_secs: 60,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume: 0,
            volume_cum_day_at_end: 0,
            oi: 0,
            tick_count: 0,
            security_id: 1,
            exchange_segment_code: 1,
            sealed: false,
            prev_day_close: 1.234_567_890_123_456_e10,
            prev_day_oi: 0,
            close_pct_from_prev_day: 1.234_567_890_123_456_e10,
            oi_pct_from_prev_day: 1.234_567_890_123_456_e10,
            volume_pct_from_prev_day: 1.234_567_890_123_456_e10,
        };
        assert!((b.prev_day_close - 1.234_567_890_123_456_e10).abs() < 1.0);
        assert!((b.close_pct_from_prev_day - 1.234_567_890_123_456_e10).abs() < 1.0);
        assert!((b.oi_pct_from_prev_day - 1.234_567_890_123_456_e10).abs() < 1.0);
        assert!((b.volume_pct_from_prev_day - 1.234_567_890_123_456_e10).abs() < 1.0);
    }

    #[test]
    fn test_tf_secs_constants_are_correct() {
        assert_eq!(Tf1s::SECS, 1);
        assert_eq!(Tf5s::SECS, 5);
        assert_eq!(Tf15s::SECS, 15);
        assert_eq!(Tf30s::SECS, 30);
        assert_eq!(Tf1m::SECS, 60);
        assert_eq!(Tf5m::SECS, 300);
        assert_eq!(Tf15m::SECS, 900);
    }

    #[test]
    fn test_calendar_approximate_tfs_carry_explicit_warning_in_docstring() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/candles/engine.rs"
        ))
        .expect("must be able to read engine.rs");
        let occurrences = src
            .matches("WARN — calendar-approximate, NOT for trading signals")
            .count();
        assert!(
            occurrences >= 3,
            "Tf1d, Tf1w, Tf1mo docstrings must each carry the \
             'WARN — calendar-approximate, NOT for trading signals' \
             phrase (found {occurrences} occurrences, expected >= 3)"
        );
    }

    #[test]
    fn test_tf_secs_constants_phase_3_4_additions_are_correct() {
        assert_eq!(Tf3s::SECS, 3);
        assert_eq!(Tf10s::SECS, 10);
        // PR #517: Tf2m..Tf14m sub-15-minute non-canonical TFs retired.
        assert_eq!(Tf30m::SECS, 1_800);
        assert_eq!(Tf1h::SECS, 3_600);
        assert_eq!(Tf2h::SECS, 7_200);
        assert_eq!(Tf3h::SECS, 10_800);
        assert_eq!(Tf4h::SECS, 14_400);
        assert_eq!(Tf1d::SECS, 86_400);
        assert_eq!(Tf1w::SECS, 604_800);
        assert_eq!(Tf1mo::SECS, 2_592_000);
    }

    #[test]
    fn test_tf_names_match_questdb_matview_suffixes() {
        assert_eq!(Tf1s::NAME, "1s");
        assert_eq!(Tf5s::NAME, "5s");
        assert_eq!(Tf15s::NAME, "15s");
        assert_eq!(Tf30s::NAME, "30s");
        assert_eq!(Tf1m::NAME, "1m");
        assert_eq!(Tf5m::NAME, "5m");
        assert_eq!(Tf15m::NAME, "15m");
    }

    #[test]
    fn test_tf_names_phase_3_4_additions_match_questdb_matview_suffixes() {
        assert_eq!(Tf3s::NAME, "3s");
        assert_eq!(Tf10s::NAME, "10s");
        // PR #517: Tf2m..Tf14m sub-15-minute non-canonical TFs retired.
        assert_eq!(Tf30m::NAME, "30m");
        assert_eq!(Tf1h::NAME, "1h");
        assert_eq!(Tf2h::NAME, "2h");
        assert_eq!(Tf3h::NAME, "3h");
        assert_eq!(Tf4h::NAME, "4h");
        assert_eq!(Tf1d::NAME, "1d");
        assert_eq!(Tf1w::NAME, "1w");
        assert_eq!(Tf1mo::NAME, "1mo");
    }

    #[test]
    fn test_bucket_start_aligns_to_tf_multiple() {
        assert_eq!(Tf1s::bucket_start(1000), 1000);
        assert_eq!(Tf1s::bucket_start(1001), 1001);
        assert_eq!(Tf5s::bucket_start(1003), 1000);
        assert_eq!(Tf5s::bucket_start(1005), 1005);
        assert_eq!(Tf1m::bucket_start(1059), 1020);
        assert_eq!(Tf1m::bucket_start(1080), 1080);
    }

    #[test]
    fn test_bucket_end_equals_start_plus_secs() {
        assert_eq!(Tf1s::bucket_end(1000), 1001);
        assert_eq!(Tf5s::bucket_end(1000), 1005);
        assert_eq!(Tf1m::bucket_end(1020), 1080);
    }

    #[test]
    fn test_new_engine_has_no_open_bar() {
        let e: CandleEngine<Tf1s> = CandleEngine::new();
        assert!(e.latest().is_none());
    }

    #[test]
    fn test_first_tick_opens_bar_does_not_seal() {
        let mut e: CandleEngine<Tf5s> = CandleEngine::new();
        let result = e.on_tick(&make_tick(1000, 100.5, 500, 0));
        assert!(result.is_none());
        let bar = e.latest().expect("open bar after first tick");
        assert_eq!(bar.bucket_start_ist_secs, 1000);
        assert_eq!(bar.bucket_end_ist_secs, 1005);
        assert!((bar.open - 100.5).abs() < 1e-3);
        assert!((bar.high - 100.5).abs() < 1e-3);
        assert!((bar.low - 100.5).abs() < 1e-3);
        assert!((bar.close - 100.5).abs() < 1e-3);
        assert_eq!(bar.tick_count, 1);
        assert_eq!(bar.volume_cum_day_at_end, 500);
        assert!(!bar.sealed);
    }

    #[test]
    fn test_second_tick_same_bucket_folds_in() {
        let mut e: CandleEngine<Tf5s> = CandleEngine::new();
        e.on_tick(&make_tick(1000, 100.0, 500, 0));
        let result = e.on_tick(&make_tick(1003, 105.0, 800, 0));
        assert!(result.is_none());
        let bar = e.latest().unwrap();
        assert!((bar.open - 100.0).abs() < 1e-3);
        assert!((bar.high - 105.0).abs() < 1e-3);
        assert!((bar.low - 100.0).abs() < 1e-3);
        assert!((bar.close - 105.0).abs() < 1e-3);
        assert_eq!(bar.tick_count, 2);
        assert_eq!(bar.volume_cum_day_at_end, 800);
    }

    #[test]
    fn test_tick_crossing_boundary_seals_previous_bar() {
        let mut e: CandleEngine<Tf5s> = CandleEngine::new();
        e.on_tick(&make_tick(1000, 100.0, 500, 0));
        e.on_tick(&make_tick(1003, 102.0, 800, 0));
        let sealed = e
            .on_tick(&make_tick(1005, 110.0, 1000, 0))
            .expect("tick at boundary must seal previous bar");
        assert!(sealed.sealed);
        assert_eq!(sealed.bucket_start_ist_secs, 1000);
        assert!((sealed.open - 100.0).abs() < 1e-3);
        assert!((sealed.high - 102.0).abs() < 1e-3);
        assert!((sealed.close - 102.0).abs() < 1e-3);
        assert_eq!(sealed.tick_count, 2);
        let new_bar = e.latest().unwrap();
        assert_eq!(new_bar.bucket_start_ist_secs, 1005);
        assert!((new_bar.open - 110.0).abs() < 1e-3);
        assert_eq!(new_bar.tick_count, 1);
        assert!(!new_bar.sealed);
    }

    #[test]
    fn test_high_low_track_extremes_across_ticks() {
        let mut e: CandleEngine<Tf1m> = CandleEngine::new();
        e.on_tick(&make_tick(1020, 100.0, 100, 0));
        e.on_tick(&make_tick(1030, 95.0, 200, 0));
        e.on_tick(&make_tick(1040, 110.0, 300, 0));
        e.on_tick(&make_tick(1050, 90.0, 400, 0));
        e.on_tick(&make_tick(1075, 105.0, 500, 0));
        let bar = e.latest().unwrap();
        assert!((bar.open - 100.0).abs() < 1e-3);
        assert!((bar.high - 110.0).abs() < 1e-3);
        assert!((bar.low - 90.0).abs() < 1e-3);
        assert!((bar.close - 105.0).abs() < 1e-3);
        assert_eq!(bar.tick_count, 5);
    }

    #[test]
    fn test_force_seal_returns_open_bar_marked_sealed_and_clears() {
        let mut e: CandleEngine<Tf5m> = CandleEngine::new();
        e.on_tick(&make_tick(1000, 100.0, 500, 0));
        let sealed = e.force_seal().expect("force_seal returns the open bar");
        assert!(sealed.sealed);
        assert!((sealed.open - 100.0).abs() < 1e-3);
        assert!(e.latest().is_none());
    }

    #[test]
    fn test_force_seal_on_empty_engine_returns_none() {
        let mut e: CandleEngine<Tf1s> = CandleEngine::new();
        assert!(e.force_seal().is_none());
    }

    #[test]
    fn test_cascade_on_sealed_bar_opens_first_bar() {
        let mut e: CandleEngine<Tf1m> = CandleEngine::new();
        let input = Bar {
            bucket_start_ist_secs: 1000,
            bucket_end_ist_secs: 1001,
            open: 100.0,
            high: 102.0,
            low: 99.0,
            close: 101.0,
            volume: 50,
            volume_cum_day_at_end: 500,
            oi: 1000,
            tick_count: 5,
            security_id: 1234,
            exchange_segment_code: 1,
            sealed: true,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        };
        let result = e.on_sealed_bar(&input);
        assert!(result.is_none());
        let bar = e.latest().unwrap();
        assert_eq!(bar.bucket_start_ist_secs, 960);
        assert!((bar.open - 100.0).abs() < 1e-3);
        assert_eq!(bar.tick_count, 5);
    }

    #[test]
    fn test_cascade_volume_accumulates() {
        let mut e: CandleEngine<Tf1m> = CandleEngine::new();
        let mut bar1 = Bar {
            bucket_start_ist_secs: 1000,
            bucket_end_ist_secs: 1001,
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 50,
            volume_cum_day_at_end: 500,
            oi: 1000,
            tick_count: 5,
            security_id: 1234,
            exchange_segment_code: 1,
            sealed: true,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        };
        e.on_sealed_bar(&bar1);
        bar1.bucket_start_ist_secs = 1010;
        bar1.bucket_end_ist_secs = 1011;
        bar1.volume = 30;
        bar1.volume_cum_day_at_end = 600;
        bar1.tick_count = 3;
        e.on_sealed_bar(&bar1);
        let bar = e.latest().unwrap();
        assert_eq!(bar.volume, 80);
        assert_eq!(bar.volume_cum_day_at_end, 600);
        assert_eq!(bar.tick_count, 8);
    }

    #[test]
    fn test_cascade_boundary_crossing_seals_previous() {
        let mut e: CandleEngine<Tf1m> = CandleEngine::new();
        let bar1 = Bar {
            bucket_start_ist_secs: 1000,
            bucket_end_ist_secs: 1001,
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 50,
            volume_cum_day_at_end: 500,
            oi: 1000,
            tick_count: 5,
            security_id: 1234,
            exchange_segment_code: 1,
            sealed: true,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        };
        e.on_sealed_bar(&bar1);
        let bar2 = Bar {
            bucket_start_ist_secs: 1020,
            bucket_end_ist_secs: 1021,
            open: 110.0,
            high: 110.0,
            low: 110.0,
            close: 110.0,
            volume: 30,
            volume_cum_day_at_end: 600,
            oi: 1100,
            tick_count: 3,
            security_id: 1234,
            exchange_segment_code: 1,
            sealed: true,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        };
        let sealed = e
            .on_sealed_bar(&bar2)
            .expect("crossing 1m boundary must seal previous bar");
        assert!(sealed.sealed);
        assert_eq!(sealed.bucket_start_ist_secs, 960);
        let new_bar = e.latest().unwrap();
        assert_eq!(new_bar.bucket_start_ist_secs, 1020);
        assert_eq!(new_bar.tick_count, 3);
    }

    #[test]
    fn test_bar_carries_composite_key_for_ip111_routing() {
        let mut e: CandleEngine<Tf1s> = CandleEngine::new();
        let mut tick = make_tick(1000, 100.0, 500, 0);
        tick.security_id = 27;
        tick.exchange_segment_code = 0;
        e.on_tick(&tick);
        let bar = e.latest().unwrap();
        assert_eq!(bar.security_id, 27);
        assert_eq!(bar.exchange_segment_code, 0);
    }

    #[test]
    fn test_on_tick_returns_some_bar_only_on_boundary_cross() {
        let mut e: CandleEngine<Tf5s> = CandleEngine::new();
        assert!(e.on_tick(&make_tick(1000, 100.0, 1, 0)).is_none());
        assert!(e.on_tick(&make_tick(1003, 101.0, 2, 0)).is_none());
        assert!(e.on_tick(&make_tick(1005, 102.0, 3, 0)).is_some());
    }

    #[test]
    fn test_latest_returns_open_bar_or_none_after_force_seal() {
        let mut e: CandleEngine<Tf1s> = CandleEngine::new();
        assert!(e.latest().is_none());
        e.on_tick(&make_tick(1000, 50.0, 100, 0));
        assert!(e.latest().is_some());
        e.force_seal();
        assert!(e.latest().is_none());
    }

    #[test]
    fn test_engine_handles_10k_ticks_no_panic() {
        let mut e: CandleEngine<Tf1s> = CandleEngine::new();
        let mut sealed_count = 0_u32;
        for i in 0..10_000_u32 {
            let result = e.on_tick(&make_tick(1000 + i, 100.0 + (i as f32) * 0.001, i, 0));
            if result.is_some() {
                sealed_count = sealed_count.saturating_add(1);
            }
        }
        assert_eq!(sealed_count, 9_999);
    }
}
