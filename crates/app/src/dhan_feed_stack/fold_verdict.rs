//! What the frame drain does with one tick after the candle fold has read it
//! (plan item LF-8): refuse it outright, keep only its row, or count it as
//! folded. Pure, so the live drain and the offline replay
//! (`live_fill::replay`) reach the same verdict from the same
//! [`ConsumeStats`] and cannot drift apart.
//!
//! The reasoning for every condition (why a stale trading day is a hard
//! refusal, why slot exhaustion keeps the row, why a prior-session WAL replay
//! keeps the row) is written at the call site in
//! `LiveIngest::ingest_tick_at`, beside the counters each arm moves. Moved
//! here unchanged on 2026-11-12; `proptest_fold_verdict_equals_inline_expressions`
//! pins the moved expressions to the old inline ones.
//!
//! # Complexity
//!
//! [`fold_verdict`]: O(1), a fixed set of boolean reads, no allocation, no
//! branch on anything but the stats. It runs once per tick on the drain.

use tickvault_trading::candles::ConsumeStats;

/// The drain's decision for one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldVerdict {
    /// Nothing is written and nothing folds. `wrong_day`: the 2026-09-10 day
    /// rule refused it and nothing else did, so the packet is kept in
    /// `feed_aux_packets` as `no_trade_today` (exact repeats skipped, Quote 61).
    HardRefusal { wrong_day: bool },
    /// The row is written; no candle bucket opened.
    CandleOnly,
    /// Fully accepted. The two flags are what the live fill tap hands on
    /// (see [`tap_trade_flags`]).
    Folded { new_trade: bool, older_trade: bool },
}

/// The live fill tap's two trade flags from the fold's stats: a NEW trade (a
/// new trade, or a first packet that traded while this process listened),
/// and a real OLDER trade. O(1).
#[must_use]
#[inline]
pub const fn tap_trade_flags(advanced: bool, fresh_first: bool, older_trade: bool) -> (bool, bool) {
    (advanced || fresh_first, older_trade)
}

/// The verdict for one tick (see the module docs). `received_at_nanos` is
/// the tick's receipt (UTC epoch nanos; 0 when unknown). O(1).
#[must_use]
#[inline]
pub fn fold_verdict(stats: &ConsumeStats, received_at_nanos: i64) -> FoldVerdict {
    // A WAL-replayed tick captured on the day it traded, refused only by the
    // replay's seeded watermark: real captured data, so the row is kept.
    let prior_session_replay = stats.stale_trading_day
        && !stats.receipt_day_mismatch
        && received_at_nanos > 0
        && !stats.refused_price
        && !stats.refused_timestamp;
    let hard_refusal = stats.refused_price
        || stats.refused_timestamp
        || (stats.stale_trading_day && !prior_session_replay)
        || stats.future_trading_day;
    let candle_only_refusal = (stats.out_of_session
        || stats.untraded_sentinel
        || stats.untraded_timestamp
        || stats.out_of_band_timestamp
        || stats.slot_exhausted
        || prior_session_replay)
        && !hard_refusal;
    if hard_refusal {
        // `receipt_day_mismatch` only ever rides a day refusal, and price and
        // timestamp are judged first, so this is exactly "the day rule
        // refused it, and nothing else did".
        return FoldVerdict::HardRefusal {
            wrong_day: stats.receipt_day_mismatch
                && !stats.refused_price
                && !stats.refused_timestamp,
        };
    }
    if candle_only_refusal {
        return FoldVerdict::CandleOnly;
    }
    let (new_trade, older_trade) =
        tap_trade_flags(stats.advanced, stats.fresh_first, stats.older_trade);
    FoldVerdict::Folded {
        new_trade,
        older_trade,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The expressions exactly as `ingest_tick_at` held them inline before
    /// 2026-11-12, kept as the oracle.
    fn old_inline(stats: &ConsumeStats, received_at_nanos: i64) -> FoldVerdict {
        let prior_session_replay = stats.stale_trading_day
            && !stats.receipt_day_mismatch
            && received_at_nanos > 0
            && !stats.refused_price
            && !stats.refused_timestamp;
        let hard_refusal = stats.refused_price
            || stats.refused_timestamp
            || (stats.stale_trading_day && !prior_session_replay)
            || stats.future_trading_day;
        let candle_only_refusal = (stats.out_of_session
            || stats.untraded_sentinel
            || stats.untraded_timestamp
            || stats.out_of_band_timestamp
            || stats.slot_exhausted
            || prior_session_replay)
            && !hard_refusal;
        if hard_refusal {
            return FoldVerdict::HardRefusal {
                wrong_day: stats.receipt_day_mismatch
                    && !stats.refused_price
                    && !stats.refused_timestamp,
            };
        }
        if candle_only_refusal {
            return FoldVerdict::CandleOnly;
        }
        FoldVerdict::Folded {
            new_trade: stats.advanced || stats.fresh_first,
            older_trade: stats.older_trade,
        }
    }

    fn stats_of(bits: u32) -> ConsumeStats {
        let b = |i: u32| bits & (1 << i) != 0;
        ConsumeStats {
            refused_price: b(0),
            refused_timestamp: b(1),
            stale_trading_day: b(2),
            future_trading_day: b(3),
            receipt_day_mismatch: b(4),
            out_of_session: b(5),
            untraded_sentinel: b(6),
            untraded_timestamp: b(7),
            out_of_band_timestamp: b(8),
            slot_exhausted: b(9),
            advanced: b(10),
            fresh_first: b(11),
            older_trade: b(12),
            repeat_quote: b(13),
            ..ConsumeStats::default()
        }
    }

    /// Every combination of the fourteen flags, at three receipts, against
    /// the oracle: the extraction did not change the live verdict.
    #[test]
    fn test_fold_verdict_matches_ingest_arms() {
        for bits in 0..(1u32 << 14) {
            for receipt in [0_i64, 1, 1_790_000_000_000_000_000] {
                let s = stats_of(bits);
                assert_eq!(
                    fold_verdict(&s, receipt),
                    old_inline(&s, receipt),
                    "bits {bits:#b} receipt {receipt}"
                );
            }
        }
        // The arms the drain relies on, by name.
        let clean = ConsumeStats {
            advanced: true,
            ..ConsumeStats::default()
        };
        assert_eq!(
            fold_verdict(&clean, 1),
            FoldVerdict::Folded {
                new_trade: true,
                older_trade: false
            }
        );
        let snapshot = ConsumeStats {
            stale_trading_day: true,
            receipt_day_mismatch: true,
            ..ConsumeStats::default()
        };
        assert_eq!(
            fold_verdict(&snapshot, 1),
            FoldVerdict::HardRefusal { wrong_day: true }
        );
        let replayed = ConsumeStats {
            stale_trading_day: true,
            ..ConsumeStats::default()
        };
        assert_eq!(fold_verdict(&replayed, 1), FoldVerdict::CandleOnly);
        assert_eq!(
            fold_verdict(&replayed, 0),
            FoldVerdict::HardRefusal { wrong_day: false },
            "no receipt: a stale day stays hard"
        );
        let slot = ConsumeStats {
            slot_exhausted: true,
            ..ConsumeStats::default()
        };
        assert_eq!(fold_verdict(&slot, 1), FoldVerdict::CandleOnly);
    }

    #[test]
    fn test_tap_trade_flags_new_trade_is_advanced_or_fresh_first() {
        assert_eq!(tap_trade_flags(false, false, false), (false, false));
        assert_eq!(tap_trade_flags(true, false, false), (true, false));
        assert_eq!(tap_trade_flags(false, true, true), (true, true));
        assert_eq!(tap_trade_flags(false, false, true), (false, true));
    }

    proptest! {
        #[test]
        fn proptest_fold_verdict_equals_inline_expressions(
            bits in 0u32..(1 << 14),
            receipt in any::<i64>(),
        ) {
            let s = stats_of(bits);
            prop_assert_eq!(fold_verdict(&s, receipt), old_inline(&s, receipt));
        }
    }
}
