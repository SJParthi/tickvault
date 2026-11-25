//! Property test: `LiveCandleBook` against a brute-force reference.
//!
//! Random streams of ticks (exchange times jittered, arrivals shuffled,
//! packets duplicated, a few stamped an hour in the future) mixed with random
//! clock readings (some stepping backwards), on several IST days (some whose
//! midnight is NOT a multiple of 7 s) and two windows (09:15 and the 40 s
//! around midnight). The reference keeps every event and recomputes everything
//! by scanning, with buckets written straight from the spec (a `(day, index)`
//! pair, the last bucket of a day cut at midnight): which ticks each
//! (instrument, frame) bar accepts, the OHLC, volume and count of each bucket,
//! and the first event that closes it (a clock reading at or past the
//! boundary, or an accepted tick of a later bucket). The book must emit
//! exactly the reference's closed bars, field for field, and no closed bar may
//! hold a tick stamped at or after its end.

use proptest::prelude::*;
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::live_candles::{
    CloseBy, ClosedBar, ClosedBars, FUTURE_TICK_SKEW_SECS, LiveCandleBook, TickVerdict,
};

const DAY_SECS: i64 = 86_400;
/// IST-midnight epoch days (IST epoch seconds / 86,400). 20,594 × 86,400 is a
/// multiple of 7; the others are not (86,400 mod 7 = 6), so a 7 s grid that
/// followed the epoch instead of midnight would disagree with the reference.
const DAYS: [u32; 6] = [20_594, 20_595, 20_596, 20_597, 20_770, 20_771];
/// Window starts, seconds of the IST day: 09:15:00, and 23:59:40 so the span
/// crosses midnight.
const WINDOW_SOD: [u32; 2] = [33_300, 86_380];
const NS: i64 = 1_000_000_000;
/// 7 s divides neither a minute nor a day: exercises the midnight cut.
const FRAMES: [u32; 4] = [1, 3, 5, 7];
const KEYS: u64 = 3;
/// Seconds after the window start the generator spans.
const SPAN_SECS: u32 = 40;
/// How far ahead a future-stamped tick claims to be.
const FUTURE_JUMP_SECS: u32 = 3_600;

#[derive(Debug, Clone, Copy)]
enum Ev {
    Tick {
        key: u64,
        secs: u32,
        receipt_ns: i64,
        px: f64,
        cum: u64,
    },
    Clock(i64),
}

fn key(id: u64) -> InstrumentKey {
    InstrumentKey {
        security_id: id,
        exchange_segment: 2,
    }
}

/// The spec's bucket: `(day, index within the day)`.
fn spec_bucket(secs: u32, size: u32) -> (i64, i64) {
    let s = i64::from(secs);
    (
        s.div_euclid(DAY_SECS),
        s.rem_euclid(DAY_SECS) / i64::from(size),
    )
}

/// The spec's `(open, end)` seconds: end cut at the next midnight.
fn spec_bounds(b: (i64, i64), size: u32) -> (i64, i64) {
    let open = b.0 * DAY_SECS + b.1 * i64::from(size);
    (open, (open + i64::from(size)).min((b.0 + 1) * DAY_SECS))
}

fn ev_strategy(base: u32) -> impl Strategy<Value = Ev> {
    prop_oneof![
        4 => (1..=KEYS, 0..SPAN_SECS, -500_000_000i64..2_000_000_000, 1u32..2_000, 0u64..5, 0u8..40)
            .prop_map(move |(k, s, jit, px, extra, fut)| {
                let real = base + s;
                Ev::Tick {
                    key: k,
                    // About one tick in forty claims an hour ahead.
                    secs: if fut == 0 { real + FUTURE_JUMP_SECS } else { real },
                    receipt_ns: i64::from(real) * NS + jit,
                    // Some zero prices: refused by both sides.
                    px: if px < 20 { 0.0 } else { f64::from(px) / 4.0 },
                    // Cumulative grows with time, so a shuffled arrival goes down.
                    cum: u64::from(s) * 100 + k * 7 + extra,
                }
            }),
        1 => (0..SPAN_SECS + 8, 0i64..NS).prop_map(move |(s, frac)| {
            Ev::Clock(i64::from(base + s) * NS + frac)
        }),
    ]
}

fn stream_strategy() -> impl Strategy<Value = Vec<Ev>> {
    (0..DAYS.len(), 0..WINDOW_SOD.len())
        .prop_flat_map(|(d, w)| {
            let base = DAYS[d] * 86_400 + WINDOW_SOD[w];
            proptest::collection::vec(ev_strategy(base), 1..120)
        })
        .prop_flat_map(|mut v| {
            // Mostly time-ordered with a shuffled tail and duplicated packets.
            v.sort_by_key(|e| match *e {
                Ev::Tick { receipt_ns, .. } => receipt_ns,
                Ev::Clock(now) => now,
            });
            let n = v.len();
            (
                Just(v),
                proptest::collection::vec((0..n, 0..n, any::<bool>()), 0..20),
            )
                .prop_map(|(mut v, ops)| {
                    for (a, b, dup) in ops {
                        if dup {
                            let e = v[a];
                            v.insert(b, e);
                        } else {
                            v.swap(a, b);
                        }
                    }
                    v
                })
        })
}

/// One accepted (tick, frame) membership in the reference.
#[derive(Clone, Copy)]
struct Member {
    idx: usize,
    key: u64,
    frame: usize,
    bucket: (i64, i64),
    secs: u32,
    receipt_ns: i64,
    px: f64,
    delta: u64,
    /// The cumulative fell below the instrument's high mark.
    volume_down: bool,
}

/// What the reference expects.
struct Want {
    bars: Vec<ClosedBar>,
    late: Vec<u64>,
    refused_future: u64,
}

/// Brute-force reference.
fn reference(events: &[Ev]) -> Want {
    let mut clocks: Vec<(usize, i64)> = Vec::new();
    let mut clock_max = i64::MIN;
    let mut members: Vec<Member> = Vec::new();
    let mut late = vec![0u64; FRAMES.len()];
    let mut refused_future = 0u64;
    let mut baseline: Vec<Option<u64>> = vec![None; KEYS as usize + 1];
    for (idx, ev) in events.iter().enumerate() {
        match *ev {
            Ev::Clock(now) => {
                if now > clock_max {
                    clock_max = now;
                    clocks.push((idx, now));
                }
            }
            Ev::Tick {
                key,
                secs,
                receipt_ns,
                px,
                cum,
            } => {
                if px <= 0.0 {
                    continue;
                }
                let reference_secs = clock_max.div_euclid(NS).max(receipt_ns.div_euclid(NS));
                let has_reference = clock_max != i64::MIN || receipt_ns > 0;
                if has_reference && i64::from(secs) > reference_secs + FUTURE_TICK_SKEW_SECS {
                    refused_future += 1;
                    continue;
                }
                let (delta, volume_down) = match baseline[key as usize] {
                    None => {
                        baseline[key as usize] = Some(cum);
                        (0, false)
                    }
                    Some(p) if cum < p => (0, true),
                    Some(p) => {
                        baseline[key as usize] = Some(cum);
                        (cum - p, false)
                    }
                };
                for (fi, &size) in FRAMES.iter().enumerate() {
                    let bucket = spec_bucket(secs, size);
                    let end = spec_bounds(bucket, size).1 * NS;
                    let passed_by_clock = clocks.iter().any(|&(_, now)| now >= end);
                    let newer_bucket_seen = members
                        .iter()
                        .any(|m| m.key == key && m.frame == fi && m.bucket > bucket);
                    if passed_by_clock || newer_bucket_seen {
                        late[fi] += 1;
                        continue;
                    }
                    members.push(Member {
                        idx,
                        key,
                        frame: fi,
                        bucket,
                        secs,
                        receipt_ns,
                        px,
                        delta,
                        volume_down,
                    });
                }
            }
        }
    }
    let mut bars = Vec::new();
    let mut groups: Vec<(u64, usize, (i64, i64))> =
        members.iter().map(|m| (m.key, m.frame, m.bucket)).collect();
    groups.sort_unstable();
    groups.dedup();
    for (k, fi, b) in groups {
        let size = FRAMES[fi];
        let ms: Vec<&Member> = members
            .iter()
            .filter(|m| m.key == k && m.frame == fi && m.bucket == b)
            .collect();
        let last = ms[ms.len() - 1];
        // The close rule (review finding 3, 2026-11-12): an older packet (its
        // cumulative fell, or its exchange second is below the close's) may
        // widen high/low and add to the count and volume, but never becomes
        // the close.
        let mut close_m = ms[0];
        for m in &ms[1..] {
            if !m.volume_down && m.secs >= close_m.secs {
                close_m = m;
            }
        }
        let (open_secs, end_secs) = spec_bounds(b, size);
        let end = end_secs * NS;
        // The first closing event after the bar's last tick.
        let clock_close = clocks
            .iter()
            .find(|&&(i, now)| i > last.idx && now >= end)
            .map(|&(i, now)| (i, CloseBy::Clock, now));
        let tick_close = members
            .iter()
            .find(|m| m.idx > last.idx && m.key == k && m.frame == fi && m.bucket > b)
            .map(|m| (m.idx, CloseBy::Tick, m.receipt_ns));
        let close = match (clock_close, tick_close) {
            (Some(c), Some(t)) => Some(if c.0 < t.0 { c } else { t }),
            (c, t) => c.or(t),
        };
        let Some((_, close_by, trigger_ns)) = close else {
            continue;
        };
        bars.push(ClosedBar {
            key: key(k),
            frame_index: fi as u8,
            frame_secs: size,
            bucket_open_secs: open_secs,
            bucket_end_secs: end_secs,
            open: ms[0].px,
            high: ms.iter().map(|m| m.px).fold(f64::MIN, f64::max),
            low: ms.iter().map(|m| m.px).fold(f64::MAX, f64::min),
            close: close_m.px,
            volume: ms.iter().map(|m| m.delta).sum(),
            tick_count: ms.len() as u32,
            last_tick_exchange_secs: i64::from(close_m.secs),
            last_tick_receipt_ns: close_m.receipt_ns,
            close_by,
            trigger_ns,
            close_delay_ns: trigger_ns - end,
        });
    }
    Want {
        bars,
        late,
        refused_future,
    }
}

fn sort_key(b: &ClosedBar) -> (u64, u8, i64) {
    (b.key.security_id, b.frame_index, b.bucket_open_secs)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn book_matches_brute_force_reference(events in stream_strategy()) {
        let keys: Vec<InstrumentKey> = (1..=KEYS).map(key).collect();
        let mut book = LiveCandleBook::configure(&keys, &FRAMES).expect("valid");
        let mut out = ClosedBars::with_capacity(4_096);
        for ev in &events {
            match *ev {
                Ev::Tick { key: k, secs, receipt_ns, px, cum } => {
                    let v = book.on_tick(key(k), secs, receipt_ns, px, cum, &mut out);
                    if px <= 0.0 {
                        prop_assert_eq!(v, TickVerdict::RefusedPrice);
                    }
                }
                Ev::Clock(now) => book.on_clock(now, &mut out),
            }
        }
        prop_assert_eq!(out.overflowed(), 0);
        let want = reference(&events);
        let mut got: Vec<ClosedBar> = out.bars().to_vec();
        for bar in &got {
            prop_assert!(bar.last_tick_exchange_secs < bar.bucket_end_secs);
            prop_assert!(bar.last_tick_exchange_secs >= bar.bucket_open_secs);
            prop_assert!(bar.low <= bar.open && bar.open <= bar.high);
            prop_assert!(bar.low <= bar.close && bar.close <= bar.high);
            // Aligned to IST midnight: the open is a whole number of frames
            // after the day's midnight, and no bar crosses midnight.
            let sod = bar.bucket_open_secs.rem_euclid(DAY_SECS);
            prop_assert_eq!(sod % i64::from(bar.frame_secs), 0);
            prop_assert!(
                bar.bucket_end_secs.div_euclid(DAY_SECS) == bar.bucket_open_secs.div_euclid(DAY_SECS)
                    || bar.bucket_end_secs.rem_euclid(DAY_SECS) == 0
            );
        }
        let mut want_bars = want.bars;
        got.sort_by_key(sort_key);
        want_bars.sort_by_key(sort_key);
        prop_assert_eq!(&got, &want_bars);
        prop_assert_eq!(&book.counters().late, &want.late);
        prop_assert_eq!(book.counters().refused_future, want.refused_future);
        let by_clock = want_bars.iter().filter(|b| b.close_by == CloseBy::Clock).count() as u64;
        prop_assert_eq!(book.counters().closed_by_clock, by_clock);
        prop_assert_eq!(
            book.counters().closed_by_tick,
            want_bars.len() as u64 - by_clock
        );
    }
}
