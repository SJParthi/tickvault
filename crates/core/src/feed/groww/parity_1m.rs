//! Groww live-vs-backtest 1-minute parity check — the deliverable that answers
//! *"is Groww live == Groww backtest?"* (operator lock §32 / §0 Quote 1).
//!
//! Pure, in-memory, **exact-match** comparison of our live candles (the shared
//! `candles_1m` table, `feed='groww'`)
//! against Groww's own backtest 1-minute candles, minute-by-minute,
//! field-by-field — ZERO tolerance (mirrors the Dhan `cross_verify_1m` zero-EPS
//! rule). Self-contained (no I/O / network / DB) so it is fully offline-unit-
//! tested; the boot orchestrator (later) fetches both sides and persists the
//! [`GrowwParityMismatch`] rows + a Telegram count.
//!
//! O(1) per-candle lookup via a `(security_id, ExchangeSegment, minute)` map
//! (I-P1-11 composite identity); O(n) over the candle set.

use std::collections::HashMap;

use tickvault_common::types::ExchangeSegment;

/// One 1-minute candle to compare (either side). Volume is `i64`; OHLC `f64`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowwParityCandle {
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

/// One differing field for one (instrument, minute). `live`/`backtest` are the
/// two values that disagreed (volume widened to `f64` for a uniform shape).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowwParityMismatch {
    pub security_id: i64,
    pub segment: ExchangeSegment,
    pub minute_ts_ist_nanos: i64,
    /// `"open"` / `"high"` / `"low"` / `"close"` / `"volume"`.
    pub field: &'static str,
    pub live: f64,
    pub backtest: f64,
}

/// The result of one parity run. `compared_minutes` = (instrument, minute) keys
/// present on BOTH sides; `missing_in_*` = present on one side only.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GrowwParityReport {
    pub mismatches: Vec<GrowwParityMismatch>,
    pub compared_minutes: usize,
    pub missing_in_backtest: usize,
    pub missing_in_live: usize,
}

impl GrowwParityReport {
    /// `true` when every compared minute matched exactly AND at least one minute
    /// was compared (false-OK avoidance, audit-findings Rule 11 — a clean report
    /// over an empty set is NOT a pass).
    #[must_use]
    pub fn is_exact_match(&self) -> bool {
        self.compared_minutes > 0
            && self.mismatches.is_empty()
            && self.missing_in_backtest == 0
            && self.missing_in_live == 0
    }
}

type Key = (i64, ExchangeSegment, i64);

#[inline]
fn key(c: &GrowwParityCandle) -> Key {
    (c.security_id, c.segment, c.minute_ts_ist_nanos)
}

/// Exact-match compare of `live` vs `backtest` 1-minute candles. ZERO tolerance:
/// any OHLC `!=` or volume `!=` is a mismatch. Pure; O(n) with O(1) lookups.
#[must_use]
pub fn compare_groww_1m(
    live: &[GrowwParityCandle],
    backtest: &[GrowwParityCandle],
) -> GrowwParityReport {
    let backtest_by_key: HashMap<Key, &GrowwParityCandle> =
        backtest.iter().map(|c| (key(c), c)).collect();
    let live_by_key: HashMap<Key, &GrowwParityCandle> = live.iter().map(|c| (key(c), c)).collect();

    let mut report = GrowwParityReport::default();

    for l in live {
        match backtest_by_key.get(&key(l)) {
            None => report.missing_in_backtest += 1,
            Some(b) => {
                report.compared_minutes += 1;
                push_field(&mut report, l, "open", l.open, b.open);
                push_field(&mut report, l, "high", l.high, b.high);
                push_field(&mut report, l, "low", l.low, b.low);
                push_field(&mut report, l, "close", l.close, b.close);
                if l.volume != b.volume {
                    report.mismatches.push(GrowwParityMismatch {
                        security_id: l.security_id,
                        segment: l.segment,
                        minute_ts_ist_nanos: l.minute_ts_ist_nanos,
                        field: "volume",
                        live: l.volume as f64,
                        backtest: b.volume as f64,
                    });
                }
            }
        }
    }
    // Minutes Groww's backtest has that our live feed never produced.
    report.missing_in_live = backtest
        .iter()
        .filter(|b| !live_by_key.contains_key(&key(b)))
        .count();

    report
}

#[inline]
fn push_field(
    report: &mut GrowwParityReport,
    c: &GrowwParityCandle,
    field: &'static str,
    live: f64,
    backtest: f64,
) {
    if live != backtest {
        report.mismatches.push(GrowwParityMismatch {
            security_id: c.security_id,
            segment: c.segment,
            minute_ts_ist_nanos: c.minute_ts_ist_nanos,
            field,
            live,
            backtest,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEG: ExchangeSegment = ExchangeSegment::NseEquity;
    const M: i64 = 1_780_000_020_000_000_000;

    fn candle(min: i64, o: f64, h: f64, l: f64, c: f64, v: i64) -> GrowwParityCandle {
        GrowwParityCandle {
            security_id: 1333,
            segment: SEG,
            minute_ts_ist_nanos: min,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    #[test]
    fn test_compare_groww_1m_exact_match_no_mismatch() {
        let live = vec![candle(M, 100.0, 110.0, 99.0, 105.0, 50)];
        let backtest = live.clone();
        let r = compare_groww_1m(&live, &backtest);
        assert!(r.is_exact_match());
        assert_eq!(r.compared_minutes, 1);
        assert!(r.mismatches.is_empty());
    }

    #[test]
    fn test_each_differing_field_is_reported() {
        let live = vec![candle(M, 100.0, 110.0, 99.0, 105.0, 50)];
        let backtest = vec![candle(M, 100.5, 110.0, 98.0, 105.0, 51)]; // open, low, volume differ
        let r = compare_groww_1m(&live, &backtest);
        assert!(!r.is_exact_match());
        assert_eq!(r.compared_minutes, 1);
        let fields: Vec<&str> = r.mismatches.iter().map(|m| m.field).collect();
        assert!(fields.contains(&"open"));
        assert!(fields.contains(&"low"));
        assert!(fields.contains(&"volume"));
        assert!(!fields.contains(&"high"));
        assert!(!fields.contains(&"close"));
        assert_eq!(r.mismatches.len(), 3);
    }

    #[test]
    fn test_missing_minutes_counted_both_directions() {
        let live = vec![
            candle(M, 1.0, 1.0, 1.0, 1.0, 1),
            candle(M + 60_000_000_000, 2.0, 2.0, 2.0, 2.0, 2),
        ];
        let backtest = vec![
            candle(M, 1.0, 1.0, 1.0, 1.0, 1),
            candle(M + 120_000_000_000, 3.0, 3.0, 3.0, 3.0, 3),
        ];
        let r = compare_groww_1m(&live, &backtest);
        assert_eq!(r.compared_minutes, 1); // only minute M on both
        assert_eq!(r.missing_in_backtest, 1); // live has M+60s
        assert_eq!(r.missing_in_live, 1); // backtest has M+120s
        assert!(!r.is_exact_match());
    }

    #[test]
    fn test_is_exact_match_false_on_empty_sets() {
        // False-OK avoidance: nothing compared is NOT a pass.
        let r = compare_groww_1m(&[], &[]);
        assert_eq!(r.compared_minutes, 0);
        assert!(!r.is_exact_match());
    }

    #[test]
    fn test_same_id_different_segment_isolated() {
        let live = vec![GrowwParityCandle {
            security_id: 1,
            segment: ExchangeSegment::NseEquity,
            minute_ts_ist_nanos: M,
            open: 10.0,
            high: 10.0,
            low: 10.0,
            close: 10.0,
            volume: 1,
        }];
        // Same numeric id + minute, DIFFERENT segment → not the same key (I-P1-11).
        let backtest = vec![GrowwParityCandle {
            security_id: 1,
            segment: ExchangeSegment::NseFno,
            minute_ts_ist_nanos: M,
            open: 999.0,
            high: 999.0,
            low: 999.0,
            close: 999.0,
            volume: 999,
        }];
        let r = compare_groww_1m(&live, &backtest);
        assert_eq!(
            r.compared_minutes, 0,
            "different segment = different instrument"
        );
        assert_eq!(r.missing_in_backtest, 1);
        assert_eq!(r.missing_in_live, 1);
    }
}
