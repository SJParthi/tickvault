//! Wall-clock harness for the socket READ stage's in-memory work per frame
//! (plan ITEM DT, 2026-10-30).
//!
//! The owner's target: every in-memory step a tick passes through takes under
//! 1 µs at p99. On the socket read task that step is, per frame, exactly what
//! the supervisor times as `Stage::SocketToWal`:
//!
//! 1. `classify_frame` (connection.rs: is this frame a disconnect?);
//! 2. `silent_swap::note_depth200_frame` (one compare off depth-200);
//! 3. `WalRingSink::accept` (receipt stamp, sequence, WAL hand-off, ring
//!    budget, ring push, drain notify).
//!
//! `socket_to_wal_tail_harness.rs` measures the same accept with the WAL
//! writer and the drain PARKED, where the reader pays a cross-thread wake (a
//! futex/eventfd syscall, about 18 µs on the x86 dev VM, the same as a bare
//! `Thread::unpark`). This harness measures the in-memory part on its own: the
//! frames are spaced like production (400 µs, so caches are as cold as live),
//! and the consumer takes each frame with `try_recv` between frames, untimed,
//! so no drain is registered on the ring's `Notify` and the push costs no
//! syscall. A real WAL writer thread runs throughout.
//!
//! `#[ignore]`d: a wall-clock figure must never become a flaky CI gate. Run
//! `cargo test --release -p tickvault-core --test socket_read_in_memory_harness -- --ignored --nocapture`.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tickvault_core::websocket::connection::classify_frame;
use tickvault_core::websocket::frame_ring::frame_ring;
use tickvault_core::websocket::pool_budget::{ConnectionSlot, DhanAccount, DhanEndpointType};
use tickvault_core::websocket::pool_supervisor::{
    CapturedFrame, FrameSink, FrameSinkOutcome, RingByteBudget, WalRingSink,
};
use tickvault_core::websocket::silent_swap::note_depth200_frame;
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType};

/// Frames per scenario.
const FRAMES: usize = 20_000;
/// The live inter-frame gap at about 2,600 frames a second.
const GAP: Duration = Duration::from_micros(400);
/// The owner's per-step target.
const TARGET_NANOS: u64 = 1_000;
/// Ring slots, same order as `FRAME_RING_CAPACITY`.
const RING_SLOTS: usize = 65_536;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "tv_socket_read_mem_{tag}_{}_{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// One main-feed Quote packet (code 4, 50 bytes).
fn quote_packet(security_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 50];
    buf[0] = 4;
    buf[1..3].copy_from_slice(&50_u16.to_le_bytes());
    buf[3] = 2;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&100.5_f32.to_le_bytes());
    buf
}

/// A frame of `packets` stacked Quote packets.
fn stacked_frame(packets: u32) -> Bytes {
    let mut frame = Vec::with_capacity(50 * packets as usize);
    for i in 0..packets {
        frame.extend_from_slice(&quote_packet(1_000 + i));
    }
    Bytes::from(frame)
}

fn spin_until(deadline: Instant) {
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

fn report(name: &str, mut nanos: Vec<u64>) {
    nanos.sort_unstable();
    let at = |q: f64| nanos[((nanos.len() as f64 * q) as usize).min(nanos.len() - 1)];
    let over = nanos.iter().filter(|n| **n > TARGET_NANOS).count();
    println!(
        "{name:<40} p50 {:>6} ns  p99 {:>6} ns  p99.9 {:>7} ns  max {:>8} ns  over 1us {over}/{}",
        at(0.50),
        at(0.99),
        at(0.999),
        nanos[nanos.len() - 1],
        nanos.len()
    );
}

/// The read task's per-frame work, spaced like production, consumer awake.
fn per_frame_spaced(frame: &Bytes, tag: &str) -> Vec<u64> {
    let dir = temp_dir(tag);
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill"));
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
    std::thread::sleep(Duration::from_millis(200));
    let mut nanos = Vec::with_capacity(FRAMES);
    let mut next = Instant::now();
    for _ in 0..FRAMES {
        next += GAP;
        spin_until(next);
        let started = Instant::now();
        let class = classify_frame(DhanEndpointType::MainFeed, frame);
        note_depth200_frame(slot, frame, started);
        let outcome = sink.accept(frame.clone());
        nanos.push(started.elapsed().as_nanos() as u64);
        std::hint::black_box(class);
        assert_eq!(outcome, FrameSinkOutcome::Captured, "every frame captured");
        let taken = rx.try_recv().expect("the frame is in the ring");
        budget.release(taken.bytes.len());
    }
    drop(sink);
    spill.shutdown(Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
    nanos
}

/// The same work back to back (a burst: the market open or a stacked read).
fn per_frame_burst(frame: &Bytes, tag: &str) -> Vec<u64> {
    let dir = temp_dir(tag);
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill"));
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
    std::thread::sleep(Duration::from_millis(200));
    let mut nanos = Vec::with_capacity(FRAMES);
    // Bursts of 256 frames, then an untimed drain, so neither the ring nor
    // the WAL queue fills.
    const BURST: usize = 256;
    for _ in 0..FRAMES / BURST {
        for _ in 0..BURST {
            let started = Instant::now();
            let class = classify_frame(DhanEndpointType::MainFeed, frame);
            note_depth200_frame(slot, frame, started);
            let outcome = sink.accept(frame.clone());
            nanos.push(started.elapsed().as_nanos() as u64);
            std::hint::black_box(class);
            assert_eq!(outcome, FrameSinkOutcome::Captured, "every frame captured");
        }
        while let Some(taken) = rx.try_recv() {
            budget.release(taken.bytes.len());
        }
        while spill.queued_records() > 0 {
            std::hint::spin_loop();
        }
    }
    drop(sink);
    spill.shutdown(Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
    nanos
}

#[test]
#[ignore = "wall-clock harness; run by hand with --ignored --nocapture"]
fn socket_read_in_memory_cost_per_frame() {
    // The floor: an empty timed region (two clock reads), spaced.
    let mut floor = Vec::with_capacity(FRAMES);
    let mut next = Instant::now();
    for _ in 0..FRAMES {
        next += GAP;
        spin_until(next);
        let started = Instant::now();
        floor.push(started.elapsed().as_nanos() as u64);
    }
    report("timer floor (empty region, spaced)", floor);
    for packets in [1_u32, 4, 64] {
        let frame = stacked_frame(packets);
        // classify alone, spaced.
        let mut classify = Vec::with_capacity(FRAMES);
        let mut next = Instant::now();
        for _ in 0..FRAMES {
            next += GAP;
            spin_until(next);
            let started = Instant::now();
            let class = classify_frame(DhanEndpointType::MainFeed, &frame);
            classify.push(started.elapsed().as_nanos() as u64);
            std::hint::black_box(class);
        }
        report(&format!("classify_frame {packets} pkt (spaced)"), classify);
        report(
            &format!("read path {packets} pkt (spaced, drain awake)"),
            per_frame_spaced(&frame, "spaced"),
        );
        report(
            &format!("read path {packets} pkt (burst)"),
            per_frame_burst(&frame, "burst"),
        );
    }
}

/// Times one closure per frame at production spacing.
fn spaced(mut step: impl FnMut()) -> Vec<u64> {
    let mut nanos = Vec::with_capacity(FRAMES);
    let mut next = Instant::now();
    for _ in 0..FRAMES {
        next += GAP;
        spin_until(next);
        let started = Instant::now();
        step();
        nanos.push(started.elapsed().as_nanos() as u64);
    }
    nanos
}

/// The read path split into its steps, each timed alone at production
/// spacing, so the step that dominates is visible.
#[test]
#[ignore = "wall-clock harness; run by hand with --ignored --nocapture"]
fn socket_read_in_memory_cost_breakdown() {
    use tickvault_storage::ws_frame_spill::{WalEndpoint, next_frame_seq, receipt_nanos_from};
    let frame = stacked_frame(1);
    report(
        "receipt: Instant::now + receipt_nanos_from",
        spaced(|| {
            std::hint::black_box(receipt_nanos_from(Instant::now()));
        }),
    );
    report(
        "next_frame_seq",
        spaced(|| {
            std::hint::black_box(next_frame_seq());
        }),
    );
    let dir = temp_dir("breakdown");
    let spill = WsFrameSpill::new(&dir).expect("spill");
    std::thread::sleep(Duration::from_millis(200));
    let mut seq = 1_u64 << 40;
    report(
        "WAL append_with_seq_at (writer awake)",
        spaced(|| {
            seq += 1 << 16;
            std::hint::black_box(spill.append_with_seq_at(
                WsType::LiveFeed,
                frame.clone(),
                seq,
                1_700_000_000_000_000_000,
                WalEndpoint::MainFeed,
            ));
        }),
    );
    spill.shutdown(Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
    let budget = RingByteBudget::with_slot_cap(1 << 30, RING_SLOTS);
    report(
        "ring budget reserve + release",
        spaced(|| {
            std::hint::black_box(budget.try_reserve_detailed(50));
            budget.release(50);
        }),
    );
    let (tx, mut rx) = frame_ring::<CapturedFrame>(RING_SLOTS);
    let base = Instant::now();
    let mut ring = Vec::with_capacity(FRAMES);
    let mut next = Instant::now();
    for s in 0..FRAMES {
        next += GAP;
        spin_until(next);
        let started = Instant::now();
        let pushed = tx.try_send(CapturedFrame {
            seq: s as u64,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at: base,
            received_at_nanos: 0,
            wal_backed: true,
            bytes: frame.clone(),
        });
        ring.push(started.elapsed().as_nanos() as u64);
        assert!(pushed.is_ok(), "room in the ring");
        assert!(rx.try_recv().is_some(), "the frame is in the ring");
    }
    report("ring try_send (drain awake)", ring);
}

/// The read path with a REAL drain task (`recv().await` on a two-worker
/// runtime, the production main runtime's shape), back to back. The drain is
/// awake for most frames and parks only when it catches up, so this is the
/// shape where a drain that registered on every `recv` (before ITEM DT1)
/// could be found WAITING by the reader.
#[test]
#[ignore = "wall-clock harness; run by hand with --ignored --nocapture"]
fn socket_read_cost_with_a_live_drain_in_a_burst() {
    let frame = stacked_frame(1);
    let dir = temp_dir("live_drain");
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill"));
    let (tx, mut rx) = frame_ring::<CapturedFrame>(RING_SLOTS);
    let budget = Arc::new(RingByteBudget::with_slot_cap(1 << 30, RING_SLOTS));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let drain_budget = Arc::clone(&budget);
    let drain = rt.spawn(async move {
        let mut seen = 0_usize;
        while let Some(frame) = rx.recv().await {
            drain_budget.release(frame.bytes.len());
            // About a drain's per-frame work, so it is busy, not idle.
            let deadline = Instant::now() + Duration::from_nanos(300);
            spin_until(deadline);
            seen += 1;
        }
        seen
    });
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
    std::thread::sleep(Duration::from_millis(200));
    let mut nanos = Vec::with_capacity(FRAMES * 5);
    for _ in 0..5 {
        for _ in 0..FRAMES {
            let started = Instant::now();
            let class = classify_frame(DhanEndpointType::MainFeed, &frame);
            note_depth200_frame(slot, &frame, started);
            let outcome = sink.accept(frame.clone());
            nanos.push(started.elapsed().as_nanos() as u64);
            std::hint::black_box(class);
            assert_eq!(outcome, FrameSinkOutcome::Captured, "every frame captured");
            // Paced like a busy socket (the 09:15 burst is about 80 us apart;
            // this is denser, so the drain is caught awake often).
            spin_until(started + Duration::from_micros(1));
        }
        while spill.queued_records() > 0 {
            std::hint::spin_loop();
        }
    }
    drop(sink);
    let seen = rt.block_on(drain).expect("drain");
    assert_eq!(seen, FRAMES * 5, "the drain saw every frame");
    report("read path 1 pkt (burst, live drain)", nanos);
    spill.shutdown(Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The drain's `recv` over a queued backlog (ITEM DT1): mean ns per item.
/// Before the fast path every `recv` called `Notified::enable()`, which with
/// no stored permit takes the waiter lock to register and again on drop.
#[test]
#[ignore = "wall-clock harness; run by hand with --ignored --nocapture"]
fn ring_recv_cost_over_a_queued_backlog() {
    const ITEMS: usize = 1_000_000;
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    for round in 0..3 {
        let (tx, mut rx) = frame_ring::<u64>(ITEMS);
        for i in 0..ITEMS as u64 {
            tx.try_send(i).expect("room");
        }
        let started = Instant::now();
        let got = rt.block_on(async {
            let mut got = 0_usize;
            for _ in 0..ITEMS {
                if rx.recv().await.is_some() {
                    got += 1;
                }
            }
            got
        });
        let per_item = started.elapsed().as_nanos() as f64 / ITEMS as f64;
        assert_eq!(got, ITEMS);
        println!("ring recv over a queued backlog, round {round}: {per_item:.1} ns per item");
    }
}
