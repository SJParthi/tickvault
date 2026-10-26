//! Wall-clock harness for the `socket_to_wal` stage (audit PR M / M4,
//! 2026-10-26).
//!
//! The live session of 2026-10-26 measured the reader's hand-off
//! (`WalRingSink::accept`) at p50 ≤ 4.1 µs, p99 ≤ 32.8 µs and a tail over the
//! 100 µs budget on 0.014% of frames. The Criterion bench
//! `ws_reader/wal_append` cannot see the tail's source: it sends back to back,
//! so the WAL writer thread never parks and no frame ever has to wake it.
//! In production frames arrive about 400 µs apart (≈ 2,600 a second), so the
//! writer and the drain are usually parked when a frame lands, and the
//! reader pays the wake.
//!
//! This harness reproduces that shape: frames spaced `GAP` apart on a
//! dedicated thread, a real `WsFrameSpill` writer, a real `frame_ring` with a
//! drain parked on a separate two-worker tokio runtime (the production main
//! runtime's shape), and times three things per frame:
//!
//! 1. the WAL hand-off alone (`append_with_seq_at`);
//! 2. the ring hand-off alone (`FrameRingSender::try_send` waking the drain);
//! 3. the whole `WalRingSink::accept`.
//!
//! `#[ignore]`d: a wall-clock figure must never become a flaky CI gate.
//! Run with
//! `cargo test --release -p tickvault-core --test socket_to_wal_tail_harness -- --ignored --nocapture`.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tickvault_core::websocket::frame_ring::frame_ring;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::{
    CapturedFrame, FrameSink, RingByteBudget, WalRingSink,
};
use tickvault_storage::ws_frame_spill::{WalEndpoint, WsFrameSpill, WsType};

/// Frames per scenario. Big enough that p99.9 rests on 20 samples.
const FRAMES: usize = 20_000;
/// The live inter-frame gap at ≈ 2,600 frames a second.
const GAP: Duration = Duration::from_micros(400);
/// The stage's budget (`Stage::SocketToWal`).
const BUDGET_NANOS: u64 = 100_000;
/// Ring slots, same order as `FRAME_RING_CAPACITY`.
const RING_SLOTS: usize = 65_536;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "tv_socket_to_wal_{tag}_{}_{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn ticker_frame() -> Bytes {
    let mut buf = vec![0u8; 16];
    buf[0] = 2;
    buf[1..3].copy_from_slice(&16_u16.to_le_bytes());
    buf[3] = 2;
    buf[4..8].copy_from_slice(&1337_u32.to_le_bytes());
    buf[8..12].copy_from_slice(&100.5_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&1_700_000_000_u32.to_le_bytes());
    Bytes::from(buf)
}

/// Busy-waits until `deadline`: a sleep would add its own timer slack to the
/// gap and let the measuring thread migrate.
fn spin_until(deadline: Instant) {
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

struct Summary {
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
    over_budget: usize,
}

fn summarise(mut nanos: Vec<u64>) -> Summary {
    nanos.sort_unstable();
    let at = |q: f64| {
        let idx = ((nanos.len() as f64) * q) as usize;
        nanos[idx.min(nanos.len() - 1)]
    };
    Summary {
        p50: at(0.50),
        p99: at(0.99),
        p999: at(0.999),
        max: nanos[nanos.len() - 1],
        over_budget: nanos.iter().filter(|n| **n > BUDGET_NANOS).count(),
    }
}

fn report(name: &str, s: &Summary) {
    println!(
        "{name:<28} p50 {:>7} ns  p99 {:>7} ns  p99.9 {:>8} ns  max {:>9} ns  over 100us {}/{FRAMES}",
        s.p50, s.p99, s.p999, s.max, s.over_budget
    );
}

/// A drain parked on a two-worker runtime, receiving until the ring closes.
fn spawn_parked_drain(
    mut rx: tickvault_core::websocket::frame_ring::FrameRingReceiver<CapturedFrame>,
    budget: Option<Arc<RingByteBudget>>,
) -> (tokio::runtime::Runtime, tokio::task::JoinHandle<usize>) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let handle = rt.spawn(async move {
        let mut seen = 0_usize;
        while let Some(frame) = rx.recv().await {
            if let Some(b) = budget.as_ref() {
                b.release(frame.bytes.len());
            }
            seen += 1;
        }
        seen
    });
    (rt, handle)
}

#[test]
#[ignore = "wall-clock harness; run by hand with --ignored --nocapture"]
fn socket_to_wal_stage_cost_with_parked_consumers() {
    let frame = ticker_frame();

    // 1. WAL hand-off alone, writer parked between frames.
    let wal_dir = temp_dir("wal_only");
    let spill = WsFrameSpill::new(&wal_dir).expect("spill");
    std::thread::sleep(Duration::from_millis(300));
    let mut wal_nanos = Vec::with_capacity(FRAMES);
    let mut next = Instant::now();
    for seq in 0..FRAMES {
        next += GAP;
        spin_until(next);
        let started = Instant::now();
        let _ = spill.append_with_seq_at(
            WsType::LiveFeed,
            frame.clone(),
            1 + seq as u64,
            1_700_000_000_000_000_000,
            WalEndpoint::MainFeed,
        );
        wal_nanos.push(started.elapsed().as_nanos() as u64);
    }
    report("wal append (writer parked)", &summarise(wal_nanos));

    // 2. Ring hand-off alone, drain parked on another runtime.
    let (ring_tx, ring_rx) = frame_ring::<CapturedFrame>(RING_SLOTS);
    let (rt, drain) = spawn_parked_drain(ring_rx, None);
    std::thread::sleep(Duration::from_millis(50));
    let mut ring_nanos = Vec::with_capacity(FRAMES);
    let base = Instant::now();
    let mut next = Instant::now();
    for seq in 0..FRAMES {
        next += GAP;
        spin_until(next);
        let started = Instant::now();
        let _ = ring_tx.try_send(CapturedFrame {
            seq: seq as u64,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at: base,
            received_at_nanos: 0,
            wal_backed: true,
            bytes: frame.clone(),
        });
        ring_nanos.push(started.elapsed().as_nanos() as u64);
    }
    drop(ring_tx);
    let seen = rt.block_on(drain).expect("drain");
    assert_eq!(seen, FRAMES, "the drain saw every frame");
    report("ring try_send (drain parked)", &summarise(ring_nanos));

    // 3. The whole accept.
    let sink_dir = temp_dir("accept");
    let sink_spill = Arc::new(WsFrameSpill::new(&sink_dir).expect("spill"));
    let (tx, rx) = frame_ring::<CapturedFrame>(RING_SLOTS);
    let budget = Arc::new(RingByteBudget::with_slot_cap(1 << 30, RING_SLOTS));
    let (rt2, drain2) = spawn_parked_drain(rx, Some(Arc::clone(&budget)));
    let sink = WalRingSink::new(
        Arc::clone(&sink_spill),
        tx,
        budget,
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        0,
    );
    std::thread::sleep(Duration::from_millis(300));
    let mut accept_nanos = Vec::with_capacity(FRAMES);
    let mut next = Instant::now();
    for _ in 0..FRAMES {
        next += GAP;
        spin_until(next);
        let started = Instant::now();
        let _ = sink.accept(frame.clone());
        accept_nanos.push(started.elapsed().as_nanos() as u64);
    }
    drop(sink);
    let seen = rt2.block_on(drain2).expect("drain");
    assert_eq!(seen, FRAMES, "the drain saw every accepted frame");
    report("WalRingSink::accept", &summarise(accept_nanos));

    let _ = std::fs::remove_dir_all(&wal_dir);
    let _ = std::fs::remove_dir_all(&sink_dir);
}
