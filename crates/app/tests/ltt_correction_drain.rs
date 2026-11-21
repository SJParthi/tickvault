//! The live drain arms the Dhan trade-time correction (scope lock 2026-11-20,
//! "DHAN TRADE TIME SENT AS UTC") on raw index trade times read 5 h 30 min
//! early, only after the record writer acknowledges the intent, and from one
//! nanosecond past the last receipt it dispatched. Its own test binary,
//! because it publishes the process-wide correction table.

use tickvault_app::dhan_feed_stack::{LiveIngest, counters, drain_main_feed_frame};
use tickvault_common::config::LttUtcAutocorrect;
use tickvault_common::feed::Feed;
use tickvault_core::parser::{ParsedFrame, dispatch_frame};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::ltt_correction::{
    LTT_UTC_OFFSET_SECS, LttTable, RecordAck, RecordKind, RecordWriterHandle, current_table,
    install_table, receipt_ist_day,
};
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

/// A live frame of five index ticks, each carrying the 2026-11-20 shape (raw
/// trade time = the receipt's UTC second).
fn frame(seq: u64, utc_secs: i64, receipt_nanos: i64) -> CapturedFrame {
    let mut bytes = Vec::with_capacity(80);
    for sid in [13u32, 25, 51, 21, 27] {
        bytes.extend_from_slice(&index_ticker(sid, utc_secs as u32));
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

#[test]
fn test_regression_2026_11_20_the_live_drain_arms_after_a_durable_intent() {
    // Regression: 2026-11-20 — every trade time arrived 19,800 s early.
    install_table(LttTable::empty());
    let (handle, cmds, acks) = RecordWriterHandle::stub_pair();
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_ltt_correction(LttUtcAutocorrect::Act, Some(handle));
    let c = counters();
    let mut last_receipt = 0;
    for n in 0..20i64 {
        let secs = START_UTC_SECS + n;
        last_receipt = secs * 1_000_000_000;
        let _ = drain_main_feed_frame(
            &mut ingest,
            &frame(n as u64, secs, last_receipt),
            last_receipt,
            1_000,
            c,
        );
    }
    // The detector proposed an entry; nothing is armed until it is durable.
    let intent = cmds.try_recv().expect("an entry intent");
    assert_eq!(intent.line.kind, RecordKind::Entry);
    assert_eq!(ingest.ltt_controller().counts().armed, 0);
    acks.send(RecordAck {
        day: intent.day,
        id: intent.line.id,
        kind: intent.line.kind,
        durable: true,
    })
    .expect("ack");
    // The next frame applies it.
    let secs = START_UTC_SECS + 20;
    let receipt = secs * 1_000_000_000;
    let _ = drain_main_feed_frame(&mut ingest, &frame(20, secs, receipt), receipt, 1_000, c);
    assert_eq!(ingest.ltt_controller().counts().armed, 1);
    let commit = cmds.try_recv().expect("the armed line");
    assert_eq!(commit.line.kind, RecordKind::Armed);
    assert_eq!(
        commit.line.nanos,
        last_receipt + 1,
        "past the last dispatched receipt"
    );
    // The published table corrects what the dispatcher reads from then on.
    let table = current_table();
    let day = receipt_ist_day(receipt);
    assert_eq!(table.day(day).map(|s| s.entries().len()), Some(1));
    let packet = index_ticker(13, secs as u32);
    match dispatch_frame(&packet, receipt) {
        Ok(ParsedFrame::Tick(t)) => {
            assert_eq!(t.exchange_timestamp, secs as u32 + LTT_UTC_OFFSET_SECS);
            assert_eq!(
                i64::from(t.exchange_timestamp) % 86_400,
                9 * 3600 + 30 * 60 + 20
            );
        }
        other => panic!("not a tick: {other:?}"),
    }
    install_table(LttTable::empty());
}

#[test]
fn test_frames_without_a_wal_stamp_or_from_replay_never_vote() {
    let (handle, cmds, _acks) = RecordWriterHandle::stub_pair();
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_ltt_correction(LttUtcAutocorrect::Act, Some(handle));
    let c = counters();
    for n in 0..40i64 {
        let secs = START_UTC_SECS + n;
        let receipt = secs * 1_000_000_000;
        // No WAL receipt stamp: the drain's fallback receipt is passed, the
        // frame's own stamp is unknown.
        let unstamped = frame(n as u64, secs, 0);
        let _ = drain_main_feed_frame(&mut ingest, &unstamped, receipt, 1_000, c);
        // A replayed frame (connection index u8::MAX).
        let mut replayed = frame(n as u64, secs, receipt);
        replayed.connection_index = u8::MAX;
        let _ = drain_main_feed_frame(&mut ingest, &replayed, receipt, 1_000, c);
    }
    assert_eq!(ingest.ltt_controller().counts().votes, 0);
    assert!(cmds.try_recv().is_err(), "nothing proposed");
}
