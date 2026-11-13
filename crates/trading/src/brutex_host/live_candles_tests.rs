use super::*;

/// An exact multiple of 86,400 (IST midnight in IST epoch seconds).
const DAY: u32 = 1_779_321_600;
/// 09:15:00 IST.
const OPEN: u32 = DAY + 33_300;
/// 15:29:59 IST.
const LAST_SEC: u32 = DAY + 55_799;
/// Receipt latency used by most tests: 200 ms after the exchange second.
const LAT_NS: i64 = 200_000_000;

fn key(id: u64) -> InstrumentKey {
    InstrumentKey {
        security_id: id,
        exchange_segment: 2,
    }
}

fn ns(secs: u32) -> i64 {
    i64::from(secs) * NANOS_PER_SEC
}

fn book(n: u64, frames: &[u32]) -> LiveCandleBook {
    let keys: Vec<InstrumentKey> = (1..=n).map(key).collect();
    LiveCandleBook::configure(&keys, frames).expect("valid config")
}

/// A tick on key 1 with the standard latency.
fn tick(b: &mut LiveCandleBook, secs: u32, px: f64, cum: u64, out: &mut ClosedBars) -> TickVerdict {
    b.on_tick(key(1), secs, ns(secs) + LAT_NS, px, cum, out)
}

#[test]
fn test_configure_refuses_bad_input_and_reports_frame_count_open_bars() {
    let k = [key(1), key(2)];
    assert_eq!(
        LiveCandleBook::configure(&[], &[5]).unwrap_err(),
        ConfigError::EmptyKeys
    );
    assert_eq!(
        LiveCandleBook::configure(&k, &[]).unwrap_err(),
        ConfigError::EmptyFrames
    );
    assert_eq!(
        LiveCandleBook::configure(&k, &[5, 0]).unwrap_err(),
        ConfigError::ZeroFrame
    );
    assert_eq!(
        LiveCandleBook::configure(&[key(1), key(1)], &[5]).unwrap_err(),
        ConfigError::DuplicateKey(key(1))
    );
    assert_eq!(
        LiveCandleBook::configure(&k, &[5, 60, 5]).unwrap_err(),
        ConfigError::DuplicateFrame(5)
    );
    let many: Vec<u32> = (1..=257).collect();
    assert_eq!(
        LiveCandleBook::configure(&k, &many).unwrap_err(),
        ConfigError::TooManyFrames(257)
    );
    let ok = LiveCandleBook::configure(&k, &[1, 3, 5, 60]).expect("ok");
    assert_eq!(ok.frame_count(), 4);
    assert_eq!(ok.counters().late.len(), 4);
    assert_eq!(ok.open_bars(), 0);
}

#[test]
fn tick_at_04_stays_in_bar_tick_at_05_closes_it() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(8);
    assert_eq!(
        tick(&mut b, OPEN, 100.0, 1_000, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    tick(&mut b, OPEN + 4, 101.0, 1_010, &mut out);
    assert!(out.bars().is_empty(), "nothing closes inside the bucket");
    tick(&mut b, OPEN + 5, 99.0, 1_030, &mut out);
    assert_eq!(out.bars().len(), 1);
    let bar = out.bars()[0];
    assert_eq!(bar.bucket_open_secs, i64::from(OPEN));
    assert_eq!(bar.bucket_end_secs, i64::from(OPEN + 5));
    assert_eq!(
        (bar.open, bar.high, bar.low, bar.close),
        (100.0, 101.0, 100.0, 101.0)
    );
    assert_eq!(bar.volume, 10);
    assert_eq!(bar.tick_count, 2);
    assert_eq!(bar.last_tick_exchange_secs, i64::from(OPEN + 4));
    assert_eq!(bar.last_tick_receipt_ns, ns(OPEN + 4) + LAT_NS);
    assert_eq!(bar.close_by, CloseBy::Tick);
    assert_eq!(bar.trigger_ns, ns(OPEN + 5) + LAT_NS);
    assert_eq!(bar.close_delay_ns, LAT_NS);
    assert!(bar.last_tick_exchange_secs < bar.bucket_end_secs);
    // The 09:15:05 tick opened the next bar.
    assert_eq!(b.open_bars(), 1);
    assert_eq!(b.counters().closed_by_tick, 1);
    assert_eq!(b.counters().accepted, 3);
}

#[test]
fn open_edge_0915_and_last_second_1529() {
    let mut b = book(1, &[5, 60]);
    let mut out = ClosedBars::with_capacity(8);
    // 09:14:58 is a pre-open second: bucket 09:14:55 (5 s) / 09:14:00 (60 s).
    tick(&mut b, OPEN - 2, 50.0, 10, &mut out);
    // The clock at exactly 09:15:00.000 closes both, delay 0.
    b.on_clock(ns(OPEN), &mut out);
    assert_eq!(out.bars().len(), 2);
    for bar in out.bars() {
        assert_eq!(bar.bucket_end_secs, i64::from(OPEN));
        assert_eq!(bar.close_by, CloseBy::Clock);
        assert_eq!(bar.close_delay_ns, 0);
    }
    out.clear();
    // A tick stamped exactly 09:15:00 opens the 09:15:00 bucket.
    tick(&mut b, OPEN, 51.0, 20, &mut out);
    b.on_clock(ns(OPEN + 5), &mut out);
    assert_eq!(out.bars().len(), 1);
    assert_eq!(out.bars()[0].bucket_open_secs, i64::from(OPEN));
    out.clear();
    // 15:29:59 belongs to the 15:29:00 minute; the clock at 15:30:00.000
    // closes it with delay 0.
    tick(&mut b, LAST_SEC, 52.0, 30, &mut out);
    b.on_clock(ns(LAST_SEC + 1), &mut out);
    let minute = out
        .bars()
        .iter()
        .find(|x| x.frame_secs == 60 && x.bucket_open_secs == i64::from(LAST_SEC - 59))
        .copied()
        .expect("15:29 minute closed");
    assert_eq!(minute.bucket_end_secs, i64::from(LAST_SEC + 1));
    assert_eq!(minute.close_delay_ns, 0);
    assert_eq!(minute.close, 52.0);
}

#[test]
fn test_on_clock_closes_at_the_boundary_not_a_nanosecond_before() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN + 1, 10.0, 1, &mut out);
    b.on_clock(ns(OPEN + 5) - 1, &mut out);
    assert!(out.bars().is_empty());
    assert_eq!(b.open_bars(), 1);
    b.on_clock(ns(OPEN + 5) + 7, &mut out);
    assert_eq!(out.bars().len(), 1);
    let bar = out.bars()[0];
    assert_eq!(bar.close_by, CloseBy::Clock);
    assert_eq!(bar.trigger_ns, ns(OPEN + 5) + 7);
    assert_eq!(bar.close_delay_ns, 7);
    assert_eq!(b.open_bars(), 0);
    assert_eq!(b.counters().closed_by_clock, 1);
}

#[test]
fn first_event_wins_and_is_recorded() {
    // Clock first: the next tick opens a new bar and closes nothing.
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN, 10.0, 1, &mut out);
    b.on_clock(ns(OPEN + 5), &mut out);
    tick(&mut b, OPEN + 5, 11.0, 2, &mut out);
    assert_eq!(out.bars().len(), 1);
    assert_eq!(out.bars()[0].close_by, CloseBy::Clock);
    assert_eq!(b.counters().closed_by_tick, 0);
    assert_eq!(b.open_bars(), 1);

    // Tick first: the clock later closes only the new bar, at its own end.
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN, 10.0, 1, &mut out);
    b.on_tick(key(1), OPEN + 5, ns(OPEN + 5) - 3, 11.0, 2, &mut out);
    b.on_clock(ns(OPEN + 5), &mut out);
    assert_eq!(out.bars().len(), 1, "the clock finds nothing due");
    assert_eq!(out.bars()[0].close_by, CloseBy::Tick);
    b.on_clock(ns(OPEN + 10), &mut out);
    assert_eq!(out.bars().len(), 2);
    assert_eq!(out.bars()[1].bucket_open_secs, i64::from(OPEN + 5));
    assert_eq!(out.bars()[1].close_by, CloseBy::Clock);
}

#[test]
fn tick_after_clock_close_is_late_and_counted() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN, 10.0, 100, &mut out);
    b.on_clock(ns(OPEN + 5), &mut out);
    let before = out.bars()[0];
    assert_eq!(
        tick(&mut b, OPEN + 4, 99.0, 110, &mut out),
        TickVerdict::Accepted { late_frames: 1 }
    );
    assert_eq!(b.counters().late[0], 1);
    assert_eq!(out.bars().len(), 1);
    assert_eq!(out.bars()[0], before, "the closed bar is untouched");
    assert_eq!(b.open_bars(), 0);
    // The late tick still moved the volume baseline: the next bar sees 5.
    tick(&mut b, OPEN + 6, 11.0, 115, &mut out);
    b.on_clock(ns(OPEN + 10), &mut out);
    assert_eq!(out.bars()[1].volume, 5);
}

#[test]
fn out_of_order_ticks() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    // Inside one bucket, order does not matter for membership; the close is
    // the newest exchange second, never an older packet that arrives later.
    tick(&mut b, OPEN + 3, 10.0, 100, &mut out);
    tick(&mut b, OPEN + 1, 12.0, 105, &mut out);
    // An older bucket than the open one is late.
    tick(&mut b, OPEN + 5, 13.0, 106, &mut out);
    assert_eq!(
        tick(&mut b, OPEN + 2, 9.0, 104, &mut out),
        TickVerdict::Accepted { late_frames: 1 }
    );
    assert_eq!(out.bars().len(), 1);
    let bar = out.bars()[0];
    assert_eq!(
        (bar.open, bar.high, bar.low, bar.close),
        (10.0, 12.0, 10.0, 10.0)
    );
    assert_eq!(bar.last_tick_exchange_secs, i64::from(OPEN + 3));
    assert_eq!(bar.volume, 5, "an older second still adds its volume rise");
    assert_eq!(bar.tick_count, 2);
    assert_eq!(b.counters().late[0], 1);
    assert_eq!(b.counters().volume_down, 1, "104 < 106");
}

#[test]
fn duplicate_ticks_count_but_add_no_volume() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN, 10.0, 100, &mut out);
    tick(&mut b, OPEN + 1, 10.5, 120, &mut out);
    tick(&mut b, OPEN + 1, 10.5, 120, &mut out);
    b.on_clock(ns(OPEN + 5), &mut out);
    let bar = out.bars()[0];
    assert_eq!(bar.tick_count, 3);
    assert_eq!(bar.volume, 20);
}

#[test]
fn feed_gap_emits_no_empty_bars_and_counts_them() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN, 10.0, 1, &mut out);
    tick(&mut b, OPEN + 17, 11.0, 2, &mut out);
    assert_eq!(out.bars().len(), 1);
    assert_eq!(out.bars()[0].bucket_open_secs, i64::from(OPEN));
    assert_eq!(b.counters().skipped_empty, 2, "09:15:05 and 09:15:10");
    b.on_clock(ns(OPEN + 20), &mut out);
    assert_eq!(out.bars().len(), 2);
    assert_eq!(out.bars()[1].bucket_open_secs, i64::from(OPEN + 15));
}

#[test]
fn clock_skew_behind_and_ahead() {
    // Behind: the closing tick's receipt is stamped before the boundary.
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN, 10.0, 1, &mut out);
    b.on_tick(
        key(1),
        OPEN + 5,
        ns(OPEN + 5) - 40_000_000,
        11.0,
        2,
        &mut out,
    );
    assert_eq!(out.bars()[0].close_delay_ns, -40_000_000);

    // Ahead: the clock passes 09:15:05 while the exchange is still in the
    // 09:15:00 bucket. The bar closes early; a later tick of that bucket is
    // late, and so is a first tick of a bucket the clock has already passed.
    let mut b = LiveCandleBook::configure(&[key(1), key(2)], &[5]).expect("ok");
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN + 1, 10.0, 1, &mut out);
    b.on_clock(ns(OPEN + 5) + 300_000_000, &mut out);
    assert_eq!(out.bars().len(), 1);
    assert_eq!(
        tick(&mut b, OPEN + 4, 10.2, 3, &mut out),
        TickVerdict::Accepted { late_frames: 1 }
    );
    assert_eq!(
        b.on_tick(key(2), OPEN + 3, ns(OPEN + 3), 20.0, 1, &mut out),
        TickVerdict::Accepted { late_frames: 1 }
    );
    assert_eq!(b.counters().late[0], 2);
    assert_eq!(b.open_bars(), 0);
    // A tick of the bucket the clock is inside opens a bar.
    tick(&mut b, OPEN + 5, 10.3, 4, &mut out);
    assert_eq!(b.open_bars(), 1);
}

#[test]
fn clock_step_backwards_does_nothing() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    b.on_clock(ns(OPEN + 7), &mut out);
    tick(&mut b, OPEN + 8, 10.0, 1, &mut out);
    b.on_clock(ns(OPEN + 2), &mut out);
    b.on_clock(ns(OPEN + 7), &mut out);
    assert!(out.bars().is_empty());
    assert_eq!(b.open_bars(), 1);
    // The backwards step did not reopen 09:15:00..09:15:05 for ticks.
    assert_eq!(
        tick(&mut b, OPEN + 3, 10.0, 2, &mut out),
        TickVerdict::Accepted { late_frames: 1 }
    );
    b.on_clock(ns(OPEN + 10), &mut out);
    assert_eq!(out.bars().len(), 1);
}

#[test]
fn several_frames_at_once() {
    let frames = [1, 3, 5, 60];
    let mut b = book(1, &frames);
    let mut out = ClosedBars::with_capacity(256);
    for s in 0..60 {
        tick(
            &mut b,
            OPEN + s,
            100.0 + f64::from(s),
            1_000 + u64::from(s) * 10,
            &mut out,
        );
    }
    b.on_clock(ns(OPEN + 60), &mut out);
    for (fi, &secs) in frames.iter().enumerate() {
        let bars: Vec<&ClosedBar> = out.bars().iter().filter(|x| x.frame_secs == secs).collect();
        assert_eq!(bars.len(), (60 / secs) as usize, "frame {secs}");
        for bar in &bars {
            assert_eq!(usize::from(bar.frame_index), fi);
            assert_eq!(bar.to_bar_input().timeframe, bar.frame_index);
            assert_eq!(bar.tick_count, secs);
            assert!(bar.last_tick_exchange_secs < bar.bucket_end_secs);
        }
        let last = bars
            .iter()
            .max_by_key(|x| x.bucket_open_secs)
            .expect("bars");
        assert_eq!(last.close_by, CloseBy::Clock);
    }
    let minute = out
        .bars()
        .iter()
        .find(|x| x.frame_secs == 60)
        .expect("minute");
    assert_eq!((minute.open, minute.close), (100.0, 159.0));
    assert_eq!(minute.volume, 590, "first tick is the baseline");
    assert_eq!(b.counters().closed_by_tick, 59 + 19 + 11);
    assert_eq!(b.counters().closed_by_clock, 4);
}

#[test]
fn one_second_frame() {
    let mut b = book(1, &[1]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN, 10.0, 1, &mut out);
    tick(&mut b, OPEN, 10.5, 2, &mut out);
    tick(&mut b, OPEN + 1, 11.0, 3, &mut out);
    let bar = out.bars()[0];
    assert_eq!(bar.bucket_end_secs, i64::from(OPEN + 1));
    assert_eq!(bar.tick_count, 2);
    assert_eq!(bar.close_delay_ns, LAT_NS);
}

#[test]
fn many_instruments() {
    const N: u64 = 500;
    let mut b = book(N, &[5]);
    let mut out = ClosedBars::with_capacity(2 * N as usize);
    for s in 0..10 {
        for id in 1..=N {
            let v = b.on_tick(
                key(id),
                OPEN + s,
                ns(OPEN + s),
                100.0 + id as f64,
                u64::from(s) + id,
                &mut out,
            );
            assert_eq!(v, TickVerdict::Accepted { late_frames: 0 });
        }
        if s == 4 {
            assert_eq!(out.bars().len(), 0);
        }
    }
    assert_eq!(out.bars().len(), N as usize, "09:15:00 bars closed by tick");
    b.on_clock(ns(OPEN + 10), &mut out);
    assert_eq!(out.bars().len(), 2 * N as usize);
    assert_eq!(b.open_bars(), 0);
    for bar in out.bars() {
        assert_eq!(bar.tick_count, 5);
        assert_eq!(bar.open, 100.0 + bar.key.security_id as f64);
    }
}

#[test]
fn unknown_key_and_bad_prices_are_refused() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    assert_eq!(
        b.on_tick(key(99), OPEN, ns(OPEN), 10.0, 1, &mut out),
        TickVerdict::RefusedUnknownKey
    );
    for px in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.0, -5.0] {
        assert_eq!(
            tick(&mut b, OPEN, px, 1, &mut out),
            TickVerdict::RefusedPrice
        );
    }
    let c = b.counters();
    assert_eq!((c.refused_unknown, c.refused_price, c.accepted), (1, 6, 0));
    assert_eq!(c.baseline, 0, "a refused tick sets no baseline");
    assert_eq!(b.open_bars(), 0);
}

#[test]
fn volume_down_keeps_the_high_baseline() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN, 10.0, 100, &mut out);
    tick(&mut b, OPEN + 1, 10.0, 150, &mut out);
    tick(&mut b, OPEN + 2, 10.0, 120, &mut out);
    tick(&mut b, OPEN + 3, 10.0, 170, &mut out);
    b.on_clock(ns(OPEN + 5), &mut out);
    assert_eq!(out.bars()[0].volume, 70);
    assert_eq!(b.counters().baseline, 1);
    assert_eq!(b.counters().volume_down, 1);
}

#[test]
fn volume_near_u64_max() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN, 10.0, u64::MAX - 10, &mut out);
    tick(&mut b, OPEN + 1, 10.0, u64::MAX, &mut out);
    tick(&mut b, OPEN + 2, 10.0, 0, &mut out);
    b.on_clock(ns(OPEN + 5), &mut out);
    assert_eq!(out.bars()[0].volume, 10);
    assert_eq!(b.counters().volume_down, 1);
}

#[test]
fn closed_bars_overflow_keeps_the_first() {
    let mut b = book(3, &[5]);
    let mut out = ClosedBars::with_capacity(2);
    for id in 1..=3 {
        b.on_tick(key(id), OPEN, ns(OPEN), id as f64, 1, &mut out);
    }
    b.on_clock(ns(OPEN + 5), &mut out);
    assert_eq!(out.bars().len(), 2);
    assert_eq!(out.overflowed(), 1);
    assert_eq!(b.counters().overflowed, 1);
    assert_eq!(b.counters().closed_by_clock, 3);
    out.clear();
    assert!(out.bars().is_empty());
    assert_eq!(out.overflowed(), 0);
    assert!(out.push(ClosedBar {
        key: key(1),
        frame_index: 0,
        frame_secs: 5,
        bucket_open_secs: 0,
        bucket_end_secs: 5,
        open: 1.0,
        high: 1.0,
        low: 1.0,
        close: 1.0,
        volume: 0,
        tick_count: 1,
        last_tick_exchange_secs: 0,
        last_tick_receipt_ns: 0,
        close_by: CloseBy::Clock,
        trigger_ns: 0,
        close_delay_ns: 0,
    }));
}

#[test]
fn test_on_tick_input_and_to_bar_input() {
    let mut b = LiveCandleBook::configure(&[key(7)], &[60, 5]).expect("ok");
    let mut out = ClosedBars::with_capacity(4);
    let t = TickInput {
        key: key(7),
        exchange_time_ist_secs: OPEN + 2,
        receipt_ns: ns(OPEN + 2),
        ltp: 42.5,
        cumulative_volume: 9,
    };
    assert_eq!(
        b.on_tick_input(&t, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    b.on_clock(ns(OPEN + 5), &mut out);
    let bi = out.bars()[0].to_bar_input();
    assert_eq!(bi.key, key(7));
    assert_eq!(bi.timeframe, 1, "5 s is index 1");
    assert_eq!(bi.open_ist_secs, i64::from(OPEN));
    assert_eq!(
        (bi.open, bi.high, bi.low, bi.close, bi.volume),
        (42.5, 42.5, 42.5, 42.5, 0)
    );
}

#[test]
fn reopen_after_clock_close_reuses_the_open_list() {
    // Swap-remove bookkeeping: close the middle of three open bars by tick,
    // then let the clock close the rest; every bar is emitted exactly once.
    let mut b = book(3, &[5]);
    let mut out = ClosedBars::with_capacity(16);
    for id in 1..=3 {
        b.on_tick(key(id), OPEN, ns(OPEN), 1.0, 1, &mut out);
    }
    b.on_tick(key(2), OPEN + 5, ns(OPEN + 5), 2.0, 2, &mut out);
    assert_eq!(out.bars().len(), 1);
    assert_eq!(b.open_bars(), 3);
    b.on_clock(ns(OPEN + 5), &mut out);
    assert_eq!(out.bars().len(), 3);
    b.on_clock(ns(OPEN + 10), &mut out);
    assert_eq!(out.bars().len(), 4);
    assert_eq!(b.open_bars(), 0);
    let mut seen: Vec<(u64, i64)> = out
        .bars()
        .iter()
        .map(|x| (x.key.security_id, x.bucket_open_secs))
        .collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 4);
}

/// IST midnight of 2026-11-13 in IST epoch seconds: a multiple of 86,400 but
/// NOT of 7 (20,770 days; 86,400 mod 7 = 6).
const DAY_1113: u32 = 1_794_528_000;

#[test]
fn test_future_stamped_tick_is_refused_and_freezes_nothing() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(8);
    b.on_clock(ns(OPEN + 1), &mut out);
    assert_eq!(
        tick(&mut b, OPEN, 100.0, 100, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    // A stamp an hour ahead, received now: refused, nothing moves.
    assert_eq!(
        b.on_tick(
            key(1),
            OPEN + 3_600,
            ns(OPEN + 1) + LAT_NS,
            200.0,
            5_000,
            &mut out
        ),
        TickVerdict::RefusedFuture
    );
    assert_eq!(b.counters().refused_future, 1);
    assert_eq!(b.counters().accepted, 1);
    assert!(out.bars().is_empty(), "the real bar is not closed early");
    assert_eq!(b.open_bars(), 1, "the real bar is still open");
    // Later real ticks are accepted, and the refused cumulative did not move
    // the baseline (110 is not a volume drop).
    assert_eq!(
        tick(&mut b, OPEN + 2, 101.0, 110, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    assert_eq!(b.counters().volume_down, 0);
    b.on_clock(ns(OPEN + 5), &mut out);
    assert_eq!(out.bars().len(), 1);
    let bar = out.bars()[0];
    assert_eq!(bar.close_by, CloseBy::Clock, "closed at its own boundary");
    assert_eq!(bar.bucket_end_secs, i64::from(OPEN + 5));
    assert_eq!(bar.close_delay_ns, 0);
    assert_eq!((bar.high, bar.close, bar.volume), (101.0, 101.0, 10));
    assert_eq!(
        tick(&mut b, OPEN + 6, 102.0, 120, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    assert_eq!(b.open_bars(), 1);

    // The edge: exactly FUTURE_TICK_SKEW_SECS ahead of the reference is
    // accepted, one second more is refused. The reference is the later of the
    // clock second and the receipt second.
    let mut b = book(1, &[5]);
    let edge = OPEN + u32::try_from(FUTURE_TICK_SKEW_SECS).expect("small");
    assert_eq!(
        b.on_tick(key(1), edge + 1, ns(OPEN), 1.0, 1, &mut out),
        TickVerdict::RefusedFuture
    );
    assert_eq!(
        b.on_tick(key(1), edge, ns(OPEN), 1.0, 1, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    // A fresh book: the clock second is the reference when it is later than
    // the receipt second.
    let mut b = book(1, &[5]);
    b.on_clock(ns(OPEN + 10), &mut out);
    assert_eq!(
        b.on_tick(key(1), edge + 11, ns(OPEN), 1.0, 2, &mut out),
        TickVerdict::RefusedFuture,
        "one second past the clock-based edge is refused"
    );
    assert_eq!(
        b.on_tick(key(1), edge + 10, ns(OPEN), 1.0, 2, &mut out),
        TickVerdict::Accepted { late_frames: 0 },
        "the clock second is the reference when it is later than the receipt"
    );
}

#[test]
fn test_seven_second_frame_aligns_to_ist_midnight() {
    assert_eq!(DAY_1113 % 86_400, 0);
    assert_ne!(
        DAY_1113 % 7,
        0,
        "the epoch grid would be off by a few seconds"
    );
    let mut b = book(1, &[7]);
    let mut out = ClosedBars::with_capacity(8);
    // 09:14:59 and 09:15:00 share one bucket opening at sod 33,299.
    tick(&mut b, DAY_1113 + 33_299, 10.0, 1, &mut out);
    assert_eq!(
        tick(&mut b, DAY_1113 + 33_300, 11.0, 2, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    assert!(out.bars().is_empty(), "same bucket: nothing closed");
    b.on_clock(ns(DAY_1113 + 33_306), &mut out);
    assert_eq!(out.bars().len(), 1);
    let bar = out.bars()[0];
    assert_eq!(bar.bucket_open_secs, i64::from(DAY_1113 + 33_299));
    assert_eq!(bar.bucket_end_secs, i64::from(DAY_1113 + 33_306));
    assert_eq!(bar.tick_count, 2);
    assert_eq!(bar.close_delay_ns, 0);
}

#[test]
fn test_seven_second_frame_is_cut_at_midnight() {
    let mut b = book(1, &[7]);
    let mut out = ClosedBars::with_capacity(8);
    // sod 86,394 = 12,342 x 7: the day's last 7 s bucket has only 6 seconds.
    tick(&mut b, DAY_1113 + 86_394, 10.0, 1, &mut out);
    tick(&mut b, DAY_1113 + 86_399, 11.0, 2, &mut out);
    b.on_clock(ns(DAY_1113 + 86_400) - 1, &mut out);
    assert!(out.bars().is_empty());
    b.on_clock(ns(DAY_1113 + 86_400), &mut out);
    assert_eq!(out.bars().len(), 1);
    let bar = out.bars()[0];
    assert_eq!(bar.bucket_open_secs, i64::from(DAY_1113 + 86_394));
    assert_eq!(bar.bucket_end_secs, i64::from(DAY_1113 + 86_400));
    assert_eq!((bar.tick_count, bar.close_delay_ns), (2, 0));
    // The next day starts a fresh grid at midnight; buckets are contiguous
    // across it, so nothing counts as skipped.
    tick(&mut b, DAY_1113 + 86_400, 12.0, 3, &mut out);
    tick(&mut b, DAY_1113 + 86_407, 13.0, 4, &mut out);
    assert_eq!(out.bars().len(), 2);
    let next = out.bars()[1];
    assert_eq!(next.bucket_open_secs, i64::from(DAY_1113 + 86_400));
    assert_eq!(next.bucket_end_secs, i64::from(DAY_1113 + 86_407));
    assert_eq!(next.close_by, CloseBy::Tick);
    assert_eq!(b.counters().skipped_empty, 0);
    // A tick-close across midnight: no skipped bucket either.
    let mut b = book(1, &[7]);
    let mut out = ClosedBars::with_capacity(8);
    tick(&mut b, DAY_1113 + 86_399, 10.0, 1, &mut out);
    tick(&mut b, DAY_1113 + 86_401, 11.0, 2, &mut out);
    assert_eq!(out.bars()[0].bucket_end_secs, i64::from(DAY_1113 + 86_400));
    assert_eq!(b.counters().skipped_empty, 0);
}

#[test]
fn test_older_packet_never_overwrites_the_close() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN + 3, 101.0, 200, &mut out);
    // Older second AND a lower cumulative: arrives later, is older.
    assert_eq!(
        tick(&mut b, OPEN + 2, 100.0, 150, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    b.on_clock(ns(OPEN + 5), &mut out);
    let bar = out.bars()[0];
    assert_eq!(bar.close, 101.0);
    assert_eq!(bar.last_tick_exchange_secs, i64::from(OPEN + 3));
    assert_eq!(bar.last_tick_receipt_ns, ns(OPEN + 3) + LAT_NS);
    assert_eq!((bar.low, bar.high, bar.tick_count), (100.0, 101.0, 2));
    assert_eq!(bar.volume, 0);

    // Same second but a lower cumulative is also older: close kept.
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(4);
    tick(&mut b, OPEN + 3, 101.0, 200, &mut out);
    tick(&mut b, OPEN + 3, 99.0, 150, &mut out);
    b.on_clock(ns(OPEN + 5), &mut out);
    assert_eq!((out.bars()[0].close, out.bars()[0].low), (101.0, 99.0));
}

/// Review finding (2026-11-12): the candle fold treats a box clock up to 10 s
/// behind the exchange as honest; such a clock must still build bars.
#[test]
fn test_box_clock_ten_seconds_behind_still_builds_bars() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(8);
    // The box clock and the receipt run 10 s behind the exchange stamps.
    b.on_clock(ns(OPEN - 10), &mut out);
    for (i, px) in [(0u32, 100.0), (1, 101.0), (4, 99.0)] {
        assert_eq!(
            b.on_tick(
                key(1),
                OPEN + i,
                ns(OPEN + i - 10),
                px,
                100 + u64::from(i),
                &mut out
            ),
            TickVerdict::Accepted { late_frames: 0 }
        );
    }
    assert_eq!(b.counters().refused_future, 0);
    // The next bucket's first tick closes the bar.
    assert_eq!(
        b.on_tick(key(1), OPEN + 5, ns(OPEN - 5), 102.0, 110, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    assert_eq!(out.bars().len(), 1);
    let bar = out.bars()[0];
    assert_eq!(bar.close_by, CloseBy::Tick);
    assert_eq!(
        (bar.open, bar.high, bar.low, bar.close),
        (100.0, 101.0, 99.0, 99.0)
    );
}

/// With no clock reading yet and no receipt (0: a replayed frame with no
/// stamp) there is nothing to judge a future stamp by, so the tick is
/// accepted, as the candle fold and the spot store do.
#[test]
fn test_no_clock_and_no_receipt_accepts_the_tick() {
    let mut b = book(1, &[5]);
    let mut out = ClosedBars::with_capacity(8);
    assert_eq!(
        b.on_tick(key(1), OPEN, 0, 100.0, 1, &mut out),
        TickVerdict::Accepted { late_frames: 0 }
    );
    assert_eq!(b.counters().refused_future, 0);
    assert_eq!(b.open_bars(), 1);
}

#[test]
fn test_last_tick_late_names_the_frames_that_counted_the_tick_late() {
    // Frames 1 s and 5 s: a tick that lands in an already-closed 1 s bucket
    // but in the still-open 5 s bucket is late on frame 0 only.
    let mut b = book(1, &[1, 5]);
    let mut out = ClosedBars::with_capacity(8);
    tick(&mut b, OPEN, 10.0, 100, &mut out);
    assert!(!b.last_tick_late(0));
    assert!(!b.last_tick_late(1));
    // A tick of second OPEN+1 closes the 1 s bar of OPEN (by tick).
    tick(&mut b, OPEN + 1, 10.5, 101, &mut out);
    // A straggler for OPEN: late in the 1 s frame, on time in the 5 s frame.
    let v = tick(&mut b, OPEN, 10.2, 102, &mut out);
    assert_eq!(v, TickVerdict::Accepted { late_frames: 1 });
    assert!(b.last_tick_late(0));
    assert!(!b.last_tick_late(1));
    // The next on-time tick clears the mask.
    tick(&mut b, OPEN + 2, 10.6, 103, &mut out);
    assert!(!b.last_tick_late(0));
    // A refused tick clears it too, and out-of-range frames read false.
    tick(&mut b, OPEN, 10.2, 104, &mut out);
    assert!(b.last_tick_late(0));
    assert_eq!(
        b.on_tick(key(99), OPEN + 2, ns(OPEN + 2), 1.0, 1, &mut out),
        TickVerdict::RefusedUnknownKey
    );
    assert!(!b.last_tick_late(0));
    assert!(!b.last_tick_late(MAX_FRAMES));
    assert!(!b.last_tick_late(usize::MAX));
}

/// LF-6b finding 2: a packet with no new trade seeds or raises the volume
/// baseline and touches no bar, so the next trade's delta is only its own.
#[test]
fn test_note_baseline_volume_seeds_raises_never_lowers_and_opens_no_bar() {
    let mut b = book(1, &[60]);
    let mut out = ClosedBars::with_capacity(8);
    assert!(!b.note_baseline_volume(key(9), 10), "unknown key");
    assert!(b.note_baseline_volume(key(1), 1_000));
    assert_eq!(b.counters().baseline, 1);
    assert_eq!(b.open_bars(), 0, "no bar opened");
    // A lower (stale) cumulative never lowers it; a higher one raises it.
    assert!(b.note_baseline_volume(key(1), 900));
    assert!(b.note_baseline_volume(key(1), 1_100));
    assert_eq!(b.counters().baseline, 1);
    // The first trade's delta counts from 1,100.
    tick(&mut b, OPEN, 100.0, 1_150, &mut out);
    b.on_clock(ns(OPEN + 60), &mut out);
    assert_eq!(out.bars().len(), 1);
    assert_eq!(out.bars()[0].volume, 50);
}

/// LF-9 (findings 8, 9, 10): `on_trade` takes the fold's own per-tick volume.
/// The day's first trade credits its first-session delta only to the bars of
/// the buckets holding 09:15:00 (here the 1 s bar at 09:15:00 and the 60 s
/// bar), every later trade credits its delta everywhere, an older trade never
/// moves a close, and the book's own cumulative baseline is never read: a
/// raw cumulative that wrapped past `u32::MAX` does not matter.
#[test]
fn test_on_trade_takes_the_folds_volume_and_the_first_session_delta() {
    assert_eq!(
        MARKET_OPEN_SECS_OF_DAY,
        i64::from(crate::candles::tf_index::CANDLE_SESSION_OPEN_SECS_OF_DAY_IST)
    );
    let mut b = book(1, &[1, 60]);
    let mut out = ClosedBars::with_capacity(16);
    let first = TradeVolume {
        delta: 0,
        first_session_delta: 1_000,
        stale: false,
        restart: false,
    };
    b.on_trade(key(1), OPEN, ns(OPEN) + LAT_NS, 100.0, first, &mut out);
    let next = TradeVolume {
        delta: 400,
        first_session_delta: 400,
        stale: false,
        restart: false,
    };
    b.on_trade(
        key(1),
        OPEN + 59,
        ns(OPEN + 59) + LAT_NS,
        101.0,
        next,
        &mut out,
    );
    // LF-9 review: a trade at the SAME second with another price that is not
    // stale (an index tick, cumulative 0) moves the close, as the fold's
    // close does (its guard is only `!stale && secs >= close second`).
    let same_second = TradeVolume {
        delta: 0,
        first_session_delta: 0,
        stale: false,
        restart: false,
    };
    b.on_trade(
        key(1),
        OPEN + 59,
        ns(OPEN + 59) + LAT_NS + 1,
        102.0,
        same_second,
        &mut out,
    );
    // A stale packet (a cumulative regression) inside the open minute:
    // widens, adds its (zero) delta, never moves the close.
    let stale = TradeVolume {
        delta: 0,
        first_session_delta: 0,
        stale: true,
        restart: false,
    };
    b.on_trade(
        key(1),
        OPEN + 59,
        ns(OPEN + 59) + LAT_NS + 2,
        120.0,
        stale,
        &mut out,
    );
    b.on_clock(ns(OPEN + 61), &mut out);
    let one_s: Vec<_> = out.bars().iter().filter(|x| x.frame_secs == 1).collect();
    let minute = out
        .bars()
        .iter()
        .find(|x| x.frame_secs == 60)
        .expect("the 09:15 minute");
    assert_eq!(one_s[0].bucket_open_secs, i64::from(OPEN));
    assert_eq!(one_s[0].volume, 1_000, "the opening trade's own volume");
    assert_eq!(minute.volume, 1_400, "the opening trade plus the next");
    assert_eq!(
        minute.close, 102.0,
        "a same-second non-stale trade moves the close; a stale one never does"
    );
    assert_eq!(minute.high, 120.0, "but it widens the high");
    // The first trade of an instrument later in the day: its first-session
    // delta never reaches a bar that does not hold 09:15:00.
    let mut b = book(1, &[1, 60]);
    let mut out = ClosedBars::with_capacity(16);
    let late_first = TradeVolume {
        delta: 0,
        first_session_delta: 9_000,
        stale: false,
        restart: false,
    };
    b.on_trade(
        key(1),
        OPEN + 600,
        ns(OPEN + 600) + LAT_NS,
        50.0,
        late_first,
        &mut out,
    );
    b.on_clock(ns(OPEN + 700), &mut out);
    assert!(out.bars().iter().all(|x| x.volume == 0), "{:?}", out.bars());
    assert_eq!(
        b.counters().baseline,
        0,
        "the book's own baseline is untouched"
    );
}
