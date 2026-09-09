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
    /// Close of the PREVIOUS sealed bar of this timeframe, snapshotted when
    /// THIS bucket opened. `0.0` means "no usable baseline" — first bar of
    /// the session, or a previous bar from an earlier trading day.
    ///
    /// Snapshotted at bucket OPEN, deliberately, not read at seal time. The
    /// late-tick amend path (`fold_late_hlc`) mutates `last_sealed[ord].close`
    /// in place and re-emits ONLY the amended bar. Reading the previous close
    /// at seal time would therefore let an amend to bar N retroactively change
    /// the sign of bar N+1 — which was already written and is never re-emitted.
    /// Snapshotting at open makes that impossible: N+1's baseline is frozen
    /// before N can be amended.
    ///
    /// Occupies the 8 bytes vacated by `oi_pct_from_prev_day`, whose DDL column
    /// was removed 2026-05-28 and which has been permanently `0.0` since (pinned
    /// by `the_dropped_volume_and_oi_percentages_are_deliberately_not_stamped`).
    pub bucket_open_prev_close: f64,
    /// Vendor's total PENDING buy-order quantity in the book, last non-zero
    /// value seen in this bucket. NOT executed volume — these are resting
    /// orders. `0` is the vendor's absent sentinel (Ticker-mode packets carry
    /// no book), so last-NON-ZERO wins, exactly as `oi` does: a lighter packet
    /// must never erase a real reading.
    pub total_buy_qty: u32,
    /// Vendor's total PENDING sell-order quantity. Same contract as
    /// [`Self::total_buy_qty`].
    ///
    /// This field and `total_buy_qty` together occupy the 8 bytes vacated by
    /// `volume_pct_from_prev_day` (same 2026-05-28 removal). The struct is
    /// therefore UNCHANGED at 128 bytes and every downstream size assertion
    /// holds without being raised.
    pub total_sell_qty: u32,
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
            bucket_open_prev_close: 0.0,
            total_buy_qty: 0,
            total_sell_qty: 0,
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

    /// Signed volume, the quantity a broker chart plots as "Net Volume":
    /// `+volume` when this bar closed above the previous bar, `-volume` when
    /// below, `0` when unchanged.
    ///
    /// Returns `None` — persisted as SQL NULL — when the question cannot be
    /// asked. That distinction is the whole reason this returns an `Option`:
    /// `0` here means "the price did not move", and it must NOT also mean
    /// "there was no previous bar". `close_pct_from_prev_day` above collapses
    /// both onto `0.0` because its column has carried that sentinel since the
    /// first row ever written; this column is new, so it can be honest.
    ///
    /// # The four refusals
    ///
    /// - **No baseline** (`bucket_open_prev_close == 0.0`) — the first bar of
    ///   the session, or a previous bar from an earlier trading day. A chart
    ///   has no bar to the left of its first bar either.
    /// - **Untraded bar** (`close == 0.0`) — `0.0` is this pipeline's absent
    ///   price sentinel, not a real price of zero.
    /// - **Non-finite either side** — `NaN` fails BOTH `>` and `<` under
    ///   partial ordering, so an unguarded comparison would silently land on
    ///   the `else` arm and persist `0` (a real "flat" reading) for a poisoned
    ///   input. Refused explicitly instead.
    /// - **Volume beyond the signed ceiling** — negating a `u64` past
    ///   `i64::MAX` wraps POSITIVE, turning a sell bar into a buy bar. The
    ///   saturating conversion below makes that unrepresentable; the same
    ///   hazard is already handled this way on the tick-persistence path.
    ///
    /// # Why the comparison is exact and not a tolerance
    ///
    /// Both sides come from `f32_to_f64_clean`, so an unchanged price is
    /// bit-identical on both and compares equal. A widening `f32 as f64`
    /// would make `10.20` become `10.19999980926514` and report an unchanged
    /// price as a RISE — systematically, on every flat bar. That is why
    /// [`Self::bucket_open_prev_close`] is `f64` and copied verbatim rather
    /// than stored narrow and re-widened.
    ///
    /// # Complexity
    /// O(1) — two compares and one negate on fields already in this struct.
    /// Zero allocation. Runs once per SEAL, never once per tick.
    #[inline]
    #[must_use]
    pub fn net_volume(&self) -> Option<i64> {
        let prev = self.bucket_open_prev_close;
        let close = self.close;
        if prev <= 0.0 || close <= 0.0 || !prev.is_finite() || !close.is_finite() {
            return None;
        }
        // Saturate BEFORE the sign is applied: `-(u64 as i64)` on a value past
        // `i64::MAX` wraps to a positive number, which would persist a sell bar
        // as a buy bar.
        let magnitude = i64::try_from(self.volume).unwrap_or(i64::MAX);
        if close > prev {
            Some(magnitude)
        } else if close < prev {
            Some(-magnitude)
        } else {
            Some(0)
        }
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
    // ROUNDED TO 2 DECIMALS -- operator directive 2026-09-03: "always our
    // percentage should be purely based only two digits after decimal points
    // which should be matched to Dhan".
    //
    // This is the vendor-matching rule, not cosmetics. Dhan publishes
    // percentage change to 2 decimals, and the daily cross-verification
    // compares our aggregation against their tape; a column carrying
    // 0.38198662315043225 where the vendor says 0.38 cannot be compared for
    // equality at all, only with a tolerance nobody has calibrated.
    //
    // Rounded at the SOURCE rather than at each reader: these values are
    // persisted, spilled to disk, and read back by the console, the API and
    // the comparator, and a rounding applied at one reader and not another is
    // how two surfaces come to disagree about the same bar. The precision
    // discarded here is below the tick size of every instrument we carry.
    //
    // `round_to_2dp` is the house helper (`price_precision.rs`) already used
    // for prices; reusing it means percentages and prices can never round by
    // two different rules.
    tickvault_common::price_precision::round_to_2dp(pct_change_raw(value, baseline))
}

/// The unrounded quotient, split out ONLY so a fixture can still pin the
/// arithmetic itself.
///
/// This is not a second code path: `pct_change` is exactly
/// `round_to_2dp(pct_change_raw(..))`, and nothing else calls this. It exists
/// because the 2-decimal rounding costs the live-NIFTY fixture its
/// discriminating power -- rounded, every drift smaller than 0.005 percentage
/// points is invisible to an assertion on the stamped column, so a real
/// arithmetic regression could land green. Keeping the raw value observable
/// lets that fixture assert BOTH: the vendor-matched 2-decimal value a reader
/// sees, and the full-precision quotient that proves the formula did not move.
#[inline]
#[must_use]
fn pct_change_raw(value: f64, baseline: f64) -> f64 {
    if !baseline.is_finite() || baseline <= 0.0 || !value.is_finite() {
        return 0.0;
    }
    let pct = (value - baseline) / baseline * 100.0;
    if !pct.is_finite() {
        return 0.0;
    }
    pct
}

#[cfg(test)]
mod tests {
    use super::{LiveCandleState, pct_change, pct_change_raw};

    /// The four percentage columns must carry AT MOST two decimals, because
    /// Dhan publishes two and the daily cross-verification compares our
    /// aggregation against their tape. Operator directive 2026-09-03.
    ///
    /// MEASURED on the live console the same day, before this rounding:
    /// `close_pct_from_prev_day` read 0.38198662315043225 and `open_gap_pct`
    /// read 0.3491612811500996 -- seventeen digits against the vendor's two.
    #[test]
    fn every_percentage_carries_at_most_two_decimals() {
        // The exact live values that prompted the directive.
        for (value, baseline) in [
            (24_005.8_f64, 23_914.4_f64),
            (24_009.9_f64, 23_914.4_f64),
            (100.0_f64, 3.0_f64),
            (1.0_f64, 3.0_f64),
            (0.1_f64, 99_999.0_f64),
        ] {
            let pct = pct_change(value, baseline);
            let rendered = format!("{pct}");
            if let Some(fraction) = rendered.split_once('.').map(|(_, f)| f) {
                assert!(
                    fraction.len() <= 2,
                    "pct_change({value}, {baseline}) rendered {rendered:?} with \
                     {} decimals -- Dhan publishes 2, and a column the vendor \
                     cannot be compared against for equality is a column the \
                     cross-verification has to guess a tolerance for",
                    fraction.len()
                );
            }
        }
    }

    /// Rounding must not turn a real move into a zero, and must not invent one.
    /// A percentage that rounds to 0.00 is REPORTED as 0.00 -- that is the
    /// vendor's own resolution, not a suppressed signal -- but the sign and
    /// magnitude of anything at or above 0.005% must survive intact.
    #[test]
    fn rounding_preserves_sign_and_does_not_fabricate_movement() {
        assert_eq!(pct_change(101.0, 100.0), 1.0);
        assert_eq!(pct_change(99.0, 100.0), -1.0);
        // 0.004% rounds to zero: below the vendor's own resolution.
        assert_eq!(pct_change(100.004, 100.0), 0.0);
        // 0.006% survives as 0.01, with its sign.
        assert_eq!(pct_change(100.006, 100.0), 0.01);
        assert_eq!(pct_change(99.994, 100.0), -0.01);
        // The guards above the arithmetic are unchanged.
        assert_eq!(pct_change(100.0, 0.0), 0.0);
        assert_eq!(pct_change(f64::NAN, 100.0), 0.0);
        assert_eq!(pct_change(100.0, f64::NAN), 0.0);
    }

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
    ///
    /// # Why this asserts TWICE per column (2026-09-03)
    ///
    /// The 2-decimal rounding the operator ordered would otherwise have
    /// GUTTED this fixture. Its whole purpose is to fail on an arithmetic
    /// drift, and against a rounded column every drift smaller than 0.005
    /// percentage points is invisible: -0.282 63 and -0.284 99 both stamp
    /// -0.28. Loosening the literals to the rounded values and calling it
    /// updated would have left a test that still passes and no longer
    /// guards anything -- the false-OK class this repository keeps
    /// recording.
    ///
    /// So each column is pinned on both sides: the RAW quotient at the
    /// original tolerance, which is the drift detector, and the STAMPED
    /// value at exact equality, which is what a reader and the vendor
    /// comparison actually see. A formula change fails the first; a change
    /// to the rounding rule fails the second.
    #[test]
    fn the_live_nifty_numbers_produce_the_percentages_the_operator_was_shown() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.stamp_seal_percentages();

        // PRE-OPEN percentage change: down 0.283% from the 09:15 open.
        assert!(
            (pct_change_raw(24_273.15, 24_341.95) - -0.282_63).abs() < 0.000_5,
            "pre-open pct drifted: {}",
            pct_change_raw(24_273.15, 24_341.95)
        );
        assert_eq!(s.open_pct, -0.28, "pre-open pct was {}", s.open_pct);

        // The overnight GAP: the 09:15 open was 0.030% above yesterday's
        // close. This is NOT what he calls the pre-open percentage.
        assert!(
            (pct_change_raw(24_341.95, 24_334.55) - 0.030_41).abs() < 0.000_5,
            "gap pct drifted: {}",
            pct_change_raw(24_341.95, 24_334.55)
        );
        assert_eq!(s.open_gap_pct, 0.03, "gap pct was {}", s.open_gap_pct);

        // PERCENTAGE CHANGE: versus yesterday's close. The headline
        // number, and the market convention.
        assert!(
            (pct_change_raw(24_273.15, 24_334.55) - -0.252_52).abs() < 0.000_5,
            "percentage change drifted: {}",
            pct_change_raw(24_273.15, 24_334.55)
        );
        assert_eq!(
            s.close_pct_from_prev_day, -0.25,
            "percentage change was {}",
            s.close_pct_from_prev_day
        );
    }

    /// The rounded wrapper and the raw helper must never be two different
    /// formulas -- the split exists only so the fixture above can see the
    /// unrounded value, and a second code path is exactly what would make it
    /// lie. Swept across the refusal boundaries as well as ordinary values.
    #[test]
    fn the_rounded_column_is_always_the_rounded_raw_value() {
        for (value, baseline) in [
            (24_273.15_f64, 24_341.95_f64),
            (24_341.95, 24_334.55),
            (94.07, 102.26),
            (100.0, 100.0),
            (0.0, 100.0),
            (100.0, 0.0),               // refused: baseline is the sentinel
            (100.0, -1.0),              // refused: negative baseline flips the sign
            (f64::NAN, 100.0),          // refused: non-finite value
            (100.0, f64::NAN),          // refused: non-finite baseline
            (100.0, f64::MIN_POSITIVE), // refused: quotient overflows
        ] {
            assert_eq!(
                pct_change(value, baseline),
                tickvault_common::price_precision::round_to_2dp(pct_change_raw(value, baseline)),
                "the wrapper drifted from the raw helper at ({value}, {baseline})"
            );
        }
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

    /// The two dropped percentage fields (`oi_pct_from_prev_day`,
    /// `volume_pct_from_prev_day`) are GONE, not merely unstamped.
    ///
    /// Their DDL columns were removed 2026-05-28 (spot has no OI, indices have
    /// no volume) and the fields then sat in every bar holding a permanent
    /// `0.0` — 16 bytes per state, multiplied by `TF_COUNT` slots and again by
    /// `last_sealed`, in a struct pinned at exactly 128 bytes by three separate
    /// compile-time assertions with zero slack between them.
    ///
    /// Reclaiming those 16 bytes is what pays for `bucket_open_prev_close`
    /// (8) + `total_buy_qty` (4) + `total_sell_qty` (4). This test is the
    /// replacement for the old "they are never stamped" pin: it asserts the
    /// struct did not grow, which is the property the assertions downstream
    /// actually depend on.
    #[test]
    fn reclaiming_the_dropped_percentages_kept_the_state_at_128_bytes() {
        assert_eq!(
            std::mem::size_of::<LiveCandleState>(),
            128,
            "LiveCandleState changed size — BufferedSeal (<=144), AggregatorCell \
             (MAX_AGGREGATOR_CELL_BYTES) and SerializedSeal (SEAL_SPILL_RECORD_SIZE) \
             all assume 128 and every one of them is at zero slack today."
        );
    }

    /// The four refusals, each one a real hazard rather than defensive noise.
    #[test]
    fn net_volume_refuses_every_question_it_cannot_answer() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.volume = 1_000;

        // No baseline: the first bar of a session has nothing to its left.
        s.bucket_open_prev_close = 0.0;
        assert_eq!(s.net_volume(), None);

        // Untraded bar: 0.0 is the absent-price sentinel, not a price.
        s.bucket_open_prev_close = 100.0;
        s.close = 0.0;
        assert_eq!(s.net_volume(), None);

        // Non-finite: NaN fails BOTH `>` and `<`, so an unguarded compare
        // would silently persist 0 — a real "flat" reading — for garbage.
        s.close = f64::NAN;
        assert_eq!(s.net_volume(), None);
        s.close = 100.0;
        s.bucket_open_prev_close = f64::INFINITY;
        assert_eq!(s.net_volume(), None);
    }

    #[test]
    fn net_volume_signs_by_direction_and_zero_means_flat() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.volume = 1_000;
        s.bucket_open_prev_close = 100.0;

        s.close = 101.0;
        assert_eq!(s.net_volume(), Some(1_000), "a rise is positive volume");
        s.close = 99.0;
        assert_eq!(s.net_volume(), Some(-1_000), "a fall is negative volume");
        s.close = 100.0;
        assert_eq!(
            s.net_volume(),
            Some(0),
            "an unchanged close is a real, reportable zero — distinct from None"
        );
    }

    /// `-(u64 as i64)` past `i64::MAX` wraps POSITIVE, which would persist a
    /// sell bar as a buy bar. The saturating conversion makes that
    /// unrepresentable.
    #[test]
    fn net_volume_saturates_instead_of_wrapping_a_sell_bar_into_a_buy_bar() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.bucket_open_prev_close = 100.0;
        s.close = 99.0; // a FALL — the sign must stay negative
        s.volume = u64::MAX;
        let nv = s.net_volume().expect("finite inputs");
        assert!(nv < 0, "a fall must never persist as a positive net volume");
        assert_eq!(nv, -i64::MAX);
    }

    /// The comparison must be exact, not a widened `f32`. `10.20_f32 as f64`
    /// is `10.19999980926514`; comparing that against a decimal-clean `10.2`
    /// reports an UNCHANGED price as a rise, on every flat bar, forever.
    #[test]
    fn an_unchanged_price_is_flat_and_not_a_fabricated_rise() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.volume = 500;
        let clean = tickvault_common::price_precision::f32_to_f64_clean(10.20_f32);
        s.bucket_open_prev_close = clean;
        s.close = clean;
        assert_eq!(
            s.net_volume(),
            Some(0),
            "identical decimal-clean prices must compare equal"
        );
    }
}
