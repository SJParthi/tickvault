//! Per-call latency of the tick hot path, as a DISTRIBUTION (p50 / p99 /
//! p99.9 / max), not a mean.
//!
//! Criterion reports a mean with a confidence interval; the owner's target
//! (2026-10-02) is a p99 in microseconds, which a mean cannot show. This
//! harness times every call on its own with `Instant` and reports the
//! percentiles, next to the timer's own floor (two back-to-back `Instant`
//! reads) so a reader can tell the work from the clock.
//!
//! Stages, each the same code the live drain runs per packet:
//! 1. `dispatch_frame` on a 16-byte ticker packet and a 162-byte full packet.
//! 2. `MultiTfAggregator::consume_tick` over 5,000 instruments, time moving
//!    forward so every timeframe rolls and seals as it does live (the seal
//!    callback is a no-op here; the hand-off to the seal writer is not timed).
//! 3. decode + fold together, per packet.
//!
//! `#[ignore]`d: a wall-clock figure must never become a flaky CI gate (the
//! house rule for every `*_sweep_cost_*` harness). Run it in release:
//!
//! ```text
//! cargo test --release -p tickvault-trading --test hot_path_latency -- --ignored --nocapture
//! ```
//!
//! What it does NOT measure: the WAL append (`tickvault-core`'s
//! `hot_path_latency`), the ILP row append, the socket read, scheduler delay,
//! or a page fault on first touch. A figure here is the CPU cost of the code,
//! not the end-to-end delay of a tick.

use std::hint::black_box;
use std::time::Instant;

use tickvault_common::constants::{RESPONSE_CODE_FULL, RESPONSE_CODE_TICKER};
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::parser::dispatcher::dispatch_frame;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;

/// An IST trading day's midnight, in IST epoch seconds (as the other suites).
const DAY: u32 = 1_779_321_600;
/// 10:00:00 IST, inside the continuous session.
const TEN: u32 = DAY + 36_000;
const IST_OFFSET_SECS: i64 = 19_800;
const INSTRUMENTS: u64 = 5_000;
const SAMPLES: usize = 1_000_000;

struct Dist {
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

fn dist(mut ns: Vec<u64>) -> Dist {
    ns.sort_unstable();
    let at = |q: f64| ns[((ns.len() as f64 - 1.0) * q) as usize];
    Dist {
        p50: at(0.50),
        p99: at(0.99),
        p999: at(0.999),
        max: ns[ns.len() - 1],
    }
}

fn report(stage: &str, d: &Dist) {
    eprintln!(
        "LATENCY | {stage:<44} | p50 {:>6} ns | p99 {:>6} ns | p99.9 {:>7} ns | max {:>9} ns",
        d.p50, d.p99, d.p999, d.max
    );
}

fn time_each(samples: usize, mut f: impl FnMut(usize)) -> Dist {
    let mut ns = Vec::with_capacity(samples);
    for i in 0..samples {
        let t = Instant::now();
        f(i);
        ns.push(t.elapsed().as_nanos() as u64);
    }
    dist(ns)
}

fn ticker_packet(sid: u32) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[0] = RESPONSE_CODE_TICKER;
    buf[1..3].copy_from_slice(&16u16.to_le_bytes());
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&sid.to_le_bytes());
    buf[8..12].copy_from_slice(&1_500.75_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&1_700_000_000_u32.to_le_bytes());
    buf
}

fn full_packet(sid: u32) -> [u8; 162] {
    let mut buf = [0u8; 162];
    buf[0] = RESPONSE_CODE_FULL;
    buf[1..3].copy_from_slice(&162u16.to_le_bytes());
    buf[3] = 2;
    buf[4..8].copy_from_slice(&sid.to_le_bytes());
    buf[8..12].copy_from_slice(&1_500.75_f32.to_le_bytes());
    buf
}

/// The i-th tick of a stream that visits every instrument once per second,
/// so each instrument's S1 bucket rolls on every visit and the longer frames
/// roll on their own boundaries, as live.
fn stream_tick(i: usize) -> ParsedTick {
    let sid = 10_000 + (i as u64 % INSTRUMENTS);
    let secs = TEN + (i as u64 / INSTRUMENTS) as u32;
    let n = (i as u64 / INSTRUMENTS) as u32;
    ParsedTick {
        security_id: sid,
        exchange_segment_code: 2,
        last_traded_price: 100.0 + (n % 50) as f32 * 0.05,
        last_trade_quantity: 1,
        exchange_timestamp: secs,
        received_at_nanos: (i64::from(secs) - IST_OFFSET_SECS) * 1_000_000_000,
        volume: 1 + n,
        ..ParsedTick::default()
    }
}

#[test]
#[ignore = "wall-clock harness; run with --release -- --ignored --nocapture"]
fn tick_hot_path_latency_distribution() {
    // The clock itself.
    let floor = time_each(SAMPLES, |_| {});
    report("timer floor (two Instant reads)", &floor);

    let ticker = ticker_packet(1_337);
    let d = time_each(SAMPLES, |i| {
        black_box(dispatch_frame(black_box(&ticker), i as i64).ok());
    });
    report("decode ticker packet (16 B)", &d);

    let full = full_packet(1_337);
    let d = time_each(SAMPLES, |i| {
        black_box(dispatch_frame(black_box(&full), i as i64).ok());
    });
    report("decode full packet (162 B)", &d);

    // Fold: pre-sized as the live drain does, then one warm pass so the
    // first-touch slot allocation is outside the timed window.
    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, INSTRUMENTS as usize);
    let mut seals: u64 = 0;
    for i in 0..INSTRUMENTS as usize {
        agg.consume_tick(Feed::Dhan, &stream_tick(i), None, |_, _, _, _, _| {
            seals += 1
        });
    }
    let base = INSTRUMENTS as usize;
    let d = time_each(SAMPLES, |i| {
        let t = stream_tick(base + i);
        black_box(agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| seals += 1));
    });
    report("candle fold, 5,000 instruments, 10 frames", &d);
    eprintln!("LATENCY | fold sealed {seals} bars during the run");
    assert!(
        seals > 0,
        "the stream must roll buckets, or the fold was not exercised"
    );

    let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, INSTRUMENTS as usize);
    for i in 0..INSTRUMENTS as usize {
        agg.consume_tick(Feed::Dhan, &stream_tick(i), None, |_, _, _, _, _| {});
    }
    let d = time_each(SAMPLES, |i| {
        let packet = ticker_packet(1_337);
        black_box(dispatch_frame(black_box(&packet), i as i64).ok());
        let t = stream_tick(base + i);
        black_box(agg.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {}));
    });
    report("decode + fold, per packet", &d);
}
