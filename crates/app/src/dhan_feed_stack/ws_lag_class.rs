//! The worst delivery delay split by segment class (2026-10-29, live audit
//! LH-05).
//!
//! `tv_dhan_ws_lag_max_ms` (one series, peak-held over 60 s) has read 61 to
//! 152 s while our own ring dwell read 3 to 30 ms, which keeps
//! `tv-prod-dhan-feed-delay-high` in ALARM. The lag is receipt time minus the
//! vendor's LAST TRADE time, so the Assumed cause is a sparse stock option
//! whose new trade is stamped with an old trade second, not the feed running
//! late. This module splits the same maximum by segment class so the two can
//! be told apart:
//!
//! `tv_dhan_ws_lag_max_ms_by_class{class="index_spot"|"fno"}`
//!
//! * `index_spot`: `IDX_I`, `NSE_EQ`, `BSE_EQ` (indices and cash stocks).
//! * `fno`: every other segment (option and future contracts; currency and
//!   commodity contracts too, none of which the lane subscribes today).
//!
//! Same samples, same 60 s peak hold, published beside the existing gauge.
//! The existing gauge, its alarm, the EMF selector (anchored `^(...)$`, so the
//! new name does not match it), the metric filters and every Telegram page are
//! unchanged: local `/metrics` only (noise lock).
//!
//! # Cost
//!
//! Per new trade on a live socket: one segment-code compare and one relaxed
//! `fetch_max` on a fixed two-slot array, O(1), no allocation, no lock. The
//! publish runs on the drain's existing report arm (~500 ms), O(classes) = 2
//! gauge writes through handles resolved once.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use tickvault_common::types::ExchangeSegment;

use super::PeakHoldCell;

/// Gauge: [`super::WS_LAG_MAX_MS_GAUGE`] split by segment class, label
/// `class`. Local `/metrics` only.
pub(super) const WS_LAG_MAX_BY_CLASS_GAUGE: &str = "tv_dhan_ws_lag_max_ms_by_class";

/// Number of classes; the array index of each.
const CLASS_COUNT: usize = 2;

/// A segment class. The discriminant is the array index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LagClass {
    /// Indices and cash stocks.
    IndexSpot = 0,
    /// Contracts (options and futures).
    Fno = 1,
}

/// Every class, in index order.
const ALL_CLASSES: [LagClass; CLASS_COUNT] = [LagClass::IndexSpot, LagClass::Fno];

impl LagClass {
    /// The `class` label value.
    const fn label(self) -> &'static str {
        match self {
            Self::IndexSpot => "index_spot",
            Self::Fno => "fno",
        }
    }

    /// The class of the packet header's segment byte. O(1): one table
    /// match (`ExchangeSegment::from_byte`) and one pattern test. An unknown
    /// byte reads as a contract.
    pub(super) fn of_segment_code(code: u8) -> Self {
        match ExchangeSegment::from_byte(code) {
            Some(
                ExchangeSegment::IdxI | ExchangeSegment::NseEquity | ExchangeSegment::BseEquity,
            ) => Self::IndexSpot,
            _ => Self::Fno,
        }
    }
}

/// The per-class window maximum (whole milliseconds) and its peak hold.
pub(super) struct LagMaxByClass {
    max_millis: [AtomicI64; CLASS_COUNT],
    peak: [PeakHoldCell; CLASS_COUNT],
}

impl LagMaxByClass {
    pub(super) const fn new() -> Self {
        Self {
            max_millis: [const { AtomicI64::new(0) }; CLASS_COUNT],
            peak: [const { PeakHoldCell::new() }; CLASS_COUNT],
        }
    }

    /// Folds one positive lag into its class's window maximum. O(1): one
    /// compare chain on the segment byte and one relaxed `fetch_max`.
    #[inline]
    pub(super) fn note(&self, segment_code: u8, lag_ms: i64) {
        let class = LagClass::of_segment_code(segment_code);
        self.max_millis[class as usize].fetch_max(lag_ms, Ordering::Relaxed);
    }

    /// Takes one class's window maximum (reset on read), folds it into that
    /// class's 60 s peak hold at `now_ms`, and returns the value to publish.
    pub(super) fn take_published(&self, class: LagClass, now_ms: i64) -> f64 {
        let i = class as usize;
        let millis = self.max_millis[i].swap(0, Ordering::Relaxed);
        let sample = f64::from(u32::try_from(millis).unwrap_or(u32::MAX));
        self.peak[i].publish(sample, now_ms)
    }
}

/// The live drain's per-class maximum.
pub(super) static WS_LAG_MAX_BY_CLASS: LagMaxByClass = LagMaxByClass::new();

/// One gauge handle per class, resolved on the first publish (after the
/// recorder is installed) and set to 0 then, so the series exist from the
/// first report as "no lag seen yet".
fn class_gauges() -> &'static [metrics::Gauge; CLASS_COUNT] {
    static GAUGES: OnceLock<[metrics::Gauge; CLASS_COUNT]> = OnceLock::new();
    GAUGES.get_or_init(|| {
        ALL_CLASSES.map(|class| {
            let g = metrics::gauge!(WS_LAG_MAX_BY_CLASS_GAUGE, "class" => class.label());
            g.set(0.0);
            g
        })
    })
}

/// Publishes every class's peak-held maximum. Called beside the existing
/// lag gauge on the drain's report arm. O(classes).
pub(super) fn publish_ws_lag_max_by_class(now_ms: i64) {
    for (class, gauge) in ALL_CLASSES.iter().zip(class_gauges().iter()) {
        gauge.set(WS_LAG_MAX_BY_CLASS.take_published(*class, now_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 152 s, the worst lag the live gauge showed.
    const OLD_TRADE_LAG_MS: i64 = 152_000;

    #[test]
    fn test_class_codes_match_the_shared_enum() {
        for code in 0..=u8::MAX {
            let expected = matches!(
                ExchangeSegment::from_byte(code),
                Some(
                    ExchangeSegment::IdxI | ExchangeSegment::NseEquity | ExchangeSegment::BseEquity
                )
            );
            assert_eq!(
                LagClass::of_segment_code(code) == LagClass::IndexSpot,
                expected,
                "segment code {code}"
            );
        }
        assert_eq!(LagClass::IndexSpot.label(), "index_spot");
        assert_eq!(LagClass::Fno.label(), "fno");
    }

    #[test]
    fn test_an_fno_tick_with_an_old_trade_time_raises_only_the_fno_class() {
        let by_class = LagMaxByClass::new();
        by_class.note(ExchangeSegment::NseFno.binary_code(), OLD_TRADE_LAG_MS);
        assert!((by_class.take_published(LagClass::Fno, 0) - 152_000.0).abs() < f64::EPSILON);
        assert!(by_class.take_published(LagClass::IndexSpot, 0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_index_and_cash_ticks_raise_only_the_index_spot_class() {
        let by_class = LagMaxByClass::new();
        by_class.note(ExchangeSegment::IdxI.binary_code(), 1_200);
        by_class.note(ExchangeSegment::NseEquity.binary_code(), 2_500);
        by_class.note(ExchangeSegment::BseEquity.binary_code(), 900);
        assert!((by_class.take_published(LagClass::IndexSpot, 0) - 2_500.0).abs() < f64::EPSILON);
        assert!(by_class.take_published(LagClass::Fno, 0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_each_class_is_peak_held_for_the_window_and_reset_on_read() {
        let by_class = LagMaxByClass::new();
        by_class.note(ExchangeSegment::NseFno.binary_code(), OLD_TRADE_LAG_MS);
        by_class.note(ExchangeSegment::IdxI.binary_code(), 1_000);
        // First window: the in-progress maximum is published.
        assert!((by_class.take_published(LagClass::Fno, 0) - 152_000.0).abs() < f64::EPSILON);
        assert!((by_class.take_published(LagClass::IndexSpot, 0) - 1_000.0).abs() < f64::EPSILON);
        // Reset on read: nothing new in the next sample, the window still
        // holds its maximum until it closes.
        assert!((by_class.take_published(LagClass::Fno, 500) - 152_000.0).abs() < f64::EPSILON);
        // The window closes at 60 s with its maximum; the next one is quiet.
        assert!((by_class.take_published(LagClass::Fno, 60_000) - 152_000.0).abs() < f64::EPSILON);
        assert!(by_class.take_published(LagClass::Fno, 120_000).abs() < f64::EPSILON);
    }
}
