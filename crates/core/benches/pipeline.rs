//! Benchmark: tick parsing pipeline throughput.
//!
//! Simulates batch processing of tick frames (as the pipeline would do)
//! to measure per-frame overhead including header parse + dispatch.
//! Budget from quality/benchmark-budgets.toml: pipeline < 100ns/tick.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use dhan_live_trader_common::constants::{
    QUOTE_PACKET_SIZE, RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE,
};
use dhan_live_trader_core::parser::dispatcher::dispatch_frame;

/// Build a minimal valid binary packet.
#[allow(clippy::indexing_slicing, clippy::as_conversions)]
fn build_packet(response_code: u8, size: usize, segment: u8, security_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    buf[0] = response_code;
    buf[1..3].copy_from_slice(&(size as u16).to_le_bytes());
    buf[3] = segment;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf
}

fn bench_batch_100_mixed(c: &mut Criterion) {
    // Pre-build 100 mixed packets (50 ticker + 50 quote)
    let packets: Vec<Vec<u8>> = (0..100u32)
        .map(|i| {
            if i % 2 == 0 {
                build_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE, 2, i)
            } else {
                build_packet(RESPONSE_CODE_QUOTE, QUOTE_PACKET_SIZE, 3, i)
            }
        })
        .collect();

    c.bench_function("pipeline/batch_100_mixed", |b| {
        b.iter(|| {
            let now = 1_000_000i64;
            for packet in &packets {
                let _ = dispatch_frame(black_box(packet), black_box(now));
            }
        });
    });
}

fn bench_burst_ticker_only(c: &mut Criterion) {
    let packets: Vec<Vec<u8>> = (0..100u32)
        .map(|i| build_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE, 2, i))
        .collect();

    c.bench_function("pipeline/burst_100_ticker", |b| {
        b.iter(|| {
            for packet in &packets {
                let _ = dispatch_frame(black_box(packet), black_box(0));
            }
        });
    });
}

criterion_group!(benches, bench_batch_100_mixed, bench_burst_ticker_only);
criterion_main!(benches);
