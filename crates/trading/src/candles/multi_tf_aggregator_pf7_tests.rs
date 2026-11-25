//! Plan item PF-7 (2026-11-06): the fold's cache warm-up changes nothing.
//!
//! The warm-up (`AggregatorCell::warm_for_tick`, called from `consume_tick`
//! right after the index lookup) only reads. This proptest folds the same
//! random stream into two aggregators, one with the warm-up switched off on
//! this thread (the old fold) and one with it on (the new fold), and requires
//! every emitted bar, every per-tick outcome, every sweep result and the whole
//! slot state to be identical after every step. The stream mixes several
//! instruments and segments, out-of-order and stale packets, cumulative
//! counter wraps, catch-up seals, the day-close seal, a day rollover and the
//! replay hand-over. Kept out of the main file so its size pin does not move.

use proptest::prelude::*;

use super::tests::{OPEN, tick};
use super::*;
use crate::candles::fold_warm_up::with_warm_up_off;

/// One emitted bar.
type Emitted = (Feed, u64, u8, TfIndex, LiveCandleState);

/// The instruments the stream trades: two stock options, one stock (NSE_EQ)
/// and one index (volume always 0).
const INSTRUMENTS: [(u64, u8); 4] = [(68_407, 2), (68_408, 2), (2_885, 1), (13, 0)];

/// IST is UTC + 5:30.
const IST_OFFSET_SECS: i64 = 19_800;

#[derive(Clone, Debug)]
enum Op {
    /// A tick for `INSTRUMENTS[inst]`: the clock moves by `dt` seconds (may be
    /// negative: out of order), the cumulative by `dvol` (negative: stale),
    /// received `delay` seconds after its trade.
    Tick {
        inst: usize,
        dt: i32,
        price_step: u16,
        dvol: i32,
        oi: u32,
        delay: u8,
    },
    /// The catch-up seal at `lag` seconds behind the clock.
    CatchUp { lag: u16 },
    /// The day-close seal.
    ForceSeal,
    /// Next trading day, 09:15 plus `offset` seconds.
    NextDay { offset: u16 },
    /// Enter replay mode.
    ReplayOn,
    /// A replay gap.
    Gap,
    /// The replay to live hand-over.
    FinishReplay { ended_on_gap: bool },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        12 => (0usize..4, -150i32..=150, 0u16..400, -60i32..=600, 0u32..100_000, 0u8..4)
            .prop_map(|(inst, dt, price_step, dvol, oi, delay)| Op::Tick {
                inst,
                dt,
                price_step,
                dvol,
                oi,
                delay,
            }),
        2 => (0u16..400).prop_map(|lag| Op::CatchUp { lag }),
        1 => Just(Op::ForceSeal),
        1 => (0u16..3_600).prop_map(|offset| Op::NextDay { offset }),
        1 => Just(Op::ReplayOn),
        1 => Just(Op::Gap),
        1 => any::<bool>().prop_map(|ended_on_gap| Op::FinishReplay { ended_on_gap }),
    ]
}

/// One side of the comparison: an aggregator and everything it emitted.
struct Side {
    agg: MultiTfAggregator,
    emitted: Vec<Emitted>,
}

impl Side {
    fn new() -> Self {
        Self {
            agg: MultiTfAggregator::new(FeedStrategy::DEFAULT),
            emitted: Vec::new(),
        }
    }

    /// Applies `op` with the shared stream state; returns what it reported.
    fn apply(&mut self, op: &Op, t: Option<&ParsedTick>, clock: u32) -> String {
        let emitted = &mut self.emitted;
        let mut sink = |f: Feed, s: u64, g: u8, tf: TfIndex, b: LiveCandleState| {
            emitted.push((f, s, g, tf, b))
        };
        match op {
            Op::Tick { .. } => match t {
                Some(t) => format!(
                    "{:?}",
                    self.agg.consume_tick(Feed::Dhan, t, None, &mut sink)
                ),
                None => String::new(),
            },
            Op::CatchUp { lag } => format!(
                "{}",
                self.agg
                    .catch_up_seal_all(clock.saturating_sub(u32::from(*lag)), &mut sink)
            ),
            Op::ForceSeal => format!("{}", self.agg.force_seal_all(&mut sink)),
            Op::NextDay { .. } => String::new(),
            Op::ReplayOn => {
                self.agg.set_replay_mode(true);
                String::new()
            }
            Op::Gap => {
                self.agg.mark_replay_gap();
                String::new()
            }
            Op::FinishReplay { ended_on_gap } => {
                format!("{:?}", self.agg.finish_replay(*ended_on_gap, &mut sink))
            }
        }
    }

    /// Every slot field plus the aggregator's own scalars.
    fn state(&self) -> String {
        let a = &self.agg;
        format!(
            "{:?} | {} {} {} {} {} {} {} {}",
            a.slots,
            a.watermark_secs,
            a.slots_exhausted_total,
            a.replay_mode,
            a.replay_gap_epoch,
            a.replay_suppressed_total,
            a.live_capture_from_secs,
            a.replay_last_frame_secs,
            a.capture_start_provisional,
        )
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn proptest_warm_up_never_changes_a_bar(
        ops in proptest::collection::vec(op(), 1..120),
        wrap_start in any::<bool>(),
    ) {
        let mut old = Side::new();
        let mut new = Side::new();
        let mut clock = OPEN + 60;
        let mut day_end = OPEN + 22_000;
        // Start two instruments just below the u32 cumulative wrap when asked.
        let mut cum: [u32; 4] = if wrap_start {
            [u32::MAX - 2_000, 50_000, u32::MAX - 500, 0]
        } else {
            [10_000, 50_000, 3_000, 0]
        };
        for (step, op) in ops.iter().enumerate() {
            let mut t = None;
            match op {
                Op::Tick { inst, dt, price_step, dvol, oi, delay } => {
                    let day_open = day_end - 22_000;
                    clock = clock.saturating_add_signed(*dt).clamp(day_open, day_end);
                    let (sid, seg) = INSTRUMENTS[*inst];
                    if seg != 0 {
                        cum[*inst] = cum[*inst].wrapping_add_signed(*dvol);
                    }
                    let mut p = tick(sid, seg, clock, 95.0 + f32::from(*price_step) * 0.05, cum[*inst]);
                    p.open_interest = *oi;
                    p.day_open = 100.0;
                    p.day_close = 99.5;
                    p.received_at_nanos = (i64::from(clock) + i64::from(*delay) - IST_OFFSET_SECS)
                        * 1_000_000_000;
                    t = Some(p);
                }
                Op::NextDay { offset } => {
                    day_end += 86_400;
                    clock = day_end - 22_000 + u32::from(*offset);
                }
                _ => {}
            }
            let old_out = with_warm_up_off(|| old.apply(op, t.as_ref(), clock));
            let new_out = new.apply(op, t.as_ref(), clock);
            prop_assert_eq!(&old_out, &new_out, "step {} {:?}: outcome differs", step, op);
            prop_assert_eq!(
                format!("{:?}", old.emitted),
                format!("{:?}", new.emitted),
                "step {} {:?}: emitted bars differ", step, op
            );
            prop_assert_eq!(old.state(), new.state(), "step {} {:?}: slot state differs", step, op);
        }
    }
}

/// The proptest above would pass vacuously if the stream never folded,
/// sealed or rolled a timeframe the warm-up reads ahead for. This fixed stream
/// does all three, with the warm-up on for one side and off for the other.
/// (Its folded word is not checked here: identical buckets cancel under XOR,
/// so zero is a legitimate value. `fold_warm_up::tests` pins which fields it
/// reads.)
#[test]
fn test_warm_for_tick_runs_on_a_rolling_stream_and_matches_the_old_fold() {
    let mut old = Side::new();
    let mut new = Side::new();
    let mut seals = 0usize;
    for n in 0..400u32 {
        let ts = OPEN + 1 + n / 2;
        let mut p = tick(
            68_407,
            2,
            ts,
            100.0 + (n % 7) as f32 * 0.05,
            10_000 + n * 25,
        );
        p.received_at_nanos = (i64::from(ts) - IST_OFFSET_SECS) * 1_000_000_000;
        let op = Op::Tick {
            inst: 0,
            dt: 0,
            price_step: 0,
            dvol: 0,
            oi: 0,
            delay: 0,
        };
        let a = with_warm_up_off(|| old.apply(&op, Some(&p), ts));
        let b = new.apply(&op, Some(&p), ts);
        assert_eq!(a, b);
        // The new side runs the warm-up; the old side does not.
        assert!(crate::candles::fold_warm_up::warm_up_enabled());
        assert!(!with_warm_up_off(
            crate::candles::fold_warm_up::warm_up_enabled
        ));
        seals = new.emitted.len();
    }
    assert!(seals > 100, "the stream must roll buckets, sealed {seals}");
    assert_eq!(format!("{:?}", old.emitted), format!("{:?}", new.emitted));
    assert_eq!(old.state(), new.state());
}
