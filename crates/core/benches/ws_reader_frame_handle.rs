//! Benchmark: STAGE-D P7.6 — WS reader frame handle latency.
//!
//! Measures the end-to-end hot path inside every WS read loop:
//!
//!   1. relaxed atomic counter bump (activity watchdog)
//!   2. durable WAL append via `WsFrameSpill::append` (disk-spill non-blocking)
//!   3. Dhan binary ticker packet dispatch via `dispatch_frame`
//!
//! Budget (`quality/benchmark-budgets.toml::ws_reader_frame_handle_ns`):
//!   < 100 ns/op on c7i.2xlarge. 5 % regression = CI hard fail.
//!
//! Rationale: the plan (P1.2 + P3.1 + P7.6) requires the read loop to
//! stay O(1) and never block on downstream. These three operations are
//! the ONLY work done per frame inside the read loop, so benchmarking
//! them together guarantees the hot path meets the latency bar even as
//! the WAL and watchdog primitives evolve.

use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::constants::{RESPONSE_CODE_TICKER, TICKER_PACKET_SIZE};
use tickvault_core::parser::dispatcher::dispatch_frame;
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType};

/// Build a minimal valid 16-byte ticker packet matching the format
/// defined in `docs/dhan-ref/03-live-market-feed-websocket.md`.
fn build_ticker_packet() -> Vec<u8> {
    let mut buf = vec![0u8; TICKER_PACKET_SIZE];
    buf[0] = RESPONSE_CODE_TICKER; // response code
    buf[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes()); // message length
    buf[3] = 2; // segment (NSE_FNO)
    buf[4..8].copy_from_slice(&1337_u32.to_le_bytes()); // security_id
    // payload: LTP (f32 LE), LTT (u32 LE)
    buf[8..12].copy_from_slice(&1500.75_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&1_700_000_000_u32.to_le_bytes());
    buf
}

/// Build a unique temp dir path for this benchmark run. We deliberately
/// do NOT add a new dependency (`tempfile` is not in the workspace and
/// adding deps requires Parthiban approval — see CLAUDE.md). Instead,
/// generate a unique path under `std::env::temp_dir()` using the
/// process PID + nanosecond timestamp so concurrent bench runs do not
/// collide, and remove it on Drop of the returned `TempDirGuard`.
struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "tickvault_ws_spill_bench_{tag}_{}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create_dir_all bench temp");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        // Best-effort cleanup. If the background writer thread still
        // holds an open segment, the directory may retain files. That
        // is acceptable for a benchmark — /tmp is cleaned on reboot.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spawn a temporary WAL spill backed by a fresh temp dir.
fn spawn_spill(tag: &str) -> (TempDirGuard, Arc<WsFrameSpill>) {
    let tmp = TempDirGuard::new(tag);
    let spill = WsFrameSpill::new(tmp.path()).expect("WsFrameSpill::new");
    (tmp, Arc::new(spill))
}

/// Benchmark the atomic counter bump in isolation. This is the single
/// hottest instruction in the read loop (fires on every frame) and is
/// the ratchet that keeps the activity watchdog wake-up reliable.
fn bench_activity_counter_bump(c: &mut Criterion) {
    let counter = AtomicU64::new(0);
    c.bench_function("ws_reader/activity_counter_bump", |b| {
        b.iter(|| {
            black_box(counter.fetch_add(1, Ordering::Relaxed));
        });
    });
}

/// Benchmark the WAL append path alone — allocate owned Vec from a
/// reference buffer, hand it to `WsFrameSpill::append`. This is the
/// single biggest source of work per frame, so it dominates the budget.
fn bench_wal_append(c: &mut Criterion) {
    let (_tmp, spill) = spawn_spill("append");
    let packet = build_ticker_packet();
    c.bench_function("ws_reader/wal_append", |b| {
        b.iter(|| {
            // Clone simulates the `data.to_vec()` done inside the read loop.
            let frame_vec = packet.clone();
            black_box(spill.append(WsType::LiveFeed, black_box(frame_vec)));
        });
    });
}

/// Benchmark the binary dispatch — the parse done AFTER the WAL append.
/// Already covered by `tick_parser.rs::dispatch_frame/ticker`, kept here
/// for side-by-side comparison in the same run.
fn bench_dispatch(c: &mut Criterion) {
    let packet = build_ticker_packet();
    c.bench_function("ws_reader/dispatch_ticker", |b| {
        b.iter(|| dispatch_frame(black_box(&packet), black_box(0)));
    });
}

/// Benchmark the full WS reader hot-path combining all three steps in
/// the exact order they execute inside the read loop:
///   counter bump → WAL append → dispatch.
///
/// This is the number compared against
/// `ws_reader_frame_handle_ns = 100` in the budget file.
fn bench_full_frame_handle(c: &mut Criterion) {
    let (_tmp, spill) = spawn_spill("full");
    let counter = AtomicU64::new(0);
    let packet = build_ticker_packet();
    c.bench_function("ws_reader/frame_handle_ns", |b| {
        b.iter(|| {
            // 1. Activity watchdog counter bump.
            counter.fetch_add(1, Ordering::Relaxed);
            // 2. Durable WAL append (owns a fresh Vec).
            let frame_vec = packet.clone();
            let outcome = spill.append(WsType::LiveFeed, frame_vec);
            black_box(outcome);
            // 3. Dispatch the binary packet.
            let parsed = dispatch_frame(black_box(&packet), black_box(0));
            black_box(parsed)
        });
    });
}

criterion_group!(
    benches,
    bench_activity_counter_bump,
    bench_wal_append,
    bench_dispatch,
    bench_full_frame_handle
);
criterion_main!(benches);
