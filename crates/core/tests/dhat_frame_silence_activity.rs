//! DHAT allocation test for the per-frame silence evidence (scope lock
//! 2026-10-10, "PER-KIND FRAME-SILENCE THRESHOLDS").
//!
//! Separate binary because DHAT allows only one profiler per process.
//! Every frame on every socket passes through `ConnEvent::FrameReceived`,
//! which now also publishes the slot's last-frame wall time (at most once a
//! second) and tracks the inter-frame gap peak; every socket polls once a
//! second. Both must stay allocation-free with the evidence opted in and the
//! fast path acting.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod dhat_support;

use std::time::{Duration, Instant};

use tickvault_core::websocket::pool_budget::{ConnectionSlot, DhanAccount, DhanEndpointType};
use tickvault_core::websocket::pool_supervisor::{
    ConnEvent, ConnectionSupervisor, FrameSilenceFastPath, SiblingEvidence,
};

/// Frames per measured round: 10,000 frames, 1 ms apart (10 s of feed, so
/// the once-a-second publish fires ten times).
const FRAMES: u64 = 10_000;
/// One poll every 10 frames: 1,000 polls per round.
const FRAMES_PER_POLL: u64 = 10;

#[test]
fn dhat_frame_silence_activity_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let start = Instant::now();
    let slot = ConnectionSlot {
        account: DhanAccount::Primary,
        endpoint: DhanEndpointType::MainFeed,
        pool_index: 0,
        global_index: 0,
    };
    let mut supervisor = ConnectionSupervisor::new(slot, start);
    supervisor.set_silence_evidence(SiblingEvidence::Pool);
    supervisor.set_fast_path(FrameSilenceFastPath::Act);
    supervisor.set_held_instruments(5_000);
    let _ = supervisor.on_event(ConnEvent::BeginDial, start);
    let _ = supervisor.on_event(ConnEvent::DialSucceeded, start);
    let _ = supervisor.on_event(ConnEvent::SubscribeAcked, start);
    // Warm up: the first frame publishes the register for the first time.
    let _ = supervisor.on_event(ConnEvent::FrameReceived, start);

    let mut clock = start;
    let (_, blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for frame in 1..=FRAMES {
                clock += Duration::from_millis(1);
                let _ = supervisor.on_event(ConnEvent::FrameReceived, clock);
                if frame % FRAMES_PER_POLL == 0 {
                    let _ = supervisor.poll(clock);
                }
            }
        },
    );

    // The measured path really ran the register: the slot carries a stamp.
    assert!(
        tickvault_core::websocket::pool_supervisor::last_frame_wall_ms(0).is_some(),
        "the frame activity register was never published"
    );
    assert_eq!(
        blocks, 0,
        "FrameReceived + poll allocated {blocks} blocks over {FRAMES} frames — \
         the frame-silence evidence must stay allocation-free (Principle #1)"
    );
}
