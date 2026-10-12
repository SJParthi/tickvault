//! Criterion benchmarks for the two per-frame drains of the live lane (audit
//! PROOF-9, 2026-10-11): the main-feed frame walk and the depth frame walk.
//!
//! Before this file no benchmark measured either. Both run once per frame on
//! the frame-drain task, depth at about 24 times the tick volume, so a
//! slowdown in either one stalls the socket drain without failing any check.
//!
//! Targets (budget keys in `quality/benchmark-budgets.toml`, per-element
//! counts in its `[elements]` table):
//!   - `feed_drain/main_feed_frame_4_quotes`: one frame stacking four Quote
//!     packets (decode, lag record, fold, row append). Budget per tick.
//!   - `feed_drain/depth20_frame_4x20`: one frame stacking four depth-20
//!     packets (80 levels). Budget per level.
//!   - `feed_drain/depth200_packet`: one depth-200 packet (200 levels).
//!     Budget per level: depth is O(levels) by nature, never O(1).
//!
//! Every level and tick is appended to an in-memory ILP buffer, which would
//! grow without bound across Criterion's millions of iterations. So the
//! ingest is rebuilt every `FRAME_POOL` frames, outside the timed region
//! (`iter_custom`); the timed region holds drain calls only.
//!
//! Companion zero-allocation evidence: `tests/dhat_live_ingest_seam.rs` and
//! `tests/dhat_depth_drain.rs`.
//!
//! Run: `cargo bench --bench feed_drain -p tickvault-app`

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_app::dhan_feed_stack::{
    DepthIngest, LiveIngest, counters, drain_depth_frame, drain_main_feed_frame,
};
use tickvault_common::constants::{DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID};
use tickvault_common::feed::Feed;
use tickvault_core::parser::depth::DepthFeedKind;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::tick_persistence::TickWriter;

/// Distinct pre-built frames, in time order. One ingest drains the pool once
/// from the start and is then rebuilt with the timer stopped, so every
/// ingest sees rising exchange times and cumulative volumes (cycling one
/// pool through one ingest would replay older trades and time the late-tick
/// path instead).
const FRAME_POOL: u64 = 1_024;

/// An exchange time inside a trading session (epoch seconds, IST wall time
/// as Dhan sends it), the same instant `dhat_live_ingest_seam.rs` uses.
const SESSION_EPOCH_SECS: u32 = 1_786_800_000;

/// A receipt instant inside the same session (UTC nanos): 13:20:00 IST.
const SESSION_RECEIPT_NANOS: i64 = (SESSION_EPOCH_SECS as i64 - 19_800) * 1_000_000_000;

/// A receipt instant the depth drain's own unit tests use.
const DEPTH_RECEIPT_NANOS: i64 = 1_779_355_000_000_000_000;

fn captured(bytes: Vec<u8>, endpoint: DhanEndpointType, seq: u64) -> CapturedFrame {
    CapturedFrame {
        seq,
        endpoint,
        connection_index: 5,
        received_at: Instant::now(),
        received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(Instant::now()),
        wal_backed: true,
        bytes: bytes.into(),
    }
}

/// One 50-byte Quote packet in the wire layout `parser/quote.rs` reads.
fn quote_packet(security_id: u32, ltp: f32, ltt: u32, volume: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 50];
    buf[0] = 4; // Quote
    buf[1..3].copy_from_slice(&50u16.to_le_bytes());
    buf[3] = 0; // IDX_I
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[14..18].copy_from_slice(&ltt.to_le_bytes());
    buf[22..26].copy_from_slice(&volume.to_le_bytes());
    buf
}

fn main_feed_frame(n: u64) -> CapturedFrame {
    let mut bytes = Vec::with_capacity(200);
    for i in 0..4u32 {
        bytes.extend_from_slice(&quote_packet(
            13 + i,
            100.0 + (n % 97) as f32 * 0.05,
            // Rising, about one second per four frames, and never ahead of
            // the receipt instant (the pool spans about 256 s before it).
            SESSION_EPOCH_SECS - 300 + (n / 4) as u32,
            2 + n as u32,
        ));
    }
    captured(bytes, DhanEndpointType::MainFeed, 1_000 + n)
}

/// One depth packet: 12-byte header, then `rows` 16-byte levels.
fn depth_packet(security_id: u32, feed_code: u8, rows: u32, depth200: bool, n: u64) -> Vec<u8> {
    let levels = rows as usize;
    let mut p = vec![0u8; 12 + levels * 16];
    let len = u16::try_from(p.len()).unwrap_or(u16::MAX);
    p[0..2].copy_from_slice(&len.to_le_bytes());
    p[2] = feed_code;
    p[3] = 2; // NSE_FNO
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    if depth200 {
        p[8..12].copy_from_slice(&rows.to_le_bytes());
    }
    for i in 0..levels {
        let base = 12 + i * 16;
        let price = 100.0_f64 + i as f64 * 0.05 + (n % 97) as f64 * 0.05;
        p[base..base + 8].copy_from_slice(&price.to_le_bytes());
        p[base + 8..base + 12].copy_from_slice(&(10 * (i as u32 + 1)).to_le_bytes());
        p[base + 12..base + 16].copy_from_slice(&(i as u32 + 1).to_le_bytes());
    }
    p
}

fn depth20_frame(n: u64) -> CapturedFrame {
    let mut bytes = Vec::with_capacity(4 * 332);
    for (sid, code) in [
        (60_001u32, DEEP_DEPTH_FEED_CODE_BID),
        (60_001, DEEP_DEPTH_FEED_CODE_ASK),
        (60_002, DEEP_DEPTH_FEED_CODE_BID),
        (60_002, DEEP_DEPTH_FEED_CODE_ASK),
    ] {
        bytes.extend_from_slice(&depth_packet(sid, code, 20, false, n));
    }
    captured(bytes, DhanEndpointType::Depth20, 1_000 + n)
}

fn depth200_frame(n: u64) -> CapturedFrame {
    let code = if n.is_multiple_of(2) {
        DEEP_DEPTH_FEED_CODE_BID
    } else {
        DEEP_DEPTH_FEED_CODE_ASK
    };
    captured(
        depth_packet(70_001, code, 200, true, n),
        DhanEndpointType::Depth200,
        1_000 + n,
    )
}

/// Times `iters` drain calls, rebuilding the ingest every `FRAME_POOL`
/// frames with the timer stopped.
fn run_timed<I>(
    iters: u64,
    frames: &[CapturedFrame],
    fresh: impl Fn() -> I,
    mut drain: impl FnMut(&mut I, &CapturedFrame, u64),
) -> Duration {
    let mut total = Duration::ZERO;
    let mut done = 0_u64;
    while done < iters {
        let mut ingest = fresh();
        let batch = (iters - done).min(FRAME_POOL);
        let start = Instant::now();
        for k in 0..batch {
            let i = done + k;
            drain(&mut ingest, &frames[k as usize], i);
        }
        total += start.elapsed();
        done += batch;
        drop(ingest);
    }
    total
}

fn bench_feed_drain(c: &mut Criterion) {
    let drain_counters = counters();

    let main_frames: Vec<CapturedFrame> = (0..FRAME_POOL).map(main_feed_frame).collect();
    c.bench_function("feed_drain/main_feed_frame_4_quotes", |b| {
        b.iter_custom(|iters| {
            run_timed(
                iters,
                &main_frames,
                || LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8),
                |ingest, frame, i| {
                    black_box(drain_main_feed_frame(
                        ingest,
                        black_box(frame),
                        SESSION_RECEIPT_NANOS,
                        1_000 + i,
                        drain_counters,
                    ));
                },
            )
        });
    });

    let depth20_frames: Vec<CapturedFrame> = (0..FRAME_POOL).map(depth20_frame).collect();
    c.bench_function("feed_drain/depth20_frame_4x20", |b| {
        b.iter_custom(|iters| {
            run_timed(
                iters,
                &depth20_frames,
                DepthIngest::for_test,
                |depth, frame, _| {
                    black_box(drain_depth_frame(
                        depth,
                        black_box(frame),
                        DEPTH_RECEIPT_NANOS,
                        DepthFeedKind::Twenty,
                        drain_counters,
                    ));
                },
            )
        });
    });

    let depth200_frames: Vec<CapturedFrame> = (0..FRAME_POOL).map(depth200_frame).collect();
    c.bench_function("feed_drain/depth200_packet", |b| {
        b.iter_custom(|iters| {
            run_timed(
                iters,
                &depth200_frames,
                DepthIngest::for_test,
                |depth, frame, _| {
                    black_box(drain_depth_frame(
                        depth,
                        black_box(frame),
                        DEPTH_RECEIPT_NANOS,
                        DepthFeedKind::TwoHundred,
                        drain_counters,
                    ));
                },
            )
        });
    });
}

criterion_group!(benches, bench_feed_drain);
criterion_main!(benches);
