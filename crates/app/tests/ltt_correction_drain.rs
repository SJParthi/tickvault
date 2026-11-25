//! The live drain feeds the Dhan trade-time reversion guard (scope lock
//! 2026-11-21, "DHAN LIVE LTT IS UTC EPOCH FROM RECEIPT 2026-11-20 00:00 IST";
//! noise lock §2.24-a). UTC-shaped index prints (the normal state since 20 Nov)
//! never page; IST-shaped prints (Dhan reverted) report once per segment per
//! IST day. The guard never changes the correction, which the dispatcher
//! applies from the receipt alone.

use tickvault_app::dhan_feed_stack::{LiveIngest, counters, drain_main_feed_frame};
use tickvault_common::config::LttUtcAutocorrect;
use tickvault_common::feed::Feed;
use tickvault_core::parser::{ParsedFrame, dispatch_frame};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::ltt_correction::LTT_UTC_OFFSET_SECS;
use tickvault_storage::tick_persistence::TickWriter;

/// 2026-11-20 09:30:00 IST as UTC epoch seconds.
const START_UTC_SECS: i64 = 1_795_152_600 - 90 * 60;

/// An index ticker packet (code 1, segment IDX_I) whose trade time is `raw`.
fn index_ticker(security_id: u32, raw: u32) -> Vec<u8> {
    let mut p = vec![0u8; 16];
    p[0] = 1;
    p[1..3].copy_from_slice(&16u16.to_le_bytes());
    p[3] = 0;
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[8..12].copy_from_slice(&25_000.5f32.to_le_bytes());
    p[12..16].copy_from_slice(&raw.to_le_bytes());
    p
}

/// A live frame of five index ticks, each carrying trade time `raw`.
fn frame(seq: u64, raw: u32, receipt_nanos: i64) -> CapturedFrame {
    let mut bytes = Vec::with_capacity(80);
    for sid in [13u32, 25, 51, 21, 27] {
        bytes.extend_from_slice(&index_ticker(sid, raw));
    }
    CapturedFrame {
        seq,
        endpoint: DhanEndpointType::MainFeed,
        connection_index: 0,
        received_at: std::time::Instant::now(),
        received_at_nanos: receipt_nanos,
        wal_backed: true,
        bytes: bytes.into(),
    }
}

fn drive(ingest: &mut LiveIngest, start_secs: i64, frames: i64, ist_shape: bool) {
    let c = counters();
    for n in 0..frames {
        let secs = start_secs + n;
        let receipt = secs * 1_000_000_000;
        let raw = if ist_shape {
            (secs + i64::from(LTT_UTC_OFFSET_SECS)) as u32
        } else {
            secs as u32
        };
        let _ = drain_main_feed_frame(ingest, &frame(n as u64, raw, receipt), receipt, 1_000, c);
    }
}

#[test]
fn test_regression_2026_11_20_utc_index_prints_never_page_and_are_corrected() {
    // Regression: 2026-11-20 — every trade time arrived as plain UTC epoch.
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_ltt_correction(LttUtcAutocorrect::Act, None);
    drive(&mut ingest, START_UTC_SECS, 60, false);
    let counts = ingest.ltt_controller().counts();
    assert_eq!(counts.votes, 300);
    assert_eq!(counts.utc_verdicts, 3);
    assert_eq!(counts.reversion_reports, 0);
    // The dispatcher corrects the very first packet, no warm-up.
    let receipt = START_UTC_SECS * 1_000_000_000;
    match dispatch_frame(&index_ticker(13, START_UTC_SECS as u32), receipt) {
        Ok(ParsedFrame::Tick(t)) => {
            assert_eq!(
                t.exchange_timestamp,
                START_UTC_SECS as u32 + LTT_UTC_OFFSET_SECS
            );
            assert_eq!(i64::from(t.exchange_timestamp) % 86_400, 9 * 3600 + 30 * 60);
        }
        other => panic!("not a tick: {other:?}"),
    }
}

#[test]
fn test_regression_ist_epoch_index_prints_page_the_reversion_guard_once() {
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_ltt_correction(LttUtcAutocorrect::Act, None);
    drive(&mut ingest, START_UTC_SECS, 80, true);
    let counts = ingest.ltt_controller().counts();
    assert_eq!(counts.normal_verdicts, 4);
    assert_eq!(counts.reversion_reports, 1, "once per segment per day");
}

#[test]
fn test_frames_without_a_wal_stamp_or_from_replay_never_vote() {
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_ltt_correction(LttUtcAutocorrect::Act, None);
    let c = counters();
    for n in 0..40i64 {
        let secs = START_UTC_SECS + n;
        let receipt = secs * 1_000_000_000;
        let raw = (secs + i64::from(LTT_UTC_OFFSET_SECS)) as u32;
        // No WAL receipt stamp: the drain's fallback receipt is passed, the
        // frame's own stamp is unknown.
        let unstamped = frame(n as u64, raw, 0);
        let _ = drain_main_feed_frame(&mut ingest, &unstamped, receipt, 1_000, c);
        // A replayed frame (connection index u8::MAX).
        let mut replayed = frame(n as u64, raw, receipt);
        replayed.connection_index = u8::MAX;
        let _ = drain_main_feed_frame(&mut ingest, &replayed, receipt, 1_000, c);
    }
    let counts = ingest.ltt_controller().counts();
    assert_eq!(counts.votes, 0);
    assert_eq!(counts.reversion_reports, 0);
}
