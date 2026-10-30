//! Benchmark: the socket READ path's in-memory work per frame.
//!
//! Rewritten 2026-10-30 (plan ITEM DT, owner: every in-memory step a tick
//! passes through under 1 µs at p99). The old `ws_reader/frame_handle_ns`
//! timed a `Vec` clone (a heap allocation the read loop never makes), a
//! `WsFrameSpill::append` and a `dispatch_frame` parse (the drain parses, not
//! the reader). It now times exactly what the supervisor's read loop does per
//! frame and records as `Stage::SocketToWal`, in the same order:
//!
//!   1. `classify_frame` (is this frame a disconnect?);
//!   2. `silent_swap::note_depth200_frame` (one compare off depth-200);
//!   3. `WalRingSink::accept`: receipt stamp, sequence mint, WAL hand-off to a
//!      real writer thread, ring byte budget, ring push, drain notify.
//!
//! # What the number does NOT include
//!
//! The consumer is awake: after each timed batch the bench takes every frame
//! back off the ring, untimed, so no drain is parked on the ring's `Notify`.
//! When the drain IS parked (most frames in production, which arrive about
//! 400 µs apart) `accept` also pays a cross-thread wake, a futex/eventfd
//! syscall of about 18 µs on the x86 dev VM, the same as a bare
//! `Thread::unpark`. That cost is measured by
//! `crates/core/tests/socket_to_wal_tail_harness.rs`, not here, and the
//! production-spaced p50/p99/max of this path are in
//! `crates/core/tests/socket_read_in_memory_harness.rs`.
//!
//! Budget: `quality/benchmark-budgets.toml::ws_reader_frame_handle` = 1,000 ns
//! (matches `ws_reader/frame_handle_ns`; bench-gate reads Criterion's median).

use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_core::websocket::connection::classify_frame;
use tickvault_core::websocket::frame_ring::{FrameRingReceiver, frame_ring};
use tickvault_core::websocket::pool_budget::{ConnectionSlot, DhanAccount, DhanEndpointType};
use tickvault_core::websocket::pool_supervisor::{
    CapturedFrame, FrameSink, RingByteBudget, WalRingSink,
};
use tickvault_core::websocket::silent_swap::note_depth200_frame;
use tickvault_storage::ws_frame_spill::{
    WalEndpoint, WsFrameSpill, WsType, next_frame_seq, receipt_nanos_from,
};

/// Ring slots, same order as `FRAME_RING_CAPACITY`.
const RING_SLOTS: usize = 65_536;
/// Frames per timed batch. Under the ring's slots and the WAL queue, so
/// neither refuses: the bench measures the capture path, not a shed.
const BATCH: u64 = 512;

/// One main-feed Quote packet (code 4, 50 bytes).
fn quote_frame() -> Bytes {
    let mut buf = vec![0u8; 50];
    buf[0] = 4;
    buf[1..3].copy_from_slice(&50_u16.to_le_bytes());
    buf[3] = 2;
    buf[4..8].copy_from_slice(&1337_u32.to_le_bytes());
    buf[8..12].copy_from_slice(&1500.75_f32.to_le_bytes());
    Bytes::from(buf)
}

/// A unique temp dir, removed on drop (no `tempfile` dependency).
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
            "tickvault_ws_reader_bench_{tag}_{}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create_dir_all bench temp");
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Waits (untimed) until the WAL writer has taken every queued record.
fn wait_wal_empty(spill: &WsFrameSpill) {
    while spill.queued_records() > 0 {
        std::hint::spin_loop();
    }
}

/// Takes every frame back off the ring (untimed) and returns its bytes to
/// the budget, as the drain does.
fn drain_ring(rx: &mut FrameRingReceiver<CapturedFrame>, budget: &RingByteBudget) {
    while let Some(frame) = rx.try_recv() {
        budget.release(frame.bytes.len());
    }
}

fn bench_classify(c: &mut Criterion) {
    let frame = quote_frame();
    c.bench_function("ws_reader/classify_frame", |b| {
        b.iter(|| classify_frame(black_box(DhanEndpointType::MainFeed), black_box(&frame)));
    });
}

fn bench_receipt_and_seq(c: &mut Criterion) {
    c.bench_function("ws_reader/receipt_stamp", |b| {
        b.iter(|| black_box(receipt_nanos_from(Instant::now())));
    });
    c.bench_function("ws_reader/next_frame_seq", |b| {
        b.iter(|| black_box(next_frame_seq()));
    });
}

/// The WAL hand-off alone, with the production `Bytes` clone (a refcount
/// bump, no allocation).
fn bench_wal_append(c: &mut Criterion) {
    let tmp = TempDirGuard::new("append");
    let spill = WsFrameSpill::new(&tmp.path).expect("WsFrameSpill::new");
    let frame = quote_frame();
    let mut seq = 1_u64 << 40;
    c.bench_function("ws_reader/wal_append", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut left = iters;
            while left > 0 {
                let n = left.min(BATCH);
                let started = Instant::now();
                for _ in 0..n {
                    seq += 1 << 16;
                    black_box(spill.append_with_seq_at(
                        WsType::LiveFeed,
                        frame.clone(),
                        seq,
                        1_700_000_000_000_000_000,
                        WalEndpoint::MainFeed,
                    ));
                }
                total += started.elapsed();
                left -= n;
                wait_wal_empty(&spill);
            }
            total
        });
    });
}

/// The ring push alone (crossbeam slot + drain notify, no parked drain).
fn bench_ring_push(c: &mut Criterion) {
    let (tx, mut rx) = frame_ring::<CapturedFrame>(RING_SLOTS);
    let budget = RingByteBudget::with_slot_cap(1 << 30, RING_SLOTS);
    let frame = quote_frame();
    let base = Instant::now();
    c.bench_function("ws_reader/ring_push", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut left = iters;
            while left > 0 {
                let n = left.min(BATCH);
                let started = Instant::now();
                for seq in 0..n {
                    let pushed = tx.try_send(CapturedFrame {
                        seq,
                        endpoint: DhanEndpointType::MainFeed,
                        connection_index: 0,
                        received_at: base,
                        received_at_nanos: 0,
                        wal_backed: true,
                        bytes: frame.clone(),
                    });
                    black_box(pushed.is_ok());
                }
                total += started.elapsed();
                left -= n;
                drain_ring(&mut rx, &budget);
            }
            total
        });
    });
}

/// The whole per-frame read path, in the read loop's order.
fn bench_full_frame_handle(c: &mut Criterion) {
    let tmp = TempDirGuard::new("full");
    let spill = Arc::new(WsFrameSpill::new(&tmp.path).expect("WsFrameSpill::new"));
    let (tx, mut rx) = frame_ring::<CapturedFrame>(RING_SLOTS);
    let budget = Arc::new(RingByteBudget::with_slot_cap(1 << 30, RING_SLOTS));
    let sink = WalRingSink::new(
        Arc::clone(&spill),
        tx,
        Arc::clone(&budget),
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        0,
    );
    let slot = ConnectionSlot {
        account: DhanAccount::Primary,
        endpoint: DhanEndpointType::MainFeed,
        pool_index: 0,
        global_index: 0,
    };
    let frame = quote_frame();
    c.bench_function("ws_reader/frame_handle_ns", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut left = iters;
            while left > 0 {
                let n = left.min(BATCH);
                let started = Instant::now();
                for _ in 0..n {
                    let class = classify_frame(DhanEndpointType::MainFeed, black_box(&frame));
                    note_depth200_frame(slot, &frame, started);
                    black_box(class);
                    black_box(sink.accept(frame.clone()));
                }
                total += started.elapsed();
                left -= n;
                drain_ring(&mut rx, &budget);
                wait_wal_empty(&spill);
            }
            total
        });
    });
}

criterion_group!(
    benches,
    bench_classify,
    bench_receipt_and_seq,
    bench_wal_append,
    bench_ring_push,
    bench_full_frame_handle
);
criterion_main!(benches);
