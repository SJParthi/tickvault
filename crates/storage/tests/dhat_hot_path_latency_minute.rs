//! DHAT zero-allocation ratchet for the per-minute latency proof and the
//! per-minute host capacity row (owner, 2026-10-30).
//!
//! The `tv-telemetry` thread runs this window once a second, forever: one
//! `publish_once`, the minute accumulator's `absorb`, the scheduling read
//! (`SchedPublisher::publish`) and its per-second fold into the host
//! sampler, and, once a minute, the host row's `/proc` reads, the day
//! verdict and `offer` into the pre-allocated hand-off queue. None of it is
//! on the frame path, but an allocation here would be a slow leak on a thread
//! nobody watches, so it is held to zero. The recording path
//! (`record_stage_nanos`, `record_receipt_to_fold_nanos`) runs inside the
//! window too, as the drain and the readers do concurrently in production.
//!
//! What is NOT measured: resolving the handles and building the samplers
//! (once, at boot), installing the queue (once), each scheduling series'
//! first metric handle (once per series, the warm-up below), the verdict's
//! `reasons` text (built on the writer thread), and the writer thread (no
//! receiver runs here, so the queue fills and the full arm is measured).

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::time::Instant;

use tickvault_storage::host_capacity_minute::{DayVerdict, HostSampler};
use tickvault_storage::hot_path_latency_minute::{
    LATENCY_MINUTE_QUEUE_MINUTES, MinuteAccumulator, OfferOutcome, ProofMessage, install_sink,
    offer,
};
use tickvault_storage::hot_path_telemetry::{
    ALL_STAGES, Stage, TelemetryHandles, publish_once, record_receipt_to_fold_nanos,
    record_stage_nanos,
};
use tickvault_storage::hot_thread_sched::{
    SchedPublisher, register_main_worker_thread, release_main_worker_thread,
};

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;

const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// One telemetry-thread second: publish, minute absorb, scheduling read and
/// fold; at a minute close, the host row, the verdict and the offers.
struct Window {
    handles: TelemetryHandles,
    minute: MinuteAccumulator,
    sched: SchedPublisher,
    host: HostSampler,
    day: DayVerdict,
    verdicts: usize,
}

impl Window {
    fn second(&mut self, wall: i64, queued: &mut usize, full: &mut usize) {
        let _readings = publish_once(&mut self.handles, Instant::now());
        if let Some(rows) = self.minute.absorb(wall, self.handles.last_second()) {
            let host_row = self.host.close_minute(rows[0].minute_ist_nanos, wall);
            if let Some(v) = self.day.observe(&rows, &host_row) {
                self.verdicts += 1;
                count(offer(&ProofMessage::Verdict(v)), queued, full);
            }
            count(offer(&ProofMessage::Minute(rows, host_row)), queued, full);
        }
        let _read = self.sched.publish();
        self.host.absorb_second(&self.sched);
    }
}

fn count(outcome: OfferOutcome, queued: &mut usize, full: &mut usize) {
    match outcome {
        OfferOutcome::Queued => *queued += 1,
        OfferOutcome::QueueFull => *full += 1,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn dhat_publish_window_with_minute_accumulator_is_zero_allocation() {
    let rx = install_sink(LATENCY_MINUTE_QUEUE_MINUTES).expect("first install");
    // This thread stands in for a main-runtime worker, so the scheduling
    // read has a thread to read (on Linux) inside the window.
    let _slot = register_main_worker_thread();
    let mut w = Window {
        handles: TelemetryHandles::resolve(),
        minute: MinuteAccumulator::new(),
        sched: SchedPublisher::new(),
        host: HostSampler::new(),
        day: DayVerdict::new(),
        verdicts: 0,
    };
    // Warm up: touch every static, resolve each scheduling series' handle
    // and run one full minute close.
    // 15:00 IST: the window crosses the last session minutes, the 15:30
    // close and the 15:40 verdict, so every arm of the day verdict runs.
    let mut wall: i64 = 1_793_026_800 * NANOS_PER_SECOND;
    record_stage_nanos(Stage::SocketToWal, 1);
    record_receipt_to_fold_nanos(1);
    let (mut queued, mut full) = (0usize, 0usize);
    for _ in 0..2 {
        w.second(wall, &mut queued, &mut full);
        wall += 60 * NANOS_PER_SECOND;
    }
    w.second(wall, &mut queued, &mut full);
    assert!(queued >= 1, "the warm-up closed a minute");

    let _profiler = dhat::Profiler::builder().testing().build();

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            // 100 minutes of one-second publishes: enough closed minutes to
            // fill the 64-item queue and exercise the full arm.
            for second in 0..6_000_i64 {
                for i in 0..20_u64 {
                    let stage = ALL_STAGES[(i as usize) % ALL_STAGES.len()];
                    let nanos = if i == 7 { 2_000_000 } else { 300 + i * 40 };
                    record_stage_nanos(stage, nanos);
                    record_receipt_to_fold_nanos(nanos);
                }
                wall += NANOS_PER_SECOND;
                // A one-second backward step every 13 s: no row reopens.
                let at = if second % 13 == 0 {
                    wall - NANOS_PER_SECOND
                } else {
                    wall
                };
                w.second(at, &mut queued, &mut full);
            }
        },
    );
    assert_eq!(
        (bytes, blocks),
        (0, 0),
        "the telemetry thread's per-second window allocated (publish_once / \
         MinuteAccumulator::absorb / SchedPublisher::publish / HostSampler / \
         DayVerdict::observe / offer)"
    );
    assert!(
        queued >= LATENCY_MINUTE_QUEUE_MINUTES,
        "the queue was filled: {queued}"
    );
    assert!(full > 0, "the full arm was measured: {full}");
    assert_eq!(
        w.verdicts, 1,
        "the 15:40 verdict was built inside the window"
    );
    // What the writer would read: whole minutes, one latency row per stage
    // and the host row for the same minute.
    match rx.try_recv().expect("a queued item") {
        ProofMessage::Minute(rows, host) => {
            assert_eq!(rows.len(), ALL_STAGES.len());
            for (row, stage) in rows.iter().zip(ALL_STAGES.iter()) {
                assert_eq!(row.stage, *stage);
                assert_eq!(row.minute_ist_nanos, host.minute_ist_nanos);
            }
        }
        ProofMessage::Verdict(_) => panic!("the first item is the warm-up minute"),
    }
    release_main_worker_thread();
}
