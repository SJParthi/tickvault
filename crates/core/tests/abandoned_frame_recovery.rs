//! The watermark and frame-fate ledger live for the process. This test binary
//! owns them so its observations are independent of the library's feed tests.

use std::time::Instant;

use bytes::Bytes;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::wal_applied_watermark::applied_watermark;
use tickvault_storage::wal_frame_fate::{LostFate, ShedKind, frame_fate};
use tickvault_storage::ws_frame_spill::{WsType, next_frame_seq};

#[test]
fn test_note_abandoned_marks_only_wal_backed_frames_for_recovery() {
    let mut frame = CapturedFrame {
        seq: next_frame_seq(),
        endpoint: DhanEndpointType::MainFeed,
        connection_index: 0,
        received_at: Instant::now(),
        received_at_nanos: 0,
        wal_backed: false,
        bytes: Bytes::from_static(b"frame"),
    };
    let unbacked = frame.seq;
    frame.note_abandoned(WsType::LiveFeed);
    // The watermark groups nearby frame IDs in one bucket. Check the empty
    // bucket before a legitimately backed frame marks it.
    assert!(
        !applied_watermark()
            .snapshot()
            .range_has_unapplied(unbacked, unbacked)
    );

    assert_eq!(
        frame_fate().note_lost(unbacked, WsType::LiveFeed),
        LostFate::NotShed
    );

    frame.seq = next_frame_seq();
    frame.wal_backed = true;
    let backed = frame.seq;
    frame.note_abandoned(WsType::LiveFeed);

    assert!(
        applied_watermark()
            .snapshot()
            .range_has_unapplied(backed, backed)
    );
    assert_eq!(
        frame_fate().note_lost(backed, WsType::LiveFeed),
        LostFate::WasShed(ShedKind::Ring)
    );
}
