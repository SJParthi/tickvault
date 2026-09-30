//! Randomized restart differential for the candle fold (plan ITEM 47,
//! review rounds 18 and 19).
//!
//! One market is folded twice. The TRUTH run folds every packet live, as a
//! process that never stopped would. The RESTART run is what production does
//! after a crash: a previous process folded every packet received by the
//! crash (and stored what it sealed); the new process replays that WAL, with
//! random frames unreadable, hands over at a capture start after the crash,
//! and folds live from there. Each instrument's socket starts listening at
//! its own time at or after the capture start, so a packet received in the
//! downtime is lost, and a subscribe may re-send the last trade. Packets
//! arrive late, out of trade order, repeated, and some never at all.
//!
//! Every bar the restart writes is compared with the truth, and with what the
//! previous process stored (a third run, live up to the crash):
//!
//! 1. no bar ever carries MORE volume than the truth (a double count);
//! 2. a bucket the previous process stored is written identical to the truth,
//!    net volume included, or not written;
//! 3. any other bar that is written equals the truth in volume and OHLC, and
//!    a bar that classifies its net volume carries the truth's (an
//!    unclassified one says the direction is unknown, which is honest);
//! 4. a bucket missing after the crash started by the moment its instrument
//!    was known to be captured: the confirmed capture start or the
//!    instrument's own first live receipt.
//!
//! No bar is exempt from 3. Until review round 19 the bar holding the first
//! live trade was, and that hid a real defect: a bucket that began in the
//! downtime was written without the downtime's trades and prices. A withheld
//! bar is counted (`tv_candle_refold_partial_suppressed_total`); a wrong bar
//! would overwrite a stored row or publish a wrong one, which is what this
//! file exists to catch.
//!
//! 256 cases per run. Measured on the round-19 fix: 20,000 cases with zero
//! failures, in about 56 s in a release build; the round-18 aggregator fails
//! these checks 32,761 times over the same 20,000 cases.

use std::collections::HashMap;

use proptest::prelude::*;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;
use tickvault_trading::candles::{LiveCandleState, TfIndex};

/// An IST trading day's midnight, in IST epoch seconds.
const DAY: u32 = 1_779_321_600;
/// 09:16:00 IST: the first trade second.
const START: u32 = DAY + 33_360;
/// The live catch-up margin production runs with.
const MARGIN: u32 = 240;
/// A capture start well before the session, for the uninterrupted runs.
const EARLY_CAPTURE: u32 = DAY + 32_000;
/// IST minus UTC, in milliseconds.
const IST_OFFSET_MS: i64 = 19_800_000;
/// Out of 100: a replayed WAL frame below this is unreadable.
const UNREADABLE_PERCENT: u8 = 4;

#[derive(Clone, Debug)]
struct Event {
    dt: u8,
    size: u16,
    price_steps: i8,
    lag_ms: u32,
    resend_ms: Option<u16>,
    delivered: bool,
}

#[derive(Clone, Debug)]
struct Case {
    instruments: Vec<Vec<Event>>,
    crash_per_mille: u16,
    start_after_crash: u32,
    listen_after_start: Vec<u32>,
    unreadable: Vec<u8>,
    resend_on_subscribe: Vec<bool>,
}

fn event() -> impl Strategy<Value = Event> {
    (
        1u8..=20,
        0u16..=40,
        -3i8..=3,
        prop_oneof![30 => 0u32..=10_000, 1 => 10_001u32..=200_000],
        prop_oneof![9 => Just(None), 1 => (0u16..=3_000).prop_map(Some)],
        prop_oneof![9 => Just(true), 1 => Just(false)],
    )
        .prop_map(
            |(dt, size, price_steps, lag_ms, resend_ms, delivered)| Event {
                dt,
                size,
                price_steps,
                lag_ms,
                resend_ms,
                delivered,
            },
        )
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(prop::collection::vec(event(), 1..150), 2..=4),
        50u16..=950,
        0u32..=30,
        prop::collection::vec(0u32..=20, 4),
        prop::collection::vec(0u8..100, 64..600),
        prop::collection::vec(any::<bool>(), 4),
    )
        .prop_map(
            |(
                instruments,
                crash_per_mille,
                start_after_crash,
                listen_after_start,
                unreadable,
                resend_on_subscribe,
            )| Case {
                instruments,
                crash_per_mille,
                start_after_crash,
                listen_after_start,
                unreadable,
                resend_on_subscribe,
            },
        )
}

#[derive(Clone, Copy, Debug)]
struct Packet {
    instrument: usize,
    trade: u32,
    cumulative: u32,
    price: f32,
    high: f32,
    low: f32,
    open: f32,
    received_ms: u64,
}

/// Instrument 0 is an index (NSE index segment, volume always 0); the rest
/// are stocks.
fn security_id(instrument: usize) -> u64 {
    if instrument == 0 {
        13
    } else {
        100 + instrument as u64
    }
}

fn tick(p: &Packet, received_ms: u64) -> ParsedTick {
    ParsedTick {
        security_id: security_id(p.instrument),
        exchange_segment_code: u8::from(p.instrument != 0),
        last_traded_price: p.price,
        exchange_timestamp: p.trade,
        volume: p.cumulative,
        day_open: p.open,
        day_close: 1_000.0,
        day_high: p.high,
        day_low: p.low,
        received_at_nanos: (received_ms as i64 - IST_OFFSET_MS) * 1_000_000,
        ..Default::default()
    }
}

/// Every delivered packet, in receipt order.
fn packets(c: &Case) -> Vec<Packet> {
    let mut out = Vec::new();
    for (instrument, events) in c.instruments.iter().enumerate() {
        let (mut trade, mut cumulative, mut price) = (START, 0u32, 1_000.0f32);
        let (mut high, mut low, mut open, mut last_received) = (0f32, f32::MAX, 0f32, 0u64);
        for e in events {
            trade += u32::from(e.dt);
            if instrument != 0 {
                cumulative += u32::from(e.size);
            }
            price = (price + f32::from(e.price_steps) * 0.05).max(1.0);
            if open == 0.0 {
                open = price;
            }
            high = high.max(price);
            low = low.min(price);
            if !e.delivered {
                continue;
            }
            last_received = last_received.max(u64::from(trade) * 1000 + u64::from(e.lag_ms));
            let p = Packet {
                instrument,
                trade,
                cumulative,
                price,
                high,
                low,
                open,
                received_ms: last_received,
            };
            out.push(p);
            if let Some(resend) = e.resend_ms {
                last_received += u64::from(resend);
                out.push(Packet {
                    received_ms: last_received,
                    ..p
                });
            }
        }
    }
    out.sort_by_key(|p| (p.received_ms, p.instrument, p.cumulative));
    out
}

/// `(security_id, timeframe ordinal, bucket start)`.
type Key = (u64, usize, u32);

#[derive(Default)]
struct Written {
    last: HashMap<Key, LiveCandleState>,
    max_volume: HashMap<Key, u64>,
}

impl Written {
    fn put(&mut self, sid: u64, tf: TfIndex, bar: LiveCandleState) {
        let key = (sid, tf.as_ordinal(), bar.bucket_start_ist_secs);
        let max = self.max_volume.entry(key).or_insert(0);
        *max = (*max).max(bar.volume);
        self.last.insert(key, bar);
    }
}

fn aggregator() -> MultiTfAggregator {
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 16);
    agg.set_catch_up_margin_secs(MARGIN);
    agg.seed_watermark_at_least(DAY);
    agg
}

/// The live catch-up sweep the app runs: `min(watermark, wall) - margin`.
fn sweep(agg: &mut MultiTfAggregator, wall_secs: u32, out: &mut Written) {
    let cutoff = agg.watermark_secs().min(wall_secs).saturating_sub(MARGIN);
    agg.catch_up_seal_all(cutoff, |_, sid, _, tf, bar| out.put(sid, tf, bar));
}

/// Folds `packets` live from a capture start before the session.
fn fold_live<'a>(
    packets: impl Iterator<Item = &'a Packet>,
    out: &mut Written,
) -> MultiTfAggregator {
    let mut agg = aggregator();
    agg.set_live_capture_start(EARLY_CAPTURE);
    for p in packets {
        agg.consume_tick(
            Feed::Dhan,
            &tick(p, p.received_ms),
            None,
            |_, sid, _, tf, bar| out.put(sid, tf, bar),
        );
        sweep(&mut agg, (p.received_ms / 1000) as u32, out);
    }
    agg
}

fn same(a: &LiveCandleState, b: &LiveCandleState, with_net: bool) -> bool {
    a.volume == b.volume
        && a.open == b.open
        && a.high == b.high
        && a.low == b.low
        && a.close == b.close
        && (!with_net
            || (a.net_volume_signed == b.net_volume_signed
                && a.net_volume_classified == b.net_volume_classified))
}

fn describe(bar: Option<&LiveCandleState>) -> String {
    bar.map_or_else(
        || "absent".to_string(),
        |b| {
            format!(
                "v{} o{} h{} l{} c{} net {} {}",
                b.volume,
                b.open,
                b.high,
                b.low,
                b.close,
                b.net_volume_signed,
                b.net_volume_classified
            )
        },
    )
}

fn check(c: &Case) -> Result<(), TestCaseError> {
    let all = packets(c);
    let (Some(first), Some(last)) = (all.first(), all.last()) else {
        return Ok(());
    };
    let end_cutoff = all.iter().map(|p| p.trade).max().unwrap_or(START) + 10_000;

    let mut truth = Written::default();
    let mut truth_agg = fold_live(all.iter(), &mut truth);
    truth_agg.catch_up_seal_all(end_cutoff, |_, sid, _, tf, bar| truth.put(sid, tf, bar));

    let crash_ms = first.received_ms
        + (last.received_ms - first.received_ms) * u64::from(c.crash_per_mille) / 1000;
    let crash_secs = (crash_ms / 1000) as u32;
    let capture_start = crash_secs + 1 + c.start_after_crash;
    let listen: Vec<u32> = (0..c.instruments.len())
        .map(|i| capture_start + c.listen_after_start[i])
        .collect();

    // The previous process: live up to the crash. What it sealed is what
    // the database holds for the restart to overwrite.
    let mut stored = Written::default();
    fold_live(
        all.iter().filter(|p| p.received_ms <= crash_ms),
        &mut stored,
    );

    // The restarted process: replay the WAL (some frames unreadable), hand
    // over, then fold what each socket receives once it listens.
    let mut restart = Written::default();
    let mut agg = aggregator();
    agg.set_replay_mode(true);
    let mut gap = true;
    for (k, p) in all.iter().filter(|p| p.received_ms <= crash_ms).enumerate() {
        if c.unreadable[k % c.unreadable.len()] < UNREADABLE_PERCENT {
            gap = true;
            continue;
        }
        if std::mem::replace(&mut gap, false) {
            agg.mark_replay_gap();
        }
        agg.consume_tick(
            Feed::Dhan,
            &tick(p, p.received_ms),
            None,
            |_, sid, _, tf, bar| restart.put(sid, tf, bar),
        );
    }
    agg.set_live_capture_start(capture_start);
    agg.finish_replay(gap, |_, sid, _, tf, bar| restart.put(sid, tf, bar));
    sweep(&mut agg, capture_start, &mut restart);

    // `(receipt ms, order, packet)`: a subscribe re-send (order 0) arrives
    // at the listen instant, before anything else received then.
    let mut live: Vec<(u64, u8, Packet)> = all
        .iter()
        .filter(|p| {
            p.received_ms > crash_ms && p.received_ms >= u64::from(listen[p.instrument]) * 1000
        })
        .map(|p| (p.received_ms, 1, *p))
        .collect();
    for (i, &resend) in c
        .resend_on_subscribe
        .iter()
        .enumerate()
        .take(c.instruments.len())
    {
        let listen_ms = u64::from(listen[i]) * 1000;
        if let Some(p) = all
            .iter()
            .rfind(|p| resend && p.instrument == i && p.received_ms < listen_ms)
        {
            live.push((listen_ms, 0, *p));
        }
    }
    live.sort_by_key(|(received, order, p)| (*received, *order, p.instrument, p.cumulative));
    // Per instrument: the first live packet's `(trade second, receipt ms)`.
    let mut first_live: HashMap<usize, (u32, u64)> = HashMap::new();
    for (received, _, p) in &live {
        first_live
            .entry(p.instrument)
            .or_insert((p.trade, *received));
        agg.consume_tick(
            Feed::Dhan,
            &tick(p, *received),
            None,
            |_, sid, _, tf, bar| restart.put(sid, tf, bar),
        );
        sweep(&mut agg, (*received / 1000) as u32, &mut restart);
    }
    agg.catch_up_seal_all(end_cutoff, |_, sid, _, tf, bar| restart.put(sid, tf, bar));
    // The first live receipt of ANY instrument confirms the capture start.
    let confirmed_start = live.first().map_or(capture_start, |(received, _, _)| {
        capture_start.max((*received / 1000) as u32)
    });

    for (i, &listen_at) in listen.iter().enumerate() {
        let sid = security_id(i);
        for tf in TfIndex::ALL {
            let mut starts: Vec<u32> = truth
                .last
                .keys()
                .chain(restart.last.keys())
                .filter(|k| k.0 == sid && k.1 == tf.as_ordinal())
                .map(|k| k.2)
                .collect();
            starts.sort_unstable();
            starts.dedup();
            for start in starts {
                let key = (sid, tf.as_ordinal(), start);
                let end = start + tf.seconds_per_bucket();
                let (t, r) = (truth.last.get(&key), restart.last.get(&key));
                // The latest instant the fold can treat as "before this
                // instrument was captured": the confirmed capture start, or
                // the instrument's own first live receipt. An instrument that
                // never received a live packet never confirmed at all.
                let captured_from = first_live
                    .get(&i)
                    .map(|&(_, received)| confirmed_start.max((received / 1000) as u32));
                let context = format!(
                    "sid {sid} {tf:?} bucket +{}..+{}, crash +{}, capture start +{}, \
                     listen +{}, first live {:?}: restart {} | truth {}",
                    start as i64 - START as i64,
                    end as i64 - START as i64,
                    crash_secs as i64 - START as i64,
                    capture_start - START,
                    listen_at - START,
                    first_live.get(&i),
                    describe(r),
                    describe(t)
                );
                // 1. Never more volume than the truth, at any emission.
                if let Some(&max) = restart.max_volume.get(&key) {
                    prop_assert!(
                        max <= t.map_or(0, |t| t.volume),
                        "over-count ({max}): {context}"
                    );
                }
                let Some(r) = r else {
                    // 4. Missing after the crash: only a bucket that started
                    // before the instrument was known to be captured.
                    if t.is_some() && u64::from(end) * 1000 > crash_ms {
                        prop_assert!(
                            captured_from.is_none_or(|from| start <= from),
                            "withheld without cause: {context}"
                        );
                    }
                    continue;
                };
                if stored.last.contains_key(&key) {
                    // 2. Stored by the previous process: identical or absent.
                    prop_assert!(
                        t.is_some_and(|t| same(r, t, true)),
                        "a stored bucket was rewritten differently: {context}"
                    );
                } else {
                    // 3. Stored nowhere else: exact, and a classified net
                    // volume is the truth's (an unclassified one says the
                    // direction is unknown, which is honest, not wrong).
                    prop_assert!(
                        t.is_some_and(
                            |t| same(r, t, false) && (!r.net_volume_classified || same(r, t, true))
                        ),
                        "a wrong bar was written: {context}"
                    );
                }
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn a_restart_never_writes_a_wrong_bar(c in case()) {
        check(&c)?;
    }
}
