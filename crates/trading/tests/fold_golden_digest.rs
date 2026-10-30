//! Old-vs-new check for the per-tick candle fold (plan ITEM DT, sub-µs fold).
//!
//! The fold was restructured for speed. Every sealed bar, every amendment and
//! every per-tick verdict must be IDENTICAL to what the code produced before
//! the change. This test drives seeded, deterministic tick streams through the
//! public aggregator API and folds every emission (the full bar, field by
//! field, through its `Debug` text) and every `ConsumeStats` into one FNV-1a
//! digest per scenario class. The expected digests were recorded on the code
//! BEFORE the speed change (commit `3b26257`, branch `perf/sub-us`); a single
//! changed bit anywhere in any bar or verdict changes a digest.
//!
//! The streams cover: live folding across the 09:15 open and the 10:00 hour,
//! pre-open quotes, a replay with gaps and a hand-over, a mid-session capture
//! start, the day-close seal, a mid-session withhold, a `u32` cumulative wrap,
//! an index with volume always 0, out-of-order and stale packets, exact repeat
//! quotes, unusable day extremes (0, NaN, infinity), receipts on a later day,
//! future-dated trades, and a second trading day.
//!
//! Deterministic by construction: a fixed xorshift seed per stream, no clock,
//! no hash-map iteration in the output path.

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;

/// An exact multiple of 86_400 (00:00 IST of the test day).
const DAY: u32 = 1_779_321_600;
const SECS_PER_DAY: u32 = 86_400;
const IST_OFFSET_SECS: i64 = 19_800;
/// Streams per scenario class.
const STREAMS_PER_CLASS: u64 = 120;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1_000) < per_mille
    }
}

struct Digest(u64, Tally);

/// How often each fold outcome occurred, so the digest is shown to cover them.
#[derive(Default, Debug, Clone, Copy)]
struct Tally {
    emitted: u64,
    sealed: u64,
    amended: u64,
    late: u64,
    refused_price: u64,
    out_of_session: u64,
    future_trading_day: u64,
    untraded_sentinel: u64,
    untraded_timestamp: u64,
    out_of_band_timestamp: u64,
    refused_timestamp: u64,
    stale_trading_day: u64,
    repeat_quote: u64,
    replay_partial_suppressed: u64,
}

impl Tally {
    fn add(&mut self, s: &tickvault_trading::candles::multi_tf_aggregator::ConsumeStats) {
        self.sealed += u64::from(s.sealed_count);
        self.amended += u64::from(s.amended_count);
        self.late += u64::from(s.late_count);
        self.refused_price += u64::from(s.refused_price);
        self.out_of_session += u64::from(s.out_of_session);
        self.future_trading_day += u64::from(s.future_trading_day);
        self.untraded_sentinel += u64::from(s.untraded_sentinel);
        self.untraded_timestamp += u64::from(s.untraded_timestamp);
        self.out_of_band_timestamp += u64::from(s.out_of_band_timestamp);
        self.refused_timestamp += u64::from(s.refused_timestamp);
        self.stale_trading_day += u64::from(s.stale_trading_day);
        self.repeat_quote += u64::from(s.repeat_quote);
        self.replay_partial_suppressed += u64::from(s.replay_partial_suppressed);
    }
}

impl Digest {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325, Tally::default())
    }
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn text(&mut self, s: &str) {
        self.bytes(s.as_bytes());
        self.bytes(&[0xff]);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    /// Plain live session folding across the open and the hour.
    Live,
    /// Live with mid-session capture start (a boot after the open).
    MidSessionBoot,
    /// A replay with gaps, then the hand-over, then live.
    ReplayHandover,
    /// Live, ending on the day-close seal, then a second day.
    DayClose,
    /// Stocks start just below `u32::MAX` and wrap.
    NearWrap,
    /// Out-of-order, stale, repeat and late-dated packets, heavy.
    Disorder,
    /// Unusable prices and extremes, pre-open quotes, refused ticks.
    Hostile,
    /// Mid-session clean exit: catch-up then withhold.
    Withhold,
}

const CLASSES: [Class; 8] = [
    Class::Live,
    Class::MidSessionBoot,
    Class::ReplayHandover,
    Class::DayClose,
    Class::NearWrap,
    Class::Disorder,
    Class::Hostile,
    Class::Withhold,
];

struct Instrument {
    security_id: u64,
    segment: u8,
    is_index: bool,
    cumulative: u64,
    price: f32,
    trade: u32,
    high: f32,
    low: f32,
    open: f32,
    oi: u32,
    last: Option<ParsedTick>,
}

fn emit(d: &mut Digest, feed: Feed, sid: u64, seg: u8, tf: usize, bar: &impl std::fmt::Debug) {
    d.1.emitted += 1;
    d.text(&format!("{feed:?}|{sid}|{seg}|{tf}|{bar:?}"));
}

fn odd_price(rng: &mut Rng, normal: f32) -> f32 {
    match rng.below(8) {
        0 => 0.0,
        1 => f32::NAN,
        2 => f32::INFINITY,
        3 => -normal,
        4 => f32::MIN_POSITIVE / 2.0,
        _ => normal,
    }
}

fn run_stream(class: Class, seed: u64, d: &mut Digest) {
    let mut rng = Rng::new(seed ^ ((class as u64) << 40));
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 8);
    let margin = [0u32, 5, 60, 240][rng.below(4) as usize];
    agg.set_catch_up_margin_secs(margin);
    agg.seed_watermark_at_least(DAY);

    let n_instruments = 1 + rng.below(4) as usize;
    let mut inst: Vec<Instrument> = (0..n_instruments)
        .map(|i| {
            let is_index = i == 0 && rng.chance(500);
            let wrap = class == Class::NearWrap && !is_index;
            Instrument {
                security_id: if is_index { 13 } else { 100 + i as u64 },
                segment: if is_index { 0 } else { [1u8, 2, 1, 2][i % 4] },
                is_index,
                cumulative: if wrap {
                    u64::from(u32::MAX) - rng.below(2_000)
                } else {
                    rng.below(5_000)
                },
                price: 100.0 + rng.below(2_000) as f32 * 0.05,
                trade: 0,
                high: 0.0,
                low: f32::MAX,
                open: 0.0,
                oi: rng.below(3) as u32 * 1_000,
                last: None,
            }
        })
        .collect();

    // Session start: before the open, at the open, or later in the day.
    let start_secs = match rng.below(6) {
        0 => 32_400 + rng.below(900) as u32,   // 09:00..09:15 pre-open
        1 => 33_300,                           // 09:15:00
        2 => 33_300 + rng.below(600) as u32,   // early session
        3 => 35_700 + rng.below(600) as u32,   // across 10:00
        4 => 54_600 + rng.below(1_000) as u32, // near the close
        _ => 33_300 + rng.below(20_000) as u32,
    };
    for i in &mut inst {
        i.trade = DAY + start_secs;
    }

    let mut replaying = false;
    if class == Class::ReplayHandover {
        agg.set_replay_mode(true);
        replaying = true;
    }
    if class == Class::MidSessionBoot {
        agg.set_live_capture_start(DAY + start_secs + rng.below(120) as u32);
    }
    let handover_at = 60 + rng.below(200);
    let ticks = 80 + rng.below(320);
    let mut day = DAY;

    for k in 0..ticks {
        // Replay hand-over.
        if replaying && k == handover_at {
            let ended_on_gap = rng.chance(300);
            let (a, b) = agg.finish_replay(ended_on_gap, |f, s, g, tf, bar| {
                emit(d, f, s, g, tf.as_ordinal(), &bar);
            });
            d.text(&format!("finish|{a}|{b}"));
            agg.set_replay_mode(false);
            replaying = false;
            if rng.chance(500) {
                let t = inst[0].trade + rng.below(60) as u32;
                agg.set_live_capture_start(t);
            }
        }
        if replaying && rng.chance(25) {
            agg.mark_replay_gap();
            d.text("gap");
        }
        // A catch-up sweep now and then.
        if rng.chance(40) {
            let cutoff = agg.watermark_secs().saturating_sub(margin);
            let n = agg.catch_up_seal_all(cutoff, |f, s, g, tf, bar| {
                emit(d, f, s, g, tf.as_ordinal(), &bar);
            });
            d.text(&format!("sweep|{n}"));
        }
        // Day close and a second day.
        if class == Class::DayClose && k == ticks / 2 {
            let n = agg.force_seal_all(|f, s, g, tf, bar| {
                emit(d, f, s, g, tf.as_ordinal(), &bar);
            });
            d.text(&format!("dayclose|{n}"));
            day += SECS_PER_DAY;
            for i in &mut inst {
                i.trade = day + 33_300 + rng.below(300) as u32;
                i.high = 0.0;
                i.low = f32::MAX;
                i.open = 0.0;
                if rng.chance(500) {
                    i.cumulative = rng.below(100);
                }
            }
        }

        let which = rng.below(inst.len() as u64) as usize;
        let i = &mut inst[which];
        let disorder = matches!(class, Class::Disorder) || rng.chance(30);
        let roll = rng.below(1_000);

        let tick = if disorder && roll < 120 && i.last.is_some() {
            // Exact repeat of the last packet, maybe with fresh book fields.
            let mut t = i.last.unwrap_or_default();
            if rng.chance(500) {
                t.open_interest = i.oi + rng.below(50) as u32;
                t.total_buy_quantity = rng.below(10_000) as u32;
            }
            t.received_at_nanos += rng.below(3_000) as i64 * 1_000_000;
            t
        } else if disorder && roll < 220 && i.last.is_some() {
            // Out of order: an older trade time and an older cumulative.
            let mut t = i.last.unwrap_or_default();
            t.exchange_timestamp = t.exchange_timestamp.saturating_sub(rng.below(400) as u32);
            t.volume = t.volume.wrapping_sub(rng.below(30) as u32);
            t.last_traded_price = i.price + (rng.below(9) as f32 - 4.0) * 0.05;
            t
        } else {
            let dt = match rng.below(20) {
                0 => rng.below(900) as u32,
                1 => 0,
                _ => rng.below(25) as u32,
            };
            i.trade = i.trade.saturating_add(dt);
            let step = (rng.below(7) as f32 - 3.0) * 0.05;
            i.price = (i.price + step).max(0.05);
            if !i.is_index && rng.below(10) != 0 {
                i.cumulative += rng.below(60);
            }
            if i.open == 0.0 {
                i.open = i.price;
            }
            i.high = i.high.max(i.price);
            i.low = i.low.min(i.price);
            if rng.chance(50) {
                i.oi = i.oi.saturating_add(rng.below(100) as u32);
            }
            let lag_ms: i64 = match rng.below(40) {
                0 => -(rng.below(20_000) as i64),
                1 => rng.below(400_000) as i64,
                2 => i64::from(SECS_PER_DAY) * 1_000,
                _ => rng.below(3_000) as i64,
            };
            let received_at_nanos = if rng.chance(15) {
                0
            } else {
                (i64::from(i.trade) - IST_OFFSET_SECS) * 1_000_000_000 + lag_ms * 1_000_000
            };
            let hostile = class == Class::Hostile;
            let (dh, dl, dopen) = if hostile || rng.chance(20) {
                (
                    odd_price(&mut rng, i.high + 0.5),
                    odd_price(&mut rng, i.low - 0.5),
                    odd_price(&mut rng, i.open),
                )
            } else {
                (
                    i.high + [0.0, 0.0, 0.1, 1.0][rng.below(4) as usize],
                    (i.low - [0.0, 0.0, 0.1, 1.0][rng.below(4) as usize]).max(0.05),
                    if rng.chance(100) { 0.0 } else { i.open },
                )
            };
            let ltp = if hostile && rng.chance(150) {
                odd_price(&mut rng, i.price)
            } else {
                i.price
            };
            let ts = if hostile && rng.chance(40) {
                [0u32, 1, u32::MAX, i.trade + 400][rng.below(4) as usize]
            } else {
                i.trade
            };
            ParsedTick {
                security_id: i.security_id,
                exchange_segment_code: i.segment,
                last_traded_price: ltp,
                last_trade_quantity: rng.below(60) as u16,
                exchange_timestamp: ts,
                received_at_nanos,
                volume: (i.cumulative % (1u64 << 32)) as u32,
                total_buy_quantity: rng.below(50_000) as u32,
                total_sell_quantity: if rng.chance(100) {
                    0
                } else {
                    rng.below(50_000) as u32
                },
                open_interest: if i.is_index { 0 } else { i.oi },
                day_open: dopen,
                day_close: if rng.chance(50) { 0.0 } else { 99.5 },
                day_high: dh,
                day_low: dl,
                ..Default::default()
            }
        };
        if tick.exchange_timestamp == i.trade {
            i.last = Some(tick);
        }
        let over = if rng.chance(30) && !i.is_index {
            Some(i.cumulative + (1u64 << 33))
        } else {
            None
        };
        let feed = Feed::Dhan;
        let stats = agg.consume_tick(feed, &tick, over, |f, s, g, tf, bar| {
            emit(d, f, s, g, tf.as_ordinal(), &bar);
        });
        d.1.add(&stats);
        d.text(&format!("{stats:?}"));
    }

    // End of stream.
    match class {
        Class::Withhold => {
            let cutoff = agg.watermark_secs().saturating_sub(margin);
            let n = agg.catch_up_seal_all(cutoff, |f, s, g, tf, bar| {
                emit(d, f, s, g, tf.as_ordinal(), &bar);
            });
            let (a, b) = agg.withhold_open_buckets();
            d.text(&format!("withhold|{n}|{a}|{b}"));
        }
        _ => {
            if replaying {
                let (a, b) = agg.finish_replay(false, |f, s, g, tf, bar| {
                    emit(d, f, s, g, tf.as_ordinal(), &bar);
                });
                d.text(&format!("finish|{a}|{b}"));
                agg.set_replay_mode(false);
            }
            let n = agg.force_seal_all(|f, s, g, tf, bar| {
                emit(d, f, s, g, tf.as_ordinal(), &bar);
            });
            d.text(&format!("end|{n}"));
        }
    }
    for i in &inst {
        let ltp = agg.last_ltp(Feed::Dhan, i.security_id, i.segment);
        d.text(&format!("ltp|{ltp:?}"));
    }
    d.text(&format!("wm|{}", agg.watermark_secs()));
}

fn class_run(class: Class) -> Digest {
    let mut d = Digest::new();
    for seed in 1..=STREAMS_PER_CLASS {
        run_stream(class, seed, &mut d);
    }
    d
}

fn class_digest(class: Class) -> u64 {
    class_run(class).0
}

/// Recorded on the code before the speed change (commit `3b26257`).
const EXPECTED: [(Class, u64); 8] = [
    (Class::Live, 0xc98b_ba8e_8df0_ea32),
    (Class::MidSessionBoot, 0xfadc_15ae_92d0_0143),
    (Class::ReplayHandover, 0x9158_e833_1c52_980a),
    (Class::DayClose, 0xbf71_5633_81d7_4970),
    (Class::NearWrap, 0x9e92_e6b4_2380_59c8),
    (Class::Disorder, 0x0683_1715_f4b8_140b),
    (Class::Hostile, 0xaa08_40fe_708a_1d29),
    (Class::Withhold, 0x7b8a_e345_922c_6f78),
];

#[test]
fn fold_output_is_identical_to_the_pre_speed_change_fold() {
    let mut mismatches = Vec::new();
    for (class, expected) in EXPECTED {
        let got = class_digest(class);
        if got != expected {
            mismatches.push(format!(
                "{class:?}: expected {expected:#018x}, got {got:#018x}"
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "fold output changed:\n{}",
        mismatches.join("\n")
    );
}

/// The digest is not vacuous: every class emits sealed bars, and the classes
/// differ from each other.
#[test]
fn every_class_produces_a_distinct_digest() {
    let mut seen = std::collections::HashSet::new();
    for class in CLASSES {
        assert!(
            seen.insert(class_digest(class)),
            "{class:?} repeats a digest"
        );
    }
}

/// The streams reach every fold outcome the digest is meant to cover, so a
/// matching digest is evidence about those paths and not about an empty run.
#[test]
fn the_streams_reach_every_fold_outcome() {
    let mut total = Tally::default();
    for class in CLASSES {
        let t = class_run(class).1;
        assert!(
            t.emitted > 0 && t.sealed > 0,
            "{class:?} sealed nothing: {t:?}"
        );
        total.emitted += t.emitted;
        total.sealed += t.sealed;
        total.amended += t.amended;
        total.late += t.late;
        total.refused_price += t.refused_price;
        total.out_of_session += t.out_of_session;
        total.future_trading_day += t.future_trading_day;
        total.untraded_sentinel += t.untraded_sentinel;
        total.untraded_timestamp += t.untraded_timestamp;
        total.out_of_band_timestamp += t.out_of_band_timestamp;
        total.refused_timestamp += t.refused_timestamp;
        total.stale_trading_day += t.stale_trading_day;
        total.repeat_quote += t.repeat_quote;
        total.replay_partial_suppressed += t.replay_partial_suppressed;
    }
    println!("{total:?}");
    let every = [
        total.amended,
        total.late,
        total.refused_price,
        total.out_of_session,
        total.future_trading_day,
        total.untraded_sentinel,
        total.out_of_band_timestamp,
        total.refused_timestamp,
        total.stale_trading_day,
        total.repeat_quote,
        total.replay_partial_suppressed,
    ];
    assert!(
        every.iter().all(|&n| n > 0),
        "an outcome is never reached: {total:?}"
    );
}
