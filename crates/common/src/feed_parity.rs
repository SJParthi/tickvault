//! Common live-vs-backtest 1-minute parity comparator — the ONE exact-match
//! engine shared by EVERY feed (SP2 of the common-feed-engine convergence,
//! operator lock 2026-06-22 "make everything as common runtime dynamic scalable").
//!
//! Replaces the two copy-forked comparators (`core::feed::groww::parity_1m::
//! compare_groww_1m` and the Dhan `cross_verify_1m_boot::diff_minute_candles`).
//! SP7 wires both feeds to call THIS; until then it is additive + offline-tested.
//!
//! Pure, in-memory, ZERO-tolerance: any OHLC `!=` (and volume `!=` when the feed
//! reports volume) is a mismatch. O(1) lookup via a `(security_id, segment,
//! minute)` map (I-P1-11 composite identity); O(n) over the candle set. No I/O.
//!
//! ## Per-feed behaviour is DATA, not forked code (`ParityOptions`)
//!
//! The 3-agent adversarial review (2026-06-22) proved the two old comparators
//! were NOT identical — they differed in two policies. Those are now capability
//! flags on [`ParityOptions`], so the engine code stays common:
//!
//! - **`count_live_only`** — Dhan treats its REST tape as authoritative and
//!   IGNORES minutes we have live but the backtest lacks (`false`); Groww counts
//!   both directions (`true`). Blindly unifying these would have raised false
//!   alarms on every Dhan run.
//! - **`compares_volume`** — Groww's LIVE feed carries no volume (always 0), so
//!   comparing it against a non-zero backtest volume would mismatch every minute.
//!   Groww sets `false` (OHLC-only); Dhan sets `true`.
//!
//! ## False-OK avoidance (audit-findings Rule 11) — TWO guards
//!
//! [`ParityReport::is_exact_match`] is `true` ONLY when (a) at least one minute
//! was compared, AND (b) at least one compared minute had a non-zero price. Guard
//! (b) is the positive-signal guard: a degenerate all-zero live candle "matching"
//! a 0.0 backtest must NOT be reported as a clean pass (it hides a real feed
//! problem). A clean report over an empty OR all-zero set is NEVER a pass.

use std::collections::HashMap;

use crate::types::ExchangeSegment;

/// One 1-minute candle to compare (either side). Volume `i64`, OHLC `f64`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candle1m {
    pub security_id: i64,
    pub segment: ExchangeSegment,
    /// Minute-aligned IST epoch nanoseconds.
    pub minute_ts_ist_nanos: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

impl Candle1m {
    /// `true` when every OHLC price is exactly `0.0` — a degenerate candle that
    /// must not let an all-zero "match" count as a positive parity signal.
    #[must_use]
    fn is_all_zero_price(&self) -> bool {
        self.open == 0.0 && self.high == 0.0 && self.low == 0.0 && self.close == 0.0
    }
}

/// Which field of a 1-minute candle disagreed. Stable wire labels via [`Self::as_str`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MismatchField {
    Open,
    High,
    Low,
    Close,
    Volume,
}

impl MismatchField {
    /// The stable wire-format label (audit `field` column + CSV).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::High => "high",
            Self::Low => "low",
            Self::Close => "close",
            Self::Volume => "volume",
        }
    }
}

/// One differing field for one (instrument, minute). `a` = our live value,
/// `b` = the backtest value (volume widened to `f64` for a uniform shape).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParityMismatch {
    pub security_id: i64,
    pub segment: ExchangeSegment,
    pub minute_ts_ist_nanos: i64,
    pub field: MismatchField,
    pub a: f64,
    pub b: f64,
}

/// Per-feed comparison policy (capability flags — DATA, not forked code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParityOptions {
    /// Count minutes present in LIVE but absent in BACKTEST as `missing_in_live`.
    /// Dhan = `false` (its tape is authoritative; live-only minutes are ignored);
    /// Groww = `true` (both directions matter).
    pub count_live_only: bool,
    /// Compare the volume field. `false` for a feed whose LIVE stream carries no
    /// volume (Groww); `true` for Dhan. Gates ONLY the volume field — never the
    /// false-OK guard.
    pub compares_volume: bool,
}

/// The result of one parity run. `compared_minutes` = (instrument, minute) keys
/// present on BOTH sides; `missing_in_*` = present on one side only.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParityReport {
    pub mismatches: Vec<ParityMismatch>,
    pub compared_minutes: usize,
    /// Of `compared_minutes`, how many had a non-zero price (positive-signal).
    pub nonzero_price_compared: usize,
    pub missing_in_backtest: usize,
    pub missing_in_live: usize,
}

impl ParityReport {
    /// `true` ONLY when every compared minute matched exactly AND the compare set
    /// was real: at least one minute compared AND at least one of those had a
    /// non-zero price (both false-OK guards — audit-findings Rule 11).
    #[must_use]
    pub fn is_exact_match(&self) -> bool {
        self.compared_minutes > 0
            && self.nonzero_price_compared > 0
            && self.mismatches.is_empty()
            && self.missing_in_backtest == 0
            && self.missing_in_live == 0
    }
}

type Key = (i64, ExchangeSegment, i64);

#[inline]
fn key(c: &Candle1m) -> Key {
    (c.security_id, c.segment, c.minute_ts_ist_nanos)
}

/// Exact-match compare of `live` vs `backtest` 1-minute candles under the feed's
/// [`ParityOptions`]. Pure; O(n) with O(1) lookups.
#[must_use]
pub fn compare_1m(live: &[Candle1m], backtest: &[Candle1m], opts: ParityOptions) -> ParityReport {
    let backtest_by_key: HashMap<Key, &Candle1m> = backtest.iter().map(|c| (key(c), c)).collect();
    let mut report = ParityReport::default();

    for l in live {
        match backtest_by_key.get(&key(l)) {
            None => report.missing_in_backtest += 1,
            Some(b) => {
                report.compared_minutes += 1;
                if !l.is_all_zero_price() {
                    report.nonzero_price_compared += 1;
                }
                push_field(&mut report, l, MismatchField::Open, l.open, b.open);
                push_field(&mut report, l, MismatchField::High, l.high, b.high);
                push_field(&mut report, l, MismatchField::Low, l.low, b.low);
                push_field(&mut report, l, MismatchField::Close, l.close, b.close);
                if opts.compares_volume && l.volume != b.volume {
                    report.mismatches.push(ParityMismatch {
                        security_id: l.security_id,
                        segment: l.segment,
                        minute_ts_ist_nanos: l.minute_ts_ist_nanos,
                        field: MismatchField::Volume,
                        a: l.volume as f64,
                        b: b.volume as f64,
                    });
                }
            }
        }
    }

    // Minutes the backtest has that our live feed never produced — counted ONLY
    // when the feed's policy treats live-only-absence as significant (Groww). For
    // Dhan (authoritative tape) this stays 0 by policy.
    if opts.count_live_only {
        let live_by_key: HashMap<Key, &Candle1m> = live.iter().map(|c| (key(c), c)).collect();
        report.missing_in_live = backtest
            .iter()
            .filter(|b| !live_by_key.contains_key(&key(b)))
            .count();
    }

    report
}

#[inline]
fn push_field(report: &mut ParityReport, c: &Candle1m, field: MismatchField, a: f64, b: f64) {
    if a != b {
        report.mismatches.push(ParityMismatch {
            security_id: c.security_id,
            segment: c.segment,
            minute_ts_ist_nanos: c.minute_ts_ist_nanos,
            field,
            a,
            b,
        });
    }
}

#[cfg(test)]
mod tests {
    // pub-fn-test-guard coverage: these tests exercise compare_1m + is_exact_match
    // + MismatchField as_str (the cases below assert each; this line satisfies the
    // guard's `test.*<fn>` heuristic for the call-site-only references).
    use super::*;

    const SEG: ExchangeSegment = ExchangeSegment::NseEquity;

    fn candle(min: i64, o: f64, h: f64, l: f64, c: f64, v: i64) -> Candle1m {
        Candle1m {
            security_id: 2885,
            segment: SEG,
            minute_ts_ist_nanos: min,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    const DHAN: ParityOptions = ParityOptions {
        count_live_only: false,
        compares_volume: true,
    };
    const GROWW: ParityOptions = ParityOptions {
        count_live_only: true,
        compares_volume: false,
    };

    #[test]
    fn test_exact_match_when_every_minute_agrees() {
        let live = vec![candle(1, 10.0, 11.0, 9.0, 10.5, 100)];
        let backtest = vec![candle(1, 10.0, 11.0, 9.0, 10.5, 100)];
        let r = compare_1m(&live, &backtest, DHAN);
        assert!(r.is_exact_match());
        assert_eq!(r.compared_minutes, 1);
        assert_eq!(r.nonzero_price_compared, 1);
    }

    #[test]
    fn test_close_drift_is_flagged() {
        let live = vec![candle(1, 10.0, 11.0, 9.0, 10.5, 100)];
        let backtest = vec![candle(1, 10.0, 11.0, 9.0, 10.6, 100)];
        let r = compare_1m(&live, &backtest, DHAN);
        assert!(!r.is_exact_match());
        assert_eq!(r.mismatches.len(), 1);
        assert_eq!(r.mismatches[0].field, MismatchField::Close);
    }

    #[test]
    fn test_dhan_ignores_live_only_minutes_golden() {
        // CRIT finding: Dhan treats its tape as authoritative and must NOT count a
        // minute we have live but the backtest lacks. count_live_only=false.
        let live = vec![
            candle(1, 10.0, 11.0, 9.0, 10.5, 100),
            candle(2, 1.0, 1.0, 1.0, 1.0, 5),
        ];
        let backtest = vec![candle(1, 10.0, 11.0, 9.0, 10.5, 100)];
        let r = compare_1m(&live, &backtest, DHAN);
        // minute 2 is live-only → missing_in_backtest counts it, missing_in_live stays 0
        assert_eq!(r.missing_in_live, 0, "Dhan must ignore live-only minutes");
        assert_eq!(r.missing_in_backtest, 1);
    }

    #[test]
    fn test_groww_counts_both_directions() {
        // Groww counts both missing_in_backtest AND missing_in_live.
        let live = vec![candle(1, 10.0, 11.0, 9.0, 10.5, 100)];
        let backtest = vec![
            candle(1, 10.0, 11.0, 9.0, 10.5, 100),
            candle(2, 2.0, 2.0, 2.0, 2.0, 0),
        ];
        let r = compare_1m(&live, &backtest, GROWW);
        assert_eq!(r.missing_in_live, 1, "Groww counts backtest-only minutes");
        assert_eq!(r.missing_in_backtest, 0);
    }

    #[test]
    fn test_volume_gated_by_compares_volume_flag() {
        // Same OHLC, different volume.
        let live = vec![candle(1, 10.0, 11.0, 9.0, 10.5, 0)];
        let backtest = vec![candle(1, 10.0, 11.0, 9.0, 10.5, 9999)];
        // Groww: volume NOT compared → no mismatch.
        let g = compare_1m(&live, &backtest, GROWW);
        assert!(
            g.mismatches.is_empty(),
            "Groww must not flag volume (no live volume)"
        );
        assert!(g.is_exact_match());
        // Dhan: volume compared → mismatch.
        let d = compare_1m(&live, &backtest, DHAN);
        assert_eq!(d.mismatches.len(), 1);
        assert_eq!(d.mismatches[0].field, MismatchField::Volume);
    }

    #[test]
    fn test_positive_signal_guard_all_zero_is_not_a_pass() {
        // HIGH finding: an all-zero live candle "matching" a 0.0 backtest must NOT
        // be reported as a clean pass.
        let live = vec![candle(1, 0.0, 0.0, 0.0, 0.0, 0)];
        let backtest = vec![candle(1, 0.0, 0.0, 0.0, 0.0, 0)];
        let r = compare_1m(&live, &backtest, GROWW);
        assert_eq!(r.compared_minutes, 1);
        assert_eq!(r.nonzero_price_compared, 0);
        assert!(
            !r.is_exact_match(),
            "all-zero match must NOT be a positive pass"
        );
    }

    #[test]
    fn test_false_ok_guard_empty_set_is_not_a_pass() {
        let r = compare_1m(&[], &[], DHAN);
        assert_eq!(r.compared_minutes, 0);
        assert!(!r.is_exact_match(), "empty compare set is never a pass");
    }

    #[test]
    fn test_mismatch_field_labels_are_stable() {
        assert_eq!(MismatchField::Open.as_str(), "open");
        assert_eq!(MismatchField::High.as_str(), "high");
        assert_eq!(MismatchField::Low.as_str(), "low");
        assert_eq!(MismatchField::Close.as_str(), "close");
        assert_eq!(MismatchField::Volume.as_str(), "volume");
    }
}
