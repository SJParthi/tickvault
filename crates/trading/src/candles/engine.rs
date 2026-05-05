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
//! ## What this module does NOT ship (deliberate scope)
//!
//! - Wiring into `tick_processor.rs` (Phase 3 commit 2)
//! - The 28-engine cascade SPSC plumbing (Phase 3 commit 3)
//! - Parity tests against live `candles_*` matviews (Phase 3 commit 4 —
//!   gated on 7-day soak per plan §6)
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
    /// Bucket-aligned start time in IST seconds-of-epoch (the time
    /// boundary the bar opened on). For a 1s bar this equals the
    /// tick's exchange_timestamp; for a 5m bar it's the start of the
    /// 5-minute window the tick fell into.
    pub bucket_start_ist_secs: u32,
    /// Bucket-aligned end time = bucket_start + tf_secs.
    pub bucket_end_ist_secs: u32,
    /// First tick's price (open).
    pub open: f64,
    /// Highest LTP seen during the bar.
    pub high: f64,
    /// Lowest LTP seen during the bar.
    pub low: f64,
    /// Last tick's price (close).
    pub close: f64,
    /// Sum of `volume_delta` across the bar (per-tick increments).
    /// Phase 2 lifecycle column — first-seen ticks contribute the
    /// full cum-day total; downstream readers should use
    /// `volume_cum_day_at_end` for absolute totals.
    pub volume: i64,
    /// Last tick's cumulative-day volume (for Top Volume mover ranking).
    pub volume_cum_day_at_end: i64,
    /// Last tick's open interest. Aggregated as `last(oi)` to match
    /// QuestDB matview semantics.
    pub oi: i64,
    /// Number of ticks folded into this bar. Useful for sparse-data
    /// detection.
    pub tick_count: u32,
    /// Security ID this bar belongs to.
    pub security_id: u32,
    /// Exchange segment code (per `ParsedTick::exchange_segment_code`).
    pub exchange_segment_code: u8,
    /// `true` once the bar has been sealed (the next tick crossed a
    /// boundary). A non-sealed bar is the open bar still accumulating.
    pub sealed: bool,
}

impl Bar {
    /// Constructs an empty Bar at the given bucket start. Used by the
    /// engine when opening a new bucket — internal helper.
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
        }
    }
}

/// Trait describing a single timeframe's bucketing math.
///
/// Implementations are zero-sized marker structs (no runtime data) so
/// `CandleEngine<TF>` is monomorphized at compile time and the
/// boundary-check arithmetic inlines into a single integer modulo.
pub trait Timeframe {
    /// Number of IST seconds in one bucket of this timeframe. For
    /// fixed-second TFs (1s, 5s, 1m, ...) this is a compile-time
    /// constant. For variable-length TFs (1d, 1w, 1mo) the engine
    /// will need a different bucketing strategy — those are added in
    /// later commits with a separate `bucket_for(secs) -> (start, end)`
    /// method.
    const SECS: u32;

    /// Human-readable name written into log lines + matview names
    /// (e.g. `"1s"`, `"5m"`, `"1h"`).
    const NAME: &'static str;

    /// Computes the bucket start (in IST seconds-of-epoch) for the
    /// given tick timestamp. Default impl works for fixed-second TFs
    /// where buckets align to integer multiples of `SECS`.
    #[inline]
    fn bucket_start(tick_ist_secs: u32) -> u32 {
        tick_ist_secs - (tick_ist_secs % Self::SECS)
    }

    /// Bucket end = bucket_start + SECS.
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

/// 5-second timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf5s;
impl Timeframe for Tf5s {
    const SECS: u32 = 5;
    const NAME: &'static str = "5s";
}

/// 15-second timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf15s;
impl Timeframe for Tf15s {
    const SECS: u32 = 15;
    const NAME: &'static str = "15s";
}

/// 30-second timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf30s;
impl Timeframe for Tf30s {
    const SECS: u32 = 30;
    const NAME: &'static str = "30s";
}

/// 1-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf1m;
impl Timeframe for Tf1m {
    const SECS: u32 = 60;
    const NAME: &'static str = "1m";
}

/// 5-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf5m;
impl Timeframe for Tf5m {
    const SECS: u32 = 300;
    const NAME: &'static str = "5m";
}

/// 15-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf15m;
impl Timeframe for Tf15m {
    const SECS: u32 = 900;
    const NAME: &'static str = "15m";
}

// ────────────────────────────────────────────────────────────────────
// Phase 3 commit 4: 22 additional Timeframe marker types covering the
// full 29-TF universe per plan §1 L3:
//   1s/3s/5s/10s/15s/30s, 1m..15m, 30m, 1h, 2h, 3h, 4h, 1d, 1w, 1mo
// (1s/5s/15s/30s/1m/5m/15m above already shipped in Phase 3 commit 1.)
//
// Day/week/month TFs use SECS-based bucketing (86_400 / 604_800 /
// 30 × 86_400). Calendar-aware bucketing (IST-midnight aligned, Sat/Sun
// skipped, exact month-end) is a follow-on refinement — the cascade
// SPSC plumbing in this PR does not require it. Bars produced today
// align to the seconds-since-epoch grid which is "approximately right"
// for 1d (UTC midnight, off from IST by 5h30m) and crudely right for
// 1w / 1mo. Pinned by per-TF unit tests below.
// ────────────────────────────────────────────────────────────────────

/// 3-second timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf3s;
impl Timeframe for Tf3s {
    const SECS: u32 = 3;
    const NAME: &'static str = "3s";
}

/// 10-second timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf10s;
impl Timeframe for Tf10s {
    const SECS: u32 = 10;
    const NAME: &'static str = "10s";
}

/// 2-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf2m;
impl Timeframe for Tf2m {
    const SECS: u32 = 120;
    const NAME: &'static str = "2m";
}

/// 3-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf3m;
impl Timeframe for Tf3m {
    const SECS: u32 = 180;
    const NAME: &'static str = "3m";
}

/// 4-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf4m;
impl Timeframe for Tf4m {
    const SECS: u32 = 240;
    const NAME: &'static str = "4m";
}

/// 6-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf6m;
impl Timeframe for Tf6m {
    const SECS: u32 = 360;
    const NAME: &'static str = "6m";
}

/// 7-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf7m;
impl Timeframe for Tf7m {
    const SECS: u32 = 420;
    const NAME: &'static str = "7m";
}

/// 8-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf8m;
impl Timeframe for Tf8m {
    const SECS: u32 = 480;
    const NAME: &'static str = "8m";
}

/// 9-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf9m;
impl Timeframe for Tf9m {
    const SECS: u32 = 540;
    const NAME: &'static str = "9m";
}

/// 10-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf10m;
impl Timeframe for Tf10m {
    const SECS: u32 = 600;
    const NAME: &'static str = "10m";
}

/// 11-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf11m;
impl Timeframe for Tf11m {
    const SECS: u32 = 660;
    const NAME: &'static str = "11m";
}

/// 12-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf12m;
impl Timeframe for Tf12m {
    const SECS: u32 = 720;
    const NAME: &'static str = "12m";
}

/// 13-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf13m;
impl Timeframe for Tf13m {
    const SECS: u32 = 780;
    const NAME: &'static str = "13m";
}

/// 14-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf14m;
impl Timeframe for Tf14m {
    const SECS: u32 = 840;
    const NAME: &'static str = "14m";
}

/// 30-minute timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf30m;
impl Timeframe for Tf30m {
    const SECS: u32 = 1_800;
    const NAME: &'static str = "30m";
}

/// 1-hour timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf1h;
impl Timeframe for Tf1h {
    const SECS: u32 = 3_600;
    const NAME: &'static str = "1h";
}

/// 2-hour timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf2h;
impl Timeframe for Tf2h {
    const SECS: u32 = 7_200;
    const NAME: &'static str = "2h";
}

/// 3-hour timeframe.
#[derive(Clone, Copy, Debug)]
pub struct Tf3h;
impl Timeframe for Tf3h {
    const SECS: u32 = 10_800;
    const NAME: &'static str = "3h";
}

/// 4-hour timeframe.
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

/// Generic single-timeframe candle engine.
///
/// `on_tick` returns `Some(sealed_bar)` when the incoming tick crosses
/// a bucket boundary — caller forwards that to the cascade SPSC
/// channel for derived TFs. Returns `None` while the tick falls into
/// the same bucket as the open bar.
///
/// The engine holds state for ONE security_id. Trading callers
/// instantiate one `CandleEngine` per `(security_id, segment, TF)`
/// triple — typically via a `papaya::HashMap` keyed on the composite
/// `(security_id, segment_code)` (per I-P1-11).
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
    /// Constructs a fresh engine with no open bar.
    pub fn new() -> Self {
        Self {
            open_bar: None,
            _tf: std::marker::PhantomData,
        }
    }

    /// Folds a tick into the open bar; if the tick crosses a bucket
    /// boundary, returns the sealed previous bar (with `sealed=true`)
    /// and starts a new open bar.
    ///
    /// O(1), zero-alloc — hot-path safe.
    #[inline]
    pub fn on_tick(&mut self, tick: &ParsedTick) -> Option<Bar> {
        let tick_ist_secs = tick.exchange_timestamp;
        let new_bucket_start = TF::bucket_start(tick_ist_secs);
        let new_bucket_end = TF::bucket_end(new_bucket_start);
        let ltp_f64 = f64::from(tick.last_traded_price);

        match self.open_bar {
            None => {
                // First tick — open the bar.
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
                bar.volume = 0; // first-seen — caller's volume_delta upstream is the cum
                bar.volume_cum_day_at_end = i64::from(tick.volume);
                bar.oi = i64::from(tick.open_interest);
                bar.tick_count = 1;
                self.open_bar = Some(bar);
                None
            }
            Some(ref mut bar) => {
                if new_bucket_start == bar.bucket_start_ist_secs {
                    // Same bucket — fold tick in.
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
                    // Crossed boundary — seal the previous bar and start
                    // a fresh one.
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

    /// Folds a sealed bar from a finer TF into this engine's open bar
    /// (cascade input). Used by the SPSC consumer that drives the 28
    /// derived engines from sealed `1s` bars.
    ///
    /// Returns `Some(sealed_bar)` if folding the input crossed a
    /// boundary in THIS TF; `None` if it stayed in the same bucket.
    ///
    /// O(1), zero-alloc.
    #[inline]
    pub fn on_sealed_bar(&mut self, input: &Bar) -> Option<Bar> {
        // The cascade input is a sealed bar whose timestamp is the
        // bucket_end (i.e. the moment it closed). We classify by
        // bucket_start of the input — bars fall into THIS TF's bucket
        // by their start time.
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
                    // Same bucket — fold input in. open stays from the
                    // first input; close = latest input's close.
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

    /// Returns the current open bar (or `None` if no ticks have
    /// arrived yet). The trading bot uses this for lock-free RAM
    /// reads.
    #[inline]
    pub fn latest(&self) -> Option<Bar> {
        self.open_bar
    }

    /// Forces the open bar to be sealed and returned (e.g. at IST
    /// midnight rollover). Sets `sealed=true` on the returned Bar
    /// and clears the engine's state so the next tick opens a fresh
    /// bucket.
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
    fn test_tf_secs_constants_are_correct() {
        assert_eq!(Tf1s::SECS, 1);
        assert_eq!(Tf5s::SECS, 5);
        assert_eq!(Tf15s::SECS, 15);
        assert_eq!(Tf30s::SECS, 30);
        assert_eq!(Tf1m::SECS, 60);
        assert_eq!(Tf5m::SECS, 300);
        assert_eq!(Tf15m::SECS, 900);
    }

    /// Hostile review C2: pin the calendar-approximate warnings on
    /// Tf1d / Tf1w / Tf1mo so a future doc edit cannot silently drop
    /// them. Source-scan against the engine.rs file at test time.
    #[test]
    fn test_calendar_approximate_tfs_carry_explicit_warning_in_docstring() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/candles/engine.rs"
        ))
        .expect("must be able to read engine.rs");
        // The exact phrase pattern guards both the "WARN — calendar-approximate"
        // tag and the "NOT for trading signals" message — together they form
        // the contract.
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
        // Phase 3 commit 4: 22 additional TFs covering the full 29-TF universe.
        assert_eq!(Tf3s::SECS, 3);
        assert_eq!(Tf10s::SECS, 10);
        assert_eq!(Tf2m::SECS, 120);
        assert_eq!(Tf3m::SECS, 180);
        assert_eq!(Tf4m::SECS, 240);
        assert_eq!(Tf6m::SECS, 360);
        assert_eq!(Tf7m::SECS, 420);
        assert_eq!(Tf8m::SECS, 480);
        assert_eq!(Tf9m::SECS, 540);
        assert_eq!(Tf10m::SECS, 600);
        assert_eq!(Tf11m::SECS, 660);
        assert_eq!(Tf12m::SECS, 720);
        assert_eq!(Tf13m::SECS, 780);
        assert_eq!(Tf14m::SECS, 840);
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
        assert_eq!(Tf2m::NAME, "2m");
        assert_eq!(Tf3m::NAME, "3m");
        assert_eq!(Tf4m::NAME, "4m");
        assert_eq!(Tf6m::NAME, "6m");
        assert_eq!(Tf7m::NAME, "7m");
        assert_eq!(Tf8m::NAME, "8m");
        assert_eq!(Tf9m::NAME, "9m");
        assert_eq!(Tf10m::NAME, "10m");
        assert_eq!(Tf11m::NAME, "11m");
        assert_eq!(Tf12m::NAME, "12m");
        assert_eq!(Tf13m::NAME, "13m");
        assert_eq!(Tf14m::NAME, "14m");
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
        // 1s: every second is its own bucket
        assert_eq!(Tf1s::bucket_start(1000), 1000);
        assert_eq!(Tf1s::bucket_start(1001), 1001);
        // 5s: aligns to multiples of 5
        assert_eq!(Tf5s::bucket_start(1003), 1000);
        assert_eq!(Tf5s::bucket_start(1005), 1005);
        // 1m: aligns to multiples of 60
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
        assert!(result.is_none(), "first tick must not seal a previous bar");
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
        assert!(result.is_none(), "same-bucket tick must not seal");
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
        // Boundary crossing: 1005 falls into bucket [1005, 1010).
        let sealed = e
            .on_tick(&make_tick(1005, 110.0, 1000, 0))
            .expect("tick at boundary must seal previous bar");
        assert!(sealed.sealed);
        assert_eq!(sealed.bucket_start_ist_secs, 1000);
        assert!((sealed.open - 100.0).abs() < 1e-3);
        assert!((sealed.high - 102.0).abs() < 1e-3);
        assert!((sealed.close - 102.0).abs() < 1e-3);
        assert_eq!(sealed.tick_count, 2);
        // New open bar is at 1005.
        let new_bar = e.latest().unwrap();
        assert_eq!(new_bar.bucket_start_ist_secs, 1005);
        assert!((new_bar.open - 110.0).abs() < 1e-3);
        assert_eq!(new_bar.tick_count, 1);
        assert!(!new_bar.sealed);
    }

    #[test]
    fn test_high_low_track_extremes_across_ticks() {
        // Tf1m buckets align to multiples of 60. Use ticks all within
        // bucket [1020, 1080) so they fold into one open bar.
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
        // Engine state cleared.
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
        };
        let result = e.on_sealed_bar(&input);
        assert!(result.is_none());
        let bar = e.latest().unwrap();
        assert_eq!(bar.bucket_start_ist_secs, 960); // 1000 - (1000 % 60) = 960
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
        };
        e.on_sealed_bar(&bar1);
        bar1.bucket_start_ist_secs = 1010;
        bar1.bucket_end_ist_secs = 1011;
        bar1.volume = 30;
        bar1.volume_cum_day_at_end = 600;
        bar1.tick_count = 3;
        e.on_sealed_bar(&bar1);
        let bar = e.latest().unwrap();
        assert_eq!(
            bar.volume, 80,
            "volume must accumulate across cascade input"
        );
        assert_eq!(
            bar.volume_cum_day_at_end, 600,
            "volume_cum_day_at_end is the LAST input's value"
        );
        assert_eq!(bar.tick_count, 8, "tick_count must accumulate");
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
        };
        e.on_sealed_bar(&bar1);
        // Bucket [960, 1020). Now feed a bar at 1020 — different bucket.
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
        };
        let sealed = e
            .on_sealed_bar(&bar2)
            .expect("crossing 1m boundary must seal previous bar");
        assert!(sealed.sealed);
        assert_eq!(sealed.bucket_start_ist_secs, 960);
        // Now the engine's open bar should be the new 1020 bucket [1020, 1080).
        let new_bar = e.latest().unwrap();
        assert_eq!(new_bar.bucket_start_ist_secs, 1020);
        assert_eq!(new_bar.tick_count, 3);
    }

    /// I-P1-11 ratchet: the engine carries the composite-key
    /// `(security_id, segment_code)` from the first tick. Trading
    /// callers route ticks to the correct engine via the composite
    /// key — but the engine itself doesn't enforce it. This test
    /// verifies the bar carries both fields.
    #[test]
    fn test_bar_carries_composite_key_for_ip111_routing() {
        let mut e: CandleEngine<Tf1s> = CandleEngine::new();
        let mut tick = make_tick(1000, 100.0, 500, 0);
        tick.security_id = 27; // FINNIFTY scenario
        tick.exchange_segment_code = 0; // IDX_I
        e.on_tick(&tick);
        let bar = e.latest().unwrap();
        assert_eq!(bar.security_id, 27);
        assert_eq!(bar.exchange_segment_code, 0);
    }

    /// Phase 3 ratchet: explicit name-matched test for `on_tick`
    /// (pub-fn-test guard requires the test name contain the fn name).
    #[test]
    fn test_on_tick_returns_some_bar_only_on_boundary_cross() {
        let mut e: CandleEngine<Tf5s> = CandleEngine::new();
        // First tick — must NOT return a sealed bar.
        assert!(e.on_tick(&make_tick(1000, 100.0, 1, 0)).is_none());
        // Same bucket — still no seal.
        assert!(e.on_tick(&make_tick(1003, 101.0, 2, 0)).is_none());
        // Cross 1005 boundary — must seal.
        assert!(e.on_tick(&make_tick(1005, 102.0, 3, 0)).is_some());
    }

    /// Phase 3 ratchet: explicit name-matched test for `latest`
    /// (pub-fn-test guard).
    #[test]
    fn test_latest_returns_open_bar_or_none_after_force_seal() {
        let mut e: CandleEngine<Tf1s> = CandleEngine::new();
        assert!(e.latest().is_none(), "no ticks → latest is None");
        e.on_tick(&make_tick(1000, 50.0, 100, 0));
        assert!(e.latest().is_some(), "after tick → latest is Some");
        e.force_seal();
        assert!(e.latest().is_none(), "force_seal clears the open bar");
    }

    /// Hot-path zero-alloc property: 10K ticks through the engine
    /// must complete without growing the heap. Without `dhat`, we
    /// can only verify there's no Vec/String/Box allocation by
    /// reading the source. This test exercises the path with a
    /// loop that would surface OOMs/leaks immediately.
    #[test]
    fn test_engine_handles_10k_ticks_no_panic() {
        let mut e: CandleEngine<Tf1s> = CandleEngine::new();
        let mut sealed_count = 0_u32;
        for i in 0..10_000_u32 {
            // Each tick advances ts by 1 second → seals each bar.
            let result = e.on_tick(&make_tick(1000 + i, 100.0 + (i as f32) * 0.001, i, 0));
            if result.is_some() {
                sealed_count = sealed_count.saturating_add(1);
            }
        }
        // 9999 boundary crossings → 9999 sealed bars.
        assert_eq!(sealed_count, 9_999);
    }
}
