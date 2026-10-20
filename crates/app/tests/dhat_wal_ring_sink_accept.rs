//! DHAT ratchet for `WalRingSink::accept`, the production per-frame path on
//! every socket read task (audit HOT-1, 2026-10-11).
//!
//! ## Why this file exists
//!
//! `pool_supervisor.rs` claimed "`WalRingSink::accept` performs no heap
//! allocation". Nothing measured it. The reader DHAT test
//! (`crates/core/tests/dhat_ws_reader_zero_alloc.rs`) times a stand-in — one
//! counter bump, one `Bytes::clone` and `dispatch_frame` — and never calls
//! `accept`, the WAL append, the byte budget or the ring `try_send`. The
//! storage guard `wal_append_zero_alloc_by_construction_guard.rs` scans the
//! source of the WAL append only.
//!
//! This file runs the REAL code: a real `WsFrameSpill` with its writer thread,
//! a real `RingByteBudget`, the real tokio channel type the boot site builds
//! (`tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(FRAME_RING_CAPACITY)`), and
//! the production metrics recorder installed so every counter handle is a real
//! one, not a no-op.
//!
//! ## Two arms, two different facts
//!
//! * **Steady state** — the drain keeps up: 10,000 frames, each accepted,
//!   received and released. The WAL writer thread's per-record work is inside
//!   the window too (the arm waits until `persisted_count` catches up). Budget:
//!   the house 8 blocks / 1 KiB for 10,000 calls.
//! * **Backlog** — no consumer: 4,096 frames queue in the ring. Until
//!   2026-10-20 the ring was a tokio bounded `mpsc`, which grows in 32-slot
//!   blocks, so this arm measured one block per 32 queued frames on the socket
//!   reader task. The ring is now `frame_ring` (audit M1, 2026-10-20): a
//!   pre-allocated array plus a `Notify`, so the backlog is held to the same
//!   house budget as the steady state.
//!
//! **⚠ CHANGED 2026-10-20 (audit M1):** the notes below describe the tokio
//! ring this file measured until then; they are kept as history. The
//! re-backlog arm now asserts the house budget too.
//!
//! **⚠ CORRECTED 2026-10-11 (review of HOT-1).** Two things above were wrong
//! or missing. (1) "Blocks are reused once the drain catches up" is false: in
//! tokio 1.53.2 `reclaim_block` re-links a drained block after the tail at
//! most 3 times and frees it otherwise, so only about 3 spare blocks survive
//! and every new backlog allocates again. The **re-backlog** phase below
//! measures that: after a full drain, a second backlog of the same size
//! allocates about one block per 32 frames again. (2) The three SHED arms of
//! `accept` (byte budget full, slot budget full, ring `try_send` full) were
//! not measured. The **shed** phase runs each against its own sink and holds
//! it to the house 8 blocks / 1 KiB budget. Still NOT measured here: the
//! read buffering inside `tokio-tungstenite`, upstream of `accept`.
//!
//! CI ENFORCEMENT: un-gated, house pattern; the Coverage lane skips it by the
//! `dhat_` name prefix. One profiler per process, so both arms share one test.
//!
//! Run: `cargo test -p tickvault-app --test dhat_wal_ring_sink_accept`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use metrics_exporter_prometheus::PrometheusBuilder;
use tickvault_app::dhan_feed_stack::{FRAME_RING_CAPACITY, FRAME_RING_MAX_BYTES};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::{
    CapturedFrame, FrameSink, FrameSinkOutcome, RingByteBudget, WalRingSink,
};
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType};

mod dhat_support;

/// House budget for 10,000 calls (same as `dhat_ws_lag_recorder.rs`).
const STEADY_BUDGET_BYTES: u64 = 1024;
const STEADY_BUDGET_BLOCKS: u64 = 8;

const STEADY_FRAMES: usize = 10_000;
const WARMUP_FRAMES: usize = 4_096;
const BACKLOG_FRAMES: usize = 4_096;
/// Frames sent into EACH shed arm per measured attempt.
const SHED_FRAMES: usize = 4_096;
/// Attempts `measure_with_phantom_retry` may make, so every attempt gets
/// fresh frames.
const MEASURE_ATTEMPTS: usize = 3;
/// A backlog of any depth is held to the same house budget as the steady
/// state since the ring became a pre-allocated array (2026-10-20, audit M1).
/// Under the old tokio `mpsc` it took one block per 32 queued frames.
const BACKLOG_BUDGET_BYTES: u64 = STEADY_BUDGET_BYTES;
const BACKLOG_BUDGET_BLOCKS: u64 = STEADY_BUDGET_BLOCKS;

fn temp_wal_dir(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "tv-dhat-wal-ring-sink-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp WAL dir"); // APPROVED: test
    p
}

/// A 16-byte Dhan ticker packet with a marker in the security-id field, in
/// the SHARED `Bytes` form tokio-tungstenite delivers (a throwaway clone
/// promotes it off-clock, exactly as the reader DHAT test does).
fn shared_ticker(marker: u32) -> Bytes {
    let mut buf = vec![0u8; 16];
    buf[0] = 2; // ticker
    buf[1..3].copy_from_slice(&16u16.to_le_bytes());
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&marker.to_le_bytes());
    buf[8..12].copy_from_slice(&100.25_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    let b = Bytes::from(buf);
    let _promoted = std::hint::black_box(b.clone());
    b
}

/// Blocks until the WAL writer has written `target` records. A sleep, never an
/// allocation, so it can sit inside a measured window.
fn wait_persisted(spill: &WsFrameSpill, target: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while spill.persisted_count() < target {
        assert!(
            Instant::now() < deadline,
            "WAL writer stalled at {} of {target} records",
            spill.persisted_count()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn dhat_wal_ring_sink_accept_allocates_nothing_in_steady_state_backlog_or_shed() {
    // The production recorder, BEFORE the sink resolves its counter handles.
    let recorder = PrometheusBuilder::new().build_recorder();
    metrics::set_global_recorder(recorder).expect("no recorder installed yet");

    let dir = temp_wal_dir("accept");
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("WsFrameSpill::new"));
    let (ring_tx, mut ring_rx) =
        tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(FRAME_RING_CAPACITY);
    // The boot site builds per-class budgets with `with_slot_cap`; the main
    // feed's share is smaller than this, but the code path is the same.
    let budget = Arc::new(RingByteBudget::with_slot_cap(
        FRAME_RING_MAX_BYTES,
        FRAME_RING_CAPACITY,
    ));
    let sink = WalRingSink::new(
        Arc::clone(&spill),
        ring_tx,
        Arc::clone(&budget),
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        0,
    );

    // Three shed sinks on the same WAL, each with its own channel and budget,
    // built before the profiler so their counter handles resolve off-clock.
    // Each forces one refusal arm of `accept`:
    //   bytes: an 8-byte budget refuses every 16-byte frame (BytesFull);
    //   slots: a 0-slot budget refuses every frame (SlotsFull);
    //   send:  a capacity-1 channel, filled once, refuses at `try_send`.
    let (bytes_tx, _bytes_rx) =
        tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(FRAME_RING_CAPACITY);
    let bytes_budget = Arc::new(RingByteBudget::with_slot_cap(8, usize::MAX));
    let bytes_sink = WalRingSink::new(
        Arc::clone(&spill),
        bytes_tx,
        Arc::clone(&bytes_budget),
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        1,
    );
    let (slots_tx, _slots_rx) =
        tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(FRAME_RING_CAPACITY);
    let slots_budget = Arc::new(RingByteBudget::with_slot_cap(FRAME_RING_MAX_BYTES, 0));
    let slots_sink = WalRingSink::new(
        Arc::clone(&spill),
        slots_tx,
        Arc::clone(&slots_budget),
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        2,
    );
    let (send_tx, _send_rx) = tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(1);
    let send_budget = Arc::new(RingByteBudget::with_slot_cap(
        FRAME_RING_MAX_BYTES,
        FRAME_RING_CAPACITY,
    ));
    let send_sink = WalRingSink::new(
        Arc::clone(&spill),
        send_tx,
        Arc::clone(&send_budget),
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        3,
    );

    let total = WARMUP_FRAMES
        + STEADY_FRAMES * MEASURE_ATTEMPTS
        + BACKLOG_FRAMES * 2
        + 4
        + SHED_FRAMES * 3 * MEASURE_ATTEMPTS;
    let frames: Vec<Bytes> = (0..total as u32).map(shared_ticker).collect();
    let mut next = 0usize;
    let mut persisted_target = 0u64;

    // Warm-up, outside every window: opens the first WAL segment, resolves
    // every gauge the writer publishes, settles the tokio channel's first
    // blocks, and registers the drain thread's epoch state.
    for frame in &frames[next..next + WARMUP_FRAMES] {
        let len = frame.len();
        assert_eq!(sink.accept(frame.clone()), FrameSinkOutcome::Captured);
        let got = ring_rx.try_recv().expect("frame in ring");
        assert_eq!(got.bytes.len(), len);
        budget.release(len);
    }
    // One shed per arm off-clock, and the one frame that fills the capacity-1
    // channel: warms the frame-fate table, the applied watermark and each
    // sink's counters.
    assert_eq!(
        bytes_sink.accept(frames[next + WARMUP_FRAMES].clone()),
        FrameSinkOutcome::RingFull
    );
    assert_eq!(
        slots_sink.accept(frames[next + WARMUP_FRAMES + 1].clone()),
        FrameSinkOutcome::RingFull
    );
    assert_eq!(
        send_sink.accept(frames[next + WARMUP_FRAMES + 2].clone()),
        FrameSinkOutcome::Captured
    );
    assert_eq!(
        send_sink.accept(frames[next + WARMUP_FRAMES + 3].clone()),
        FrameSinkOutcome::RingFull
    );
    next += WARMUP_FRAMES + 4;
    persisted_target += WARMUP_FRAMES as u64 + 4;
    wait_persisted(&spill, persisted_target);

    let _profiler = dhat::Profiler::builder().testing().build();

    // ---- Arm A: steady state. A fresh slice of frames per attempt, so a
    // retry measures the same work on new markers.
    let (s_bytes, s_blocks) = dhat_support::measure_with_phantom_retry(
        STEADY_BUDGET_BYTES,
        STEADY_BUDGET_BLOCKS,
        || {},
        || {
            for frame in &frames[next..next + STEADY_FRAMES] {
                let len = frame.len();
                let outcome = sink.accept(frame.clone());
                assert_eq!(outcome, FrameSinkOutcome::Captured);
                let got = ring_rx.try_recv();
                assert!(got.is_some(), "frame must be in the ring");
                drop(got);
                budget.release(len);
            }
            next += STEADY_FRAMES;
            persisted_target += STEADY_FRAMES as u64;
            wait_persisted(&spill, persisted_target);
        },
    );
    eprintln!(
        "dhat_wal_ring_sink_accept: steady state {s_bytes} B / {s_blocks} blocks over \
         {STEADY_FRAMES} frames"
    );
    assert!(
        s_bytes <= STEADY_BUDGET_BYTES && s_blocks <= STEADY_BUDGET_BLOCKS,
        "WalRingSink::accept allocated {s_bytes} B / {s_blocks} blocks over {STEADY_FRAMES} \
         frames with the drain keeping up (budget {STEADY_BUDGET_BYTES} B / \
         {STEADY_BUDGET_BLOCKS} blocks). Something on the per-frame path now allocates: a \
         `metrics::counter!` macro with a label (resolve the handle at construction), a \
         `to_vec()` of the frame, or per-record work on the WAL writer thread."
    );

    // ---- Arm B: backlog. No consumer, so the ring holds every frame.
    let backlog = &frames[next..next + BACKLOG_FRAMES];
    let before = dhat::HeapStats::get();
    for frame in backlog {
        assert_eq!(sink.accept(frame.clone()), FrameSinkOutcome::Captured);
    }
    let after = dhat::HeapStats::get();
    let b_blocks = after.total_blocks.saturating_sub(before.total_blocks);
    let b_bytes = after.total_bytes.saturating_sub(before.total_bytes);
    eprintln!(
        "dhat_wal_ring_sink_accept: backlog of {BACKLOG_FRAMES} frames allocated {b_bytes} B / \
         {b_blocks} blocks"
    );
    assert!(
        b_bytes <= BACKLOG_BUDGET_BYTES && b_blocks <= BACKLOG_BUDGET_BLOCKS,
        "a ring backlog of {BACKLOG_FRAMES} frames allocated {b_bytes} B / {b_blocks} blocks \
         (budget {BACKLOG_BUDGET_BYTES} B / {BACKLOG_BUDGET_BLOCKS} blocks). The frame ring is a \
         pre-allocated array; a growable channel (tokio mpsc) on this path allocates per 32 \
         queued frames on the socket read task"
    );

    // Drain the backlog so the ring and the budget end empty.
    for frame in backlog {
        let got = ring_rx.try_recv().expect("backlog frame in ring");
        assert_eq!(got.bytes, *frame, "ring must hand frames back in order");
        budget.release(frame.len());
    }
    assert_eq!(budget.resident(), 0, "every reservation released");
    persisted_target += BACKLOG_FRAMES as u64;
    wait_persisted(&spill, persisted_target);
    next += BACKLOG_FRAMES;

    // ---- Arm B2: re-backlog after a full drain. The old tokio ring freed
    // all but about 3 drained blocks, so a second backlog allocated again;
    // the array ring reuses its slots and must allocate nothing.
    let rebacklog = &frames[next..next + BACKLOG_FRAMES];
    let before = dhat::HeapStats::get();
    for frame in rebacklog {
        assert_eq!(sink.accept(frame.clone()), FrameSinkOutcome::Captured);
    }
    let after = dhat::HeapStats::get();
    let r_blocks = after.total_blocks.saturating_sub(before.total_blocks);
    let r_bytes = after.total_bytes.saturating_sub(before.total_bytes);
    eprintln!(
        "dhat_wal_ring_sink_accept: re-backlog of {BACKLOG_FRAMES} frames after a full drain \
         allocated {r_bytes} B / {r_blocks} blocks"
    );
    assert!(
        r_bytes <= BACKLOG_BUDGET_BYTES && r_blocks <= BACKLOG_BUDGET_BLOCKS,
        "a re-backlog of {BACKLOG_FRAMES} frames allocated {r_bytes} B / {r_blocks} blocks \
         (budget {BACKLOG_BUDGET_BYTES} B / {BACKLOG_BUDGET_BLOCKS} blocks)"
    );
    for frame in rebacklog {
        let got = ring_rx.try_recv().expect("re-backlog frame in ring");
        assert_eq!(got.bytes, *frame, "ring must hand frames back in order");
        budget.release(frame.len());
    }
    assert_eq!(budget.resident(), 0, "every reservation released");
    persisted_target += BACKLOG_FRAMES as u64;
    wait_persisted(&spill, persisted_target);
    next += BACKLOG_FRAMES;

    // ---- Arm C: the three shed arms. Each frame is still written to the WAL
    // (an Arc bump) and marked unapplied and shed; nothing may allocate.
    for (name, shed_sink) in [
        ("bytes_full", &bytes_sink),
        ("slots_full", &slots_sink),
        ("try_send_full", &send_sink),
    ] {
        let (c_bytes, c_blocks) = dhat_support::measure_with_phantom_retry(
            STEADY_BUDGET_BYTES,
            STEADY_BUDGET_BLOCKS,
            || {},
            || {
                for frame in &frames[next..next + SHED_FRAMES] {
                    let outcome = shed_sink.accept(frame.clone());
                    assert_eq!(outcome, FrameSinkOutcome::RingFull);
                }
                next += SHED_FRAMES;
                persisted_target += SHED_FRAMES as u64;
                wait_persisted(&spill, persisted_target);
            },
        );
        eprintln!(
            "dhat_wal_ring_sink_accept: shed arm {name}: {c_bytes} B / {c_blocks} blocks over \
             {SHED_FRAMES} refused frames"
        );
        assert!(
            c_bytes <= STEADY_BUDGET_BYTES && c_blocks <= STEADY_BUDGET_BLOCKS,
            "the {name} shed arm of WalRingSink::accept allocated {c_bytes} B / {c_blocks} \
             blocks over {SHED_FRAMES} frames (budget {STEADY_BUDGET_BYTES} B / \
             {STEADY_BUDGET_BLOCKS} blocks)"
        );
    }
    assert_eq!(bytes_budget.resident(), 0, "BytesFull hands its slot back");
    assert_eq!(slots_budget.resident(), 0, "SlotsFull reserves nothing");
    assert_eq!(
        send_budget.resident(),
        16,
        "a refused try_send releases its reservation; only the filling frame is held"
    );

    assert_eq!(spill.shutdown(Duration::from_secs(10)), 0);
    let _ = std::fs::remove_dir_all(&dir);
}
