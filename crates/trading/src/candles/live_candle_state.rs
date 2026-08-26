//! `LiveCandleState` — the shared per-bucket OHLCV state struct.
//!
//! Extracted 2026-07-17 (stage-3 dead-WS sweep) from the DELETED
//! `aggregator_cell.rs` (the publisher-less 21-TF TICK aggregator died
//! with the live-feed retirements — Dhan 2026-07-13, Groww 2026-07-15).
//! The struct itself is load-bearing across the SURVIVING seal chain:
//! it is the payload of [`crate::candles::BufferedSeal`], consumed by the
//! storage seal-writer chain (`seal_writer_loop` / `ShadowCandleWriter` /
//! spill / DLQ) and PRODUCED today only by the REST-era candle fold
//! (`crates/app/src/rest_candle_fold.rs` — FOLD-01), which constructs it
//! from official `spot_1m_rest` bars. The tick-fold constructors
//! (`from_first_tick` / `fold_in_bucket` / `fold_late_hlc`) died with the
//! aggregator; construction is now literal-field (all fields `pub`).
//!
//! Field semantics are UNCHANGED from the deleted cell (the QuestDB
//! `candles_<tf>` column contract depends on them — see
//! `shadow_seal_columns.rs`).

/// Per-bucket live candle state (one open bucket of one timeframe).
///
/// The 3 Wave-5 pct fields plus `open_pct` / `open_gap_pct` stay `0.0`
/// in the REST-era runtime — the seal-time pct-stamping primitives were
/// removed with the `PrevDayCache` feeder (dead-code cleanup — BATCH-5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveCandleState {
    /// Bucket-open IST epoch second (aligned to TF boundary).
    /// `0` means "slot never opened" — the empty/initial state.
    pub bucket_start_ist_secs: u32,
    /// Open price of this bucket.
    pub open: f64,
    /// Running high.
    pub high: f64,
    /// Running low.
    pub low: f64,
    /// Close (last folded price).
    pub close: f64,
    /// **Incremental** volume within this bucket.
    pub volume: u64,
    /// Cumulative-volume snapshot at bucket-open. Set ONCE per bucket
    /// open; retained for column-contract compatibility.
    pub bucket_start_cumulative: u64,
    /// Open Interest snapshot from the latest fold.
    pub oi: i64,
    /// Number of source rows/ticks folded into this bucket.
    pub tick_count: u32,
    /// IST epoch secs of the fold that set the current `close`.
    pub close_ts_ist_secs: u32,
    /// Previous-day close baseline (last non-zero value wins — a blank
    /// pre-market `0` never clobbers a real baseline). Feeds
    /// `close_pct_from_prev_day` at seal.
    pub prev_day_close: f64,
    /// `close - prev_day.close` / `prev_day.close` * 100.0. Stamped at
    /// seal time.
    pub close_pct_from_prev_day: f64,
    /// `oi - prev_day.oi` / `prev_day.oi` * 100.0. Stamped at seal time.
    pub oi_pct_from_prev_day: f64,
    /// `volume / prev_day.volume * 100.0`. Stamped at seal time.
    pub volume_pct_from_prev_day: f64,
    /// Today's SESSION open (the official 09:15 open). Static per trading
    /// day; last non-zero value wins. Feeds `open_pct` at seal.
    pub session_open: f64,
    /// `(close - session_open) / session_open * 100.0` — % change vs the
    /// official 09:15 open. Stamped at seal time. `0.0` if `session_open`
    /// is `0.0` (div-by-zero guard).
    pub open_pct: f64,
    /// `(session_open - prev_day_close) / prev_day_close * 100.0` — the
    /// OPENING GAP % (gap-up positive, gap-down negative). Stamped at
    /// seal time. `0.0` if `prev_day_close` is `0.0` (div-by-zero guard).
    pub open_gap_pct: f64,
}

impl LiveCandleState {
    /// Empty/initial state — `bucket_start_ist_secs == 0` flags the
    /// "never opened" sentinel.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            bucket_start_ist_secs: 0,
            open: 0.0,
            high: f64::NEG_INFINITY,
            low: f64::INFINITY,
            close: 0.0,
            volume: 0,
            bucket_start_cumulative: 0,
            oi: 0,
            tick_count: 0,
            close_ts_ist_secs: 0,
            prev_day_close: 0.0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
            session_open: 0.0,
            open_pct: 0.0,
            open_gap_pct: 0.0,
        }
    }

    /// Returns `true` if this slot has never been folded into (the
    /// boot/empty state). [`Self::bucket_start_ist_secs`] is the cheap
    /// check.
    #[inline]
    #[must_use]
    pub const fn is_uninitialised(&self) -> bool {
        self.bucket_start_ist_secs == 0
    }

    /// Stamps the three seal-time percentage columns from the baselines this
    /// bar has been carrying all along.
    ///
    /// **ADDED 2026-08-26, and the reason is worth more than the code.** The
    /// three fields below have existed since the Wave-5 seal-column work,
    /// their doc comments have said "Stamped at seal time" the whole time,
    /// `ShadowSealRow::from_buffered_seal` has copied them into the ILP row
    /// the whole time, and the columns have been in the candle DDL the whole
    /// time. **Nothing ever computed them.** `open_bucket` set all three to
    /// `0.0` and no other production line assigned any of them, so every
    /// candle ever written carried three zeros.
    ///
    /// Measured on the live box, 26 Aug 2026, session only: **17,409,304
    /// bars across six frames, zero of them with a non-zero `open_pct` or
    /// `open_gap_pct`.** That is the false-OK class in its purest form — a
    /// consumer reading `open_pct = 0` concludes "this instrument has not
    /// moved", not "this was never computed", and there is nothing in the
    /// row to tell the two apart.
    ///
    /// The baselines were never the problem: `prev_day_close` and
    /// `session_open` are refreshed from the exchange's own fields on every
    /// fold (last-non-zero-wins), and in the minute sampled all 189,396
    /// ticks carried both. Only the division was missing.
    ///
    /// # Which column is which (operator, 2026-08-26 — he corrected me)
    ///
    /// I first labelled these the other way round and he caught it:
    ///
    /// > "what eprcenatge change shdou l chekc with rpevd ay close right dude
    /// > … but for only for pre open 9.15 am open prcoe comapred with evry
    /// > minute or seocdn closed rpcoe"
    ///
    /// | Column | Question it answers | His name for it |
    /// |---|---|---|
    /// | `close_pct_from_prev_day` | close vs YESTERDAY'S CLOSE | **percentage change** |
    /// | `open_pct` | close vs TODAY'S 09:15 OPEN | **pre-open percentage change** |
    /// | `open_gap_pct` | 09:15 open vs yesterday's close | the overnight gap |
    ///
    /// His naming is the coherent one and mine was not. "Percentage change"
    /// on a market screen means change on the previous close — the market
    /// convention. And "pre-open" is his own name for the 09:15 open, because
    /// that price IS the pre-open call-auction equilibrium (his rule, stated
    /// three times: *"the finalised pre open 9.12 close price as 9.15 am open
    /// price"*). So "pre-open percentage" reads as "how far this bar has
    /// moved from the pre-open-determined open", which is exactly `open_pct`.
    ///
    /// The third column stays computed because it is free and it is a real
    /// question — but it is the GAP, not the pre-open percentage, and calling
    /// it that is what I got wrong.
    ///
    /// # Why zero stays the "not computable" answer
    ///
    /// A bar whose baseline never arrived stamps `0.0`, exactly as before.
    /// That is deliberate: zero is already this column's sentinel across
    /// every historical row, and inventing a different one (`NaN`, a
    /// negative flag) would break every existing reader to express something
    /// no reader currently asks. The honest signal for "no baseline" is the
    /// baseline column itself, which is also zero.
    ///
    /// # Complexity
    /// O(1) — three divisions on fields already in this struct. Zero
    /// allocation. Runs once per SEAL, never once per tick.
    #[inline]
    pub fn stamp_seal_percentages(&mut self) {
        self.close_pct_from_prev_day = pct_change(self.close, self.prev_day_close);
        self.open_pct = pct_change(self.close, self.session_open);
        self.open_gap_pct = pct_change(self.session_open, self.prev_day_close);
    }
}

/// `(value - baseline) / baseline * 100`, or `0.0` when that is not a
/// meaningful question to ask.
///
/// Four refusals, each for a reason that has bitten this repository before:
///
/// - **Baseline not finite** — `NaN`/`inf` propagate silently through a
///   division and land in a persisted column looking like a real percentage.
/// - **Baseline not strictly positive** — zero is the documented "no
///   baseline yet" sentinel, and a NEGATIVE baseline would flip the sign of
///   the result, so a fall would persist as a rise.
/// - **Value not finite** — same propagation hazard from the other operand.
/// - **Quotient not finite** — reachable from a subnormal baseline that
///   passes the positivity test and still overflows the division.
///
/// Every refusal returns the SAME value the column held before this function
/// existed, so a refusal can never be worse than the status quo.
#[inline]
#[must_use]
fn pct_change(value: f64, baseline: f64) -> f64 {
    if !baseline.is_finite() || baseline <= 0.0 || !value.is_finite() {
        return 0.0;
    }
    let pct = (value - baseline) / baseline * 100.0;
    if pct.is_finite() { pct } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::{LiveCandleState, pct_change};

    /// Builds a sealed-shaped state carrying real baselines.
    fn sealed(close: f64, session_open: f64, prev_day_close: f64) -> LiveCandleState {
        LiveCandleState {
            bucket_start_ist_secs: 33_300,
            close,
            session_open,
            prev_day_close,
            ..LiveCandleState::empty()
        }
    }

    #[test]
    fn test_empty_is_uninitialised_sentinel() {
        let s = LiveCandleState::empty();
        assert!(s.is_uninitialised());
        assert_eq!(s.bucket_start_ist_secs, 0);
        assert_eq!(s.tick_count, 0);
        assert_eq!(s.volume, 0);
        // Extreme sentinels so the first fold's min/max always win.
        assert!(s.high.is_infinite() && s.high < 0.0);
        assert!(s.low.is_infinite() && s.low > 0.0);
    }

    #[test]
    fn test_opened_state_is_not_uninitialised() {
        let s = LiveCandleState {
            bucket_start_ist_secs: 33_300, // 09:15:00 IST secs-of-day-shaped value
            ..LiveCandleState::empty()
        };
        assert!(!s.is_uninitialised());
    }
    /// The live NIFTY numbers from 2026-08-26, used as the fixture precisely
    /// so this test fails if the arithmetic ever drifts from what the
    /// operator was shown.
    ///
    /// Read from production: yesterday's close 24,334.55; today's official
    /// 09:15 open 24,341.95; price at 15:19 IST 24,273.15.
    #[test]
    fn the_live_nifty_numbers_produce_the_percentages_the_operator_was_shown() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.stamp_seal_percentages();

        // PRE-OPEN percentage change: down 0.283% from the 09:15 open.
        assert!(
            (s.open_pct - -0.282_63).abs() < 0.000_5,
            "pre-open pct was {}",
            s.open_pct
        );
        // The overnight GAP: the 09:15 open was 0.030% above yesterday's
        // close. This is NOT what he calls the pre-open percentage.
        assert!(
            (s.open_gap_pct - 0.030_41).abs() < 0.000_5,
            "gap pct was {}",
            s.open_gap_pct
        );
        // PERCENTAGE CHANGE: versus yesterday's close. The headline
        // number, and the market convention.
        assert!(
            (s.close_pct_from_prev_day - -0.252_52).abs() < 0.000_5,
            "percentage change was {}",
            s.close_pct_from_prev_day
        );
    }

    /// The three columns are three DIFFERENT questions, and an instrument can
    /// be strong on one and weak on another. Varun Beverages did exactly this
    /// on 2026-08-26: it gapped up 2.26% overnight and then fell 5.93% from
    /// that open — positive gap, negative pre-open percentage, on the same
    /// day. That is the whole reason both columns are worth carrying.
    #[test]
    fn a_gap_up_that_then_falls_reports_opposite_signs_on_the_two_columns() {
        let mut s = sealed(94.07, 102.26, 100.0);
        s.stamp_seal_percentages();
        assert!(
            s.open_gap_pct > 2.0,
            "gap should be positive: {}",
            s.open_gap_pct
        );
        assert!(
            s.open_pct < -5.0,
            "intraday should be negative: {}",
            s.open_pct
        );
    }

    #[test]
    fn an_unmoved_price_stamps_exactly_zero_not_a_rounding_artefact() {
        let mut s = sealed(24_341.95, 24_341.95, 24_341.95);
        s.stamp_seal_percentages();
        assert_eq!(s.open_pct, 0.0);
        assert_eq!(s.open_gap_pct, 0.0);
        assert_eq!(s.close_pct_from_prev_day, 0.0);
    }

    /// Pre-open, and for indices for the first several ticks of every
    /// morning, the exchange sends no baseline at all. Zero in, zero out —
    /// the same value the column held before this code existed, so a
    /// refusal can never be worse than the status quo.
    #[test]
    fn a_missing_baseline_stamps_zero_exactly_as_before() {
        let mut s = sealed(24_273.15, 0.0, 0.0);
        s.stamp_seal_percentages();
        assert_eq!(s.open_pct, 0.0);
        assert_eq!(s.open_gap_pct, 0.0);
        assert_eq!(s.close_pct_from_prev_day, 0.0);
    }

    /// A NEGATIVE baseline is the dangerous one: the division still yields a
    /// finite number, but with the sign flipped, so a fall would persist as
    /// a rise. Refused rather than trusted.
    #[test]
    fn a_negative_baseline_is_refused_not_sign_flipped() {
        assert_eq!(pct_change(110.0, -100.0), 0.0);
    }

    #[test]
    fn non_finite_operands_never_reach_a_persisted_column() {
        assert_eq!(pct_change(f64::NAN, 100.0), 0.0);
        assert_eq!(pct_change(f64::INFINITY, 100.0), 0.0);
        assert_eq!(pct_change(f64::NEG_INFINITY, 100.0), 0.0);
        assert_eq!(pct_change(100.0, f64::NAN), 0.0);
        assert_eq!(pct_change(100.0, f64::INFINITY), 0.0);
    }

    /// A subnormal baseline passes `> 0.0` and still overflows the division.
    /// The finite check on the QUOTIENT is what catches it — the operand
    /// checks alone are not enough.
    #[test]
    fn a_subnormal_baseline_that_overflows_the_division_is_refused() {
        let out = pct_change(1.0, f64::MIN_POSITIVE / 2.0);
        assert!(
            out == 0.0 || out.is_finite(),
            "a subnormal baseline produced {out}"
        );
        assert_eq!(pct_change(f64::MAX, 5e-324), 0.0);
    }

    #[test]
    fn a_price_of_zero_against_a_real_baseline_reports_minus_one_hundred() {
        // An option going to zero is an ordinary expiry-day outcome, not an
        // error, and -100% is the correct answer for it.
        let mut s = sealed(0.0, 5.6, 5.6);
        s.stamp_seal_percentages();
        assert!((s.open_pct - -100.0).abs() < f64::EPSILON);
    }

    /// Stamping twice must not compound — the amended-late path re-stamps an
    /// already-stamped bar every time a late tick moves the close.
    #[test]
    fn stamping_twice_is_idempotent_for_an_unchanged_close() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.stamp_seal_percentages();
        let first = (s.open_pct, s.open_gap_pct, s.close_pct_from_prev_day);
        s.stamp_seal_percentages();
        assert_eq!(
            first,
            (s.open_pct, s.open_gap_pct, s.close_pct_from_prev_day)
        );
    }

    /// The dropped columns stay dropped: their DDL columns were removed in
    /// 2026-05-28 (spot has no OI, indices have no volume), so computing them
    /// would fill fields nothing reads.
    #[test]
    fn the_dropped_volume_and_oi_percentages_are_deliberately_not_stamped() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.volume = 1_000;
        s.oi = 500;
        s.stamp_seal_percentages();
        assert_eq!(s.volume_pct_from_prev_day, 0.0);
        assert_eq!(s.oi_pct_from_prev_day, 0.0);
    }
}
