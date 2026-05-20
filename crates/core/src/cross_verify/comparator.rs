//! Pure-logic zero-tolerance OHLCV comparison engine.
//!
//! The cross-verify schedulers (PR #9c/#9d) fetch our derived live
//! candle series from QuestDB and the authoritative series from Dhan
//! REST, then hand both to [`compare_series`]. This module has NO I/O,
//! NO async, NO QuestDB or Dhan dependency — it is a deterministic pure
//! function over two candle slices, and is exhaustively unit-tested.
//!
//! # Zero tolerance
//!
//! Per `.claude/rules/project/historical-candles-cross-verify.md`:
//! "NO tolerance. NO epsilon. NO ±10%. Every value must match exactly."
//! The comparator therefore uses exact `!=` equality. This relies on
//! [`tickvault_common::constants::CROSS_VERIFY_OHLCV_TOLERANCE`] being
//! `0.0` — pinned by `constants::tests::test_cross_verify_tolerance_is_exactly_zero`.
//! Exact `!=` also flags `NaN` candle data (a `NaN` field never equals
//! anything), which an `abs() > tolerance` form would silently miss.

/// One OHLCV candle, timeframe-agnostic.
///
/// `volume` is `f64` so all five fields compare uniformly under exact
/// equality. Index volume is integer-valued and well below `2^53`, so
/// it is represented exactly. Indices carry no meaningful volume from
/// Dhan historical (always `0`); the scheduler that builds these
/// candles is responsible for populating `volume` consistently on both
/// the live and historical side so it never spuriously mismatches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrossVerifyCandle {
    /// Candle-open timestamp, IST epoch microseconds. This is the join
    /// key: a live candle and a historical candle describe the same
    /// bar iff their `ts_ist_micros` are equal.
    pub ts_ist_micros: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// Which OHLCV field of a candle disagreed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandleField {
    Open,
    High,
    Low,
    Close,
    Volume,
}

impl CandleField {
    /// All five fields, in canonical OHLCV order. The comparator walks
    /// this array so adding a field is a one-line change here.
    pub const ALL: [CandleField; 5] = [
        CandleField::Open,
        CandleField::High,
        CandleField::Low,
        CandleField::Close,
        CandleField::Volume,
    ];

    /// Stable wire-format label — used as the audit-table `field`
    /// SYMBOL value and in the mismatch Telegram message.
    pub fn as_str(self) -> &'static str {
        match self {
            CandleField::Open => "open",
            CandleField::High => "high",
            CandleField::Low => "low",
            CandleField::Close => "close",
            CandleField::Volume => "volume",
        }
    }

    /// Reads this field's value out of a candle.
    fn value_of(self, candle: &CrossVerifyCandle) -> f64 {
        match self {
            CandleField::Open => candle.open,
            CandleField::High => candle.high,
            CandleField::Low => candle.low,
            CandleField::Close => candle.close,
            CandleField::Volume => candle.volume,
        }
    }
}

/// One field-level disagreement at one candle timestamp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandleMismatch {
    /// IST epoch microseconds of the candle that disagreed.
    pub ts_ist_micros: i64,
    /// Which OHLCV field disagreed.
    pub field: CandleField,
    /// The value in our derived live candle.
    pub live_value: f64,
    /// The authoritative value from Dhan REST.
    pub hist_value: f64,
}

/// Verdict of comparing one (SID, timeframe) live series vs Dhan REST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossVerifyOutcome {
    /// Every compared candle matched exactly.
    Passed,
    /// At least one field disagreed.
    Failed,
    /// Dhan REST could not be reached — verification was skipped, not
    /// a data-integrity failure. The scheduler maps this from a REST
    /// fetch error rather than from a [`ComparisonReport`].
    HistUnreachable,
}

impl CrossVerifyOutcome {
    /// Stable wire-format label — used as the audit-table `outcome`
    /// SYMBOL value.
    pub fn as_str(self) -> &'static str {
        match self {
            CrossVerifyOutcome::Passed => "passed",
            CrossVerifyOutcome::Failed => "failed",
            CrossVerifyOutcome::HistUnreachable => "hist_unreachable",
        }
    }
}

/// Result of comparing two aligned OHLCV series.
#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonReport {
    /// Count of candles present in BOTH series that were compared
    /// field-by-field.
    pub candles_compared: usize,
    /// Count of timestamps the historical (authoritative) series
    /// carried that our derived live series did NOT — i.e. bars we
    /// failed to capture. A non-zero value means missed ticks.
    pub missing_live: usize,
    /// Every field-level disagreement found across all compared
    /// candles. Empty iff every compared candle matched exactly.
    pub mismatches: Vec<CandleMismatch>,
}

impl ComparisonReport {
    /// Classifies the report into a [`CrossVerifyOutcome`].
    ///
    /// `Passed` iff there are zero field mismatches AND zero missing
    /// live candles. A missing live candle is itself an integrity
    /// failure — the bar exists in the authoritative feed but not in
    /// ours. `HistUnreachable` is never produced here (it models a
    /// REST transport failure, not a data comparison).
    pub fn outcome(&self) -> CrossVerifyOutcome {
        if self.mismatches.is_empty() && self.missing_live == 0 {
            CrossVerifyOutcome::Passed
        } else {
            CrossVerifyOutcome::Failed
        }
    }

    /// Number of field-level disagreements.
    pub fn mismatch_count(&self) -> usize {
        self.mismatches.len()
    }
}

/// Compares the five OHLCV fields of one candle pair, appending every
/// disagreement to `out`. Exact equality — see the module docstring.
fn compare_candle(
    live: &CrossVerifyCandle,
    hist: &CrossVerifyCandle,
    out: &mut Vec<CandleMismatch>,
) {
    for field in CandleField::ALL {
        let live_value = field.value_of(live);
        let hist_value = field.value_of(hist);
        // Exact `!=`: catches every difference including `NaN`, where
        // `NaN != NaN` is `true`.
        if live_value != hist_value {
            out.push(CandleMismatch {
                ts_ist_micros: live.ts_ist_micros,
                field,
                live_value,
                hist_value,
            });
        }
    }
}

/// Compares our derived live candle series against the authoritative
/// Dhan REST series, joining candles by `ts_ist_micros`.
///
/// The historical series is treated as ground truth: every historical
/// candle is expected to have a live counterpart. A historical
/// timestamp with no live candle increments `missing_live`. A live
/// candle with no historical counterpart is ignored — Dhan REST
/// defines the set of bars that "should" exist, and our aggregator
/// occasionally derives an extra boundary bar that the authoritative
/// feed does not (e.g. a partial pre-open bar), which is not an
/// integrity failure for the windows the scheduler queries.
///
/// This runs once per (SID, timeframe) per day — a cold-path daily
/// task, NOT the tick hot path — so the per-call allocations
/// (lookup map, mismatch vector) are acceptable.
pub fn compare_series(live: &[CrossVerifyCandle], hist: &[CrossVerifyCandle]) -> ComparisonReport {
    use std::collections::HashMap;

    let mut live_by_ts: HashMap<i64, &CrossVerifyCandle> = HashMap::with_capacity(live.len());
    for candle in live {
        live_by_ts.insert(candle.ts_ist_micros, candle);
    }

    let mut mismatches: Vec<CandleMismatch> = Vec::new();
    let mut candles_compared = 0usize;
    let mut missing_live = 0usize;

    for hist_candle in hist {
        match live_by_ts.get(&hist_candle.ts_ist_micros) {
            Some(live_candle) => {
                candles_compared += 1;
                compare_candle(live_candle, hist_candle, &mut mismatches);
            }
            None => {
                missing_live += 1;
            }
        }
    }

    ComparisonReport {
        candles_compared,
        missing_live,
        mismatches,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(ts: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> CrossVerifyCandle {
        CrossVerifyCandle {
            ts_ist_micros: ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    #[test]
    fn test_candle_field_as_str_stable() {
        assert_eq!(CandleField::Open.as_str(), "open");
        assert_eq!(CandleField::High.as_str(), "high");
        assert_eq!(CandleField::Low.as_str(), "low");
        assert_eq!(CandleField::Close.as_str(), "close");
        assert_eq!(CandleField::Volume.as_str(), "volume");
    }

    #[test]
    fn test_candle_field_all_has_five_unique() {
        assert_eq!(CandleField::ALL.len(), 5);
        let mut labels: Vec<&str> = CandleField::ALL.iter().map(|f| f.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 5, "every field label must be unique");
    }

    #[test]
    fn test_cross_verify_outcome_as_str_stable() {
        assert_eq!(CrossVerifyOutcome::Passed.as_str(), "passed");
        assert_eq!(CrossVerifyOutcome::Failed.as_str(), "failed");
        assert_eq!(
            CrossVerifyOutcome::HistUnreachable.as_str(),
            "hist_unreachable"
        );
    }

    #[test]
    fn test_compare_series_identical_passes() {
        let live = vec![
            candle(1, 100.0, 110.0, 95.0, 105.0, 0.0),
            candle(2, 105.0, 112.0, 104.0, 108.0, 0.0),
        ];
        let hist = live.clone();
        let report = compare_series(&live, &hist);
        assert_eq!(report.candles_compared, 2);
        assert_eq!(report.missing_live, 0);
        assert_eq!(report.mismatch_count(), 0);
        assert_eq!(report.outcome(), CrossVerifyOutcome::Passed);
    }

    #[test]
    fn test_compare_series_empty_both_passes() {
        let report = compare_series(&[], &[]);
        assert_eq!(report.candles_compared, 0);
        assert_eq!(report.missing_live, 0);
        assert_eq!(report.outcome(), CrossVerifyOutcome::Passed);
    }

    #[test]
    fn test_compare_series_single_field_mismatch() {
        let live = vec![candle(1, 100.0, 110.0, 95.0, 105.0, 0.0)];
        // close differs: 105.0 -> 105.5
        let hist = vec![candle(1, 100.0, 110.0, 95.0, 105.5, 0.0)];
        let report = compare_series(&live, &hist);
        assert_eq!(report.candles_compared, 1);
        assert_eq!(report.mismatch_count(), 1);
        let m = report.mismatches[0];
        assert_eq!(m.field, CandleField::Close);
        assert_eq!(m.ts_ist_micros, 1);
        assert_eq!(m.live_value, 105.0);
        assert_eq!(m.hist_value, 105.5);
        assert_eq!(report.outcome(), CrossVerifyOutcome::Failed);
    }

    #[test]
    fn test_compare_series_all_five_fields_mismatch() {
        let live = vec![candle(1, 1.0, 2.0, 3.0, 4.0, 5.0)];
        let hist = vec![candle(1, 9.0, 9.0, 9.0, 9.0, 9.0)];
        let report = compare_series(&live, &hist);
        assert_eq!(report.mismatch_count(), 5, "all five fields disagree");
        let fields: Vec<CandleField> = report.mismatches.iter().map(|m| m.field).collect();
        assert_eq!(fields, CandleField::ALL.to_vec());
        assert_eq!(report.outcome(), CrossVerifyOutcome::Failed);
    }

    #[test]
    fn test_compare_series_missing_live_candle() {
        // hist has ts 1 and 2; live only has ts 1.
        let live = vec![candle(1, 100.0, 110.0, 95.0, 105.0, 0.0)];
        let hist = vec![
            candle(1, 100.0, 110.0, 95.0, 105.0, 0.0),
            candle(2, 105.0, 112.0, 104.0, 108.0, 0.0),
        ];
        let report = compare_series(&live, &hist);
        assert_eq!(report.candles_compared, 1);
        assert_eq!(report.missing_live, 1, "ts 2 exists in hist, not live");
        assert_eq!(report.mismatch_count(), 0);
        // A missing live candle is itself an integrity failure.
        assert_eq!(report.outcome(), CrossVerifyOutcome::Failed);
    }

    #[test]
    fn test_compare_series_extra_live_candle_ignored() {
        // live has an extra ts 3 the authoritative feed never published.
        let live = vec![
            candle(1, 100.0, 110.0, 95.0, 105.0, 0.0),
            candle(3, 1.0, 1.0, 1.0, 1.0, 0.0),
        ];
        let hist = vec![candle(1, 100.0, 110.0, 95.0, 105.0, 0.0)];
        let report = compare_series(&live, &hist);
        assert_eq!(report.candles_compared, 1);
        assert_eq!(report.missing_live, 0);
        assert_eq!(report.outcome(), CrossVerifyOutcome::Passed);
    }

    #[test]
    fn test_compare_series_nan_is_flagged() {
        // A NaN open in our derived candle must be caught — exact `!=`
        // flags it, an `abs() > tolerance` form would not.
        let live = vec![candle(1, f64::NAN, 110.0, 95.0, 105.0, 0.0)];
        let hist = vec![candle(1, 100.0, 110.0, 95.0, 105.0, 0.0)];
        let report = compare_series(&live, &hist);
        assert_eq!(report.mismatch_count(), 1);
        assert_eq!(report.mismatches[0].field, CandleField::Open);
        assert!(report.mismatches[0].live_value.is_nan());
        assert_eq!(report.outcome(), CrossVerifyOutcome::Failed);
    }

    #[test]
    fn test_compare_series_volume_mismatch_detected() {
        let live = vec![candle(1, 100.0, 110.0, 95.0, 105.0, 5_000_000.0)];
        let hist = vec![candle(1, 100.0, 110.0, 95.0, 105.0, 4_999_800.0)];
        let report = compare_series(&live, &hist);
        assert_eq!(report.mismatch_count(), 1);
        assert_eq!(report.mismatches[0].field, CandleField::Volume);
        assert_eq!(report.mismatches[0].live_value, 5_000_000.0);
        assert_eq!(report.mismatches[0].hist_value, 4_999_800.0);
    }

    #[test]
    fn test_compare_series_join_is_by_timestamp_not_index() {
        // Same candles, different ordering — join must be by ts.
        let live = vec![
            candle(2, 105.0, 112.0, 104.0, 108.0, 0.0),
            candle(1, 100.0, 110.0, 95.0, 105.0, 0.0),
        ];
        let hist = vec![
            candle(1, 100.0, 110.0, 95.0, 105.0, 0.0),
            candle(2, 105.0, 112.0, 104.0, 108.0, 0.0),
        ];
        let report = compare_series(&live, &hist);
        assert_eq!(report.candles_compared, 2);
        assert_eq!(report.mismatch_count(), 0);
        assert_eq!(report.outcome(), CrossVerifyOutcome::Passed);
    }

    #[test]
    fn test_comparison_report_mismatch_count() {
        let report = ComparisonReport {
            candles_compared: 4,
            missing_live: 0,
            mismatches: vec![
                CandleMismatch {
                    ts_ist_micros: 1,
                    field: CandleField::Open,
                    live_value: 1.0,
                    hist_value: 2.0,
                },
                CandleMismatch {
                    ts_ist_micros: 2,
                    field: CandleField::Close,
                    live_value: 3.0,
                    hist_value: 4.0,
                },
            ],
        };
        assert_eq!(report.mismatch_count(), 2);
        assert_eq!(
            report.mismatch_count(),
            report.mismatches.len(),
            "mismatch_count must mirror the mismatches vector length"
        );
    }

    #[test]
    fn test_comparison_report_outcome_passed_requires_no_missing() {
        let clean = ComparisonReport {
            candles_compared: 10,
            missing_live: 0,
            mismatches: Vec::new(),
        };
        assert_eq!(clean.outcome(), CrossVerifyOutcome::Passed);

        let with_missing = ComparisonReport {
            candles_compared: 10,
            missing_live: 1,
            mismatches: Vec::new(),
        };
        assert_eq!(with_missing.outcome(), CrossVerifyOutcome::Failed);
    }
}
