//! DHAT zero-allocation ratchet for the per-minute latency proof
//! (owner, 2026-10-30).
//!
//! The `tv-telemetry` thread runs this window once a second, forever: one
//! `publish_once`, the minute accumulator's `absorb`, and, once a minute,
//! `offer` into the pre-allocated hand-off queue. None of it is on the frame
//! path, but an allocation here would be a slow leak on a thread nobody
//! watches, so it is held to zero. The recording path (`record_stage_nanos`,
//! `record_receipt_to_fold_nanos`) runs inside the window too, as the drain
//! and the readers do concurrently in production.
//!
//! What is NOT measured: resolving the handles (once, at boot), installing
//! the queue (once), and the writer thread (it receives on another thread;
//! no receiver runs here, so the queue fills and the full arm is measured).

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::time::Instant;

use tickvault_storage::hot_path_latency_minute::{
    LATENCY_MINUTE_QUEUE_MINUTES, MinuteAccumulator, OfferOutcome, install_sink, offer,
};
use tickvault_storage::hot_path_telemetry::{
    ALL_STAGES, Stage, TelemetryHandles, publish_once, record_receipt_to_fold_nanos,
    record_stage_nanos,
};

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;

const NANOS_PER_SECOND: i64 = 1_000_000_000;

#[test]
fn dhat_publish_window_with_minute_accumulator_is_zero_allocation() {
    let rx = install_sink(LATENCY_MINUTE_QUEUE_MINUTES).expect("first install");
    let mut handles = TelemetryHandles::resolve();
    let mut minute = MinuteAccumulator::new();
    // Warm up: touch every static and run one full publish and minute close.
    let mut wall: i64 = 1_793_000_000 * NANOS_PER_SECOND;
    record_stage_nanos(Stage::SocketToWal, 1);
    record_receipt_to_fold_nanos(1);
    let _ = publish_once(&mut handles, Instant::now());
    let _ = minute.absorb(wall, handles.last_second());
    wall += 60 * NANOS_PER_SECOND;
    let warm = minute
        .absorb(wall, handles.last_second())
        .expect("warm-up minute closed");
    assert_eq!(offer(&warm), OfferOutcome::Queued);

    let _profiler = dhat::Profiler::builder().testing().build();

    let mut queued = 1usize;
    let mut full = 0usize;
    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            // 100 minutes of one-second publishes: enough closed minutes to
            // fill the 64-minute queue and exercise the full arm.
            for second in 0..6_000_i64 {
                for i in 0..20_u64 {
                    let stage = ALL_STAGES[(i as usize) % ALL_STAGES.len()];
                    let nanos = if i == 7 { 2_000_000 } else { 300 + i * 40 };
                    record_stage_nanos(stage, nanos);
                    record_receipt_to_fold_nanos(nanos);
                }
                let _readings = publish_once(&mut handles, Instant::now());
                wall += NANOS_PER_SECOND;
                // A one-second backward step every 13 s: no row reopens.
                let at = if second % 13 == 0 {
                    wall - NANOS_PER_SECOND
                } else {
                    wall
                };
                if let Some(rows) = minute.absorb(at, handles.last_second()) {
                    match offer(&rows) {
                        OfferOutcome::Queued => queued += 1,
                        OfferOutcome::QueueFull => full += 1,
                        other => panic!("unexpected {other:?}"),
                    }
                }
            }
        },
    );
    assert_eq!(
        (bytes, blocks),
        (0, 0),
        "publish_once / MinuteAccumulator::absorb / offer allocated on the telemetry thread"
    );
    assert!(
        queued >= LATENCY_MINUTE_QUEUE_MINUTES,
        "the queue was filled: {queued}"
    );
    assert!(full > 0, "the full arm was measured: {full}");
    // What the writer would read: whole minutes of rows, one per stage.
    let first = rx.try_recv().expect("a queued minute");
    assert_eq!(first.len(), ALL_STAGES.len());
    for (row, stage) in first.iter().zip(ALL_STAGES.iter()) {
        assert_eq!(row.stage, *stage);
    }
}
