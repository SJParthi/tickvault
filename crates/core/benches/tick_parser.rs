//! Benchmark: binary tick parser latency.
//!
//! Measures dispatch_frame() performance for ticker and quote packets.
//! Budget from quality/benchmark-budgets.toml.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::constants::{
    QUOTE_PACKET_SIZE, RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE,
};
use tickvault_core::parser::dispatcher::dispatch_frame;

/// Build a minimal valid binary packet.
fn build_packet(response_code: u8, size: usize, segment: u8, security_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    buf[0] = response_code;
    buf[1..3].copy_from_slice(&(size as u16).to_le_bytes());
    buf[3] = segment;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf
}

fn bench_ticker_parse(c: &mut Criterion) {
    let packet = build_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE, 2, 42);
    c.bench_function("dispatch_frame/ticker", |b| {
        b.iter(|| dispatch_frame(black_box(&packet), black_box(0)));
    });
}

fn bench_quote_parse(c: &mut Criterion) {
    let packet = build_packet(RESPONSE_CODE_QUOTE, QUOTE_PACKET_SIZE, 2, 42);
    c.bench_function("dispatch_frame/quote", |b| {
        b.iter(|| dispatch_frame(black_box(&packet), black_box(0)));
    });
}

/// The Dhan trade-time correction ARMED (scope lock 2026-11-20): an index
/// ticker on a confirmed segment, its time corrected by +19,800 s. Unarmed,
/// the correction is one atomic load (the two benches above).
fn bench_ticker_parse_ltt_armed(c: &mut Criterion) {
    use tickvault_storage::ltt_correction::{LttEntry, LttTable, install_table, receipt_ist_day};
    // 2026-11-20 11:00:00 IST as a UTC receipt; raw time = UTC (the 20 Nov shape).
    let receipt: i64 = 1_795_152_600 * 1_000_000_000;
    let mut table = LttTable::empty();
    let _armed = table.arm(
        receipt_ist_day(receipt),
        LttEntry {
            id: 0,
            segment: 0,
            from_nanos: receipt - 1_000_000_000,
            to_nanos: i64::MAX,
        },
    );
    install_table(table);
    let mut packet = build_packet(RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE, 0, 13);
    packet[12..16].copy_from_slice(&1_795_152_600u32.to_le_bytes());
    c.bench_function("dispatch_frame/ticker_ltt_armed", |b| {
        b.iter(|| dispatch_frame(black_box(&packet), black_box(receipt)));
    });
    install_table(LttTable::empty());
}

fn bench_invalid_empty(c: &mut Criterion) {
    c.bench_function("dispatch_frame/empty_error", |b| {
        b.iter(|| dispatch_frame(black_box(&[]), black_box(0)));
    });
}

criterion_group!(
    benches,
    bench_ticker_parse,
    bench_quote_parse,
    bench_ticker_parse_ltt_armed,
    bench_invalid_empty
);
criterion_main!(benches);
