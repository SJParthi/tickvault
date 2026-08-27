//! Random-input attack on the per-minute candle fold.
//!
//! This is the other half of the operator's stated requirement — "each and
//! every one minute of high low tracking capturing" — and it is the surface a
//! 2026-08-25 permutation sweep already found a real defect in: the OPEN
//! bucket had no out-of-order guard on `close` while the SEALED path did, so
//! a reordered packet inside a still-open bucket overwrote `close` with an
//! EARLIER price. On the daily bar that window is the whole session, so any
//! reordered packet could rewrite the day's close.
//!
//! That defect was found by permuting inputs, which is exactly what a
//! property test does systematically rather than by hand. The central claim
//! here is therefore PERMUTATION INVARIANCE: within one bucket, the recorded
//! high and low must not depend on the order packets happened to arrive in.
//! Ticks reach us over a network that makes no ordering promise, and Dhan
//! publishes no sequence number, so arrival order is genuinely arbitrary.
//!
//! `close` is deliberately NOT permutation-invariant — it is defined by the
//! exchange timestamp, with last-write-wins inside one second — so it is
//! asserted against that definition instead.

use proptest::prelude::*;

use tickvault_common::feed::Feed;
use tickvault_common::price_precision::f32_to_f64_clean;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::{
    AggregatorCell, FeedStrategy, tick_price_is_sane,
};
use tickvault_trading::candles::live_candle_state::LiveCandleState;
use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;
use tickvault_trading::candles::tf_index::TfIndex;

/// One 1-minute bucket, so every generated tick folds into the same bar.
/// The aggregator gates on `exchange_timestamp % 86_400`, so this must be an
/// EPOCH second whose IST seconds-of-day lands in session, NOT a bare
/// seconds-of-day value. Using 33_300 directly put every tick in January 1970,
/// which the session gate refused — so the whole file reported "no bucket"
/// rather than testing the fold. The offsets keep the run inside one minute.
const BUCKET_BASE: u32 = 1_756_000_000 - (1_756_000_000 % 86_400) + 33_300;

fn tick(price: f32, ts_offset: u32, volume: u32, oi: u32, day_open: f32) -> ParsedTick {
    ParsedTick {
        security_id: 13,
        exchange_segment_code: 0,
        last_traded_price: price,
        exchange_timestamp: BUCKET_BASE + ts_offset,
        volume,
        open_interest: oi,
        day_open,
        ..ParsedTick::default()
    }
}

/// Prices including the corrupt shapes the parsers are proven to emit, plus
/// the subnormal that is finite, positive and in range yet still widens to
/// zero — the case `tick_price_is_sane` exists for.
fn price() -> impl Strategy<Value = f32> {
    prop_oneof![
        70 => (0.05f32..90_000.0),
        4 => Just(0.0f32),
        3 => Just(f32::NAN),
        3 => Just(f32::INFINITY),
        3 => Just(-1.0f32),
        3 => Just(f32::MIN_POSITIVE),
        3 => Just(f32::MAX),
    ]
}

/// Open interest, INCLUDING the absent sentinel at a real rate.
///
/// **Added 2026-08-26 after this suite caught a defect essentially by luck.**
/// The OI properties are entirely about `0` — the ABSENT sentinel a
/// Ticker-mode packet carries — and the generator drew OI from `0..5_000`,
/// so the sentinel appeared about once in five thousand ticks. CI hit it;
/// 60,000 local cases did not. A property whose subject the generator almost
/// never produces is a coin flip, not a test.
///
/// The weighting mirrors `price()` above, which already names the shapes that
/// matter instead of trusting a uniform range to find them.
fn open_interest() -> impl Strategy<Value = u32> {
    prop_oneof![
        // The absent sentinel: EVERY Ticker-mode packet carries it, and every
        // equity carries it always. Common in the feed, so common here.
        3 => Just(0u32),
        7 => (1u32..5_000),
    ]
}

/// A scenario's ticks, sharing ONE day-open.
///
/// `day_open` is a per-DAY constant for an instrument, not a per-tick value,
/// and generating it per tick was a harness error with real consequences: the
/// aggregator arms the day's FIRST bucket from `tick.day_open` (the operator's
/// 09:15 equilibrium open, `armed_for_day_open`), so a per-tick day-open makes
/// the bar depend on WHICH tick arrived first — and the permutation claim
/// below stops being a statement about the fold at all.
///
/// `0.0` is generated too: it is the documented absent sentinel, and it is
/// what every Ticker-mode instrument carries.
fn ticks() -> impl Strategy<Value = Vec<ParsedTick>> {
    (
        prop_oneof![3 => (0.05f32..90_000.0), 1 => Just(0.0f32)],
        prop::collection::vec((price(), 0u32..59, 0u32..100_000, open_interest()), 1..24),
    )
        .prop_map(|(day_open, raw)| {
            raw.into_iter()
                .map(|(p, off, v, oi)| tick(p, off, v, oi, day_open))
                .collect()
        })
}

/// Folds a whole list through the REAL caller and returns the 1-minute slot.
///
/// Deliberately `MultiTfAggregator` and not `AggregatorCell` directly. The
/// cell's own doc says it plainly — *"The caller is responsible for
/// `tick_price_is_sane`; this function assumes a sane price so the check is
/// paid once per tick, not 21 times"* — so a harness that calls the cell
/// bypasses the gate and then reports the fold as accepting NaN. The first
/// version of this file did exactly that and produced six simultaneous
/// failures, which is the shape of a wrong harness rather than six defects.
///
/// Driving the aggregator tests the gate and the fold as the composition that
/// actually runs, which is also the only arrangement in which "an insane
/// price never reaches the bar" is a real claim.
fn fold_all(ts: &[ParsedTick]) -> Option<LiveCandleState> {
    let mut agg = MultiTfAggregator::new(FeedStrategy::default());
    for t in ts {
        agg.consume_tick(Feed::Dhan, t, Some(u64::from(t.volume)), |_, _, _, _, _| {});
    }
    agg.snapshot(Feed::Dhan, 13, 0, TfIndex::M1)
}

/// The prices that should actually reach the fold.
fn sane_prices(ts: &[ParsedTick]) -> Vec<f64> {
    ts.iter()
        .filter(|t| tick_price_is_sane(t))
        .map(|t| f32_to_f64_clean(t.last_traded_price))
        .collect()
}

/// The `open` the day's FIRST bar is armed with.
///
/// `armed_for_day_open` makes the first bucket of the day open at the
/// exchange-published `tick.day_open` — the operator's 09:15 equilibrium
/// price — rather than at the first tick's LTP. So that value is part of the
/// bar's range and the extremes must widen to include it.
///
/// This is the operator's own rule implemented inside the fold, and omitting
/// it from the expectation was the harness error that made this file's first
/// version report a defect that did not exist: one tick at 0.05 produced a
/// high of 81995.2, which is the day-open, correctly.
fn armed_open(ts: &[ParsedTick]) -> Option<f64> {
    let first = ts.iter().find(|t| tick_price_is_sane(t))?;
    let day_open = f32_to_f64_clean(first.day_open);
    Some(if day_open > 0.0 {
        day_open
    } else {
        f32_to_f64_clean(first.last_traded_price)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// THE CENTRAL CLAIM. Arrival order must not change the recorded high or
    /// low. Ticks cross a network with no ordering promise and Dhan publishes
    /// no sequence number, so any dependence on order is a silent, unrepeatable
    /// error in the extremes the operator asked to capture.
    #[test]
    fn the_high_and_low_do_not_depend_on_arrival_order(
        ts in ticks(),
        seed in 0usize..1000,
    ) {
        let forward = fold_all(&ts);
        let mut shuffled = ts.clone();
        // A deterministic rotation-plus-reverse: no RNG, reproducible from the
        // shrunk input alone, and it reaches every cyclic order.
        let n = shuffled.len();
        shuffled.rotate_left(seed % n.max(1));
        if seed % 2 == 0 {
            shuffled.reverse();
        }
        let other = fold_all(&shuffled);

        let (Some(forward), Some(other)) = (forward, other) else {
            prop_assert!(forward.is_none() && other.is_none(), "one order opened a bucket and the other did not");
            return Ok(());
        };
        prop_assert_eq!(
            forward.high, other.high,
            "high moved with arrival order: {} vs {}", forward.high, other.high
        );
        prop_assert_eq!(
            forward.low, other.low,
            "low moved with arrival order: {} vs {}", forward.low, other.low
        );
    }

    /// The high IS the maximum sane price folded, and the low the minimum.
    /// Anything less is a lost extreme; anything more is invented.
    #[test]
    fn the_high_is_the_max_and_the_low_is_the_min_of_what_was_folded(ts in ticks()) {
        let got = fold_all(&ts);
        let sane = sane_prices(&ts);
        if sane.is_empty() {
            prop_assert!(got.is_none(), "a bucket opened with no sane price");
            return Ok(());
        }
        let Some(got) = got else {
            prop_assert!(false, "no bucket despite {} sane prices", sane.len());
            return Ok(());
        };
        // The day-armed open is part of the range — see `armed_open`.
        let mut want = sane.clone();
        if let Some(o) = armed_open(&ts) {
            want.push(o);
        }
        let want_high = want.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let want_low = want.iter().copied().fold(f64::INFINITY, f64::min);
        prop_assert_eq!(got.high, want_high, "high {} want {}", got.high, want_high);
        prop_assert_eq!(got.low, want_low, "low {} want {}", got.low, want_low);
    }

    /// The range contains the open and the close, which every consumer of a
    /// candle assumes and no consumer re-checks.
    #[test]
    fn the_bar_is_internally_consistent(ts in ticks()) {
        let Some(got) = fold_all(&ts) else { return Ok(()); };
        prop_assert!(got.low <= got.high, "low {} > high {}", got.low, got.high);
        prop_assert!(got.low <= got.close && got.close <= got.high,
            "close {} outside [{}, {}]", got.close, got.low, got.high);
        prop_assert!(got.low <= got.open && got.open <= got.high,
            "open {} outside [{}, {}]", got.open, got.low, got.high);
    }

    /// An insane price contributes NOTHING. NaN reaching `high` would make
    /// every later comparison false and freeze the extreme for the session —
    /// the absorbing-NaN class this repository was bitten by elsewhere.
    #[test]
    fn an_insane_price_never_reaches_the_bar(ts in ticks()) {
        let Some(got) = fold_all(&ts) else { return Ok(()); };
        for v in [got.open, got.high, got.low, got.close] {
            prop_assert!(v.is_finite() && v > 0.0, "a bar field is {}", v);
        }
        prop_assert_eq!(
            got.tick_count as usize,
            sane_prices(&ts).len(),
            "tick_count counts something other than the folded ticks"
        );
    }

    /// The close is the price of the LATEST exchange timestamp, last-write-wins
    /// within one second. This is the half that is deliberately NOT
    /// order-invariant, and the 2026-08-25 defect was exactly its absence on
    /// the open-bucket path.
    #[test]
    fn the_close_belongs_to_the_latest_timestamp(ts in ticks()) {
        let Some(got) = fold_all(&ts) else { return Ok(()); };
        let sane: Vec<&ParsedTick> = ts.iter().filter(|t| tick_price_is_sane(t)).collect();
        if sane.is_empty() {
            return Ok(());
        }
        let latest = sane.iter().map(|t| t.exchange_timestamp).max().unwrap_or(0);
        prop_assert_eq!(got.close_ts_ist_secs, latest);
        // The close must be SOME tick carrying that timestamp.
        let candidates: Vec<f64> = sane
            .iter()
            .filter(|t| t.exchange_timestamp == latest)
            .map(|t| f32_to_f64_clean(t.last_traded_price))
            .collect();
        prop_assert!(
            candidates.contains(&got.close),
            "close {} is not any price at the latest timestamp {:?}",
            got.close, candidates
        );
    }

    /// Volume within a bucket only ever widens. A stale packet carries a
    /// smaller day-cumulative, and letting it drag the bar down moves volume
    /// BETWEEN buckets rather than leaving it where it traded.
    #[test]
    fn bucket_volume_never_regresses(ts in ticks()) {
        let mut agg = MultiTfAggregator::new(FeedStrategy::default());
        let mut last = 0u64;
        for t in &ts {
            agg.consume_tick(Feed::Dhan, t, Some(u64::from(t.volume)), |_, _, _, _, _| {});
            let Some(now) = agg.snapshot(Feed::Dhan, 13, 0, TfIndex::M1).map(|s| s.volume) else { continue; };
            prop_assert!(now >= last, "volume fell {} -> {}", last, now);
            last = now;
        }
    }

    /// Open interest: last NON-ZERO wins. Zero is the absent sentinel — a
    /// Ticker packet has no OI field and an equity never has one — so a newer
    /// blank packet must not erase a real reading from earlier in the bucket.
    #[test]
    fn a_blank_open_interest_never_erases_a_real_one(ts in ticks()) {
        let Some(got) = fold_all(&ts) else { return Ok(()); };
        let any_real_oi = ts
            .iter()
            .any(|t| tick_price_is_sane(t) && t.open_interest != 0);
        if any_real_oi {
            prop_assert_ne!(got.oi, 0, "a real open interest was erased by a blank one");
        }
    }

    /// Folding the same ticks twice gives the same bar. A fold that depended
    /// on process state would make a replayed session disagree with the live
    /// one, which is what the cross-verification compares.
    #[test]
    fn folding_the_same_ticks_twice_gives_the_same_bar(ts in ticks()) {
        let (Some(a), Some(b)) = (fold_all(&ts), fold_all(&ts)) else { return Ok(()); };
        prop_assert_eq!(a.open, b.open);
        prop_assert_eq!(a.high, b.high);
        prop_assert_eq!(a.low, b.low);
        prop_assert_eq!(a.close, b.close);
        prop_assert_eq!(a.volume, b.volume);
        prop_assert_eq!(a.tick_count, b.tick_count);
    }

    /// It never panics, on any interleaving of any of these shapes.
    #[test]
    fn it_never_panics(ts in ticks()) {
        let _ = fold_all(&ts);
    }
}
