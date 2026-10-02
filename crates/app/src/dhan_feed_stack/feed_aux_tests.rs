//! Item 45h (2026-10-02): every received non-tick packet class, the prior-day
//! connect snapshot and the post-close tick are persisted to
//! `feed_aux_packets`, never to `ticks` and never to a candle.
//!
//! The row CONTENT is pinned on the pure builder (`aux_row_for_non_tick`),
//! which both the live drain and the WAL replay call with the frame's own
//! sequence, packet index and receipt; the drain tests then pin that each arm
//! actually appends, and the candle test pins that the fold never sees them.

use super::*;
use tickvault_storage::feed_aux_persistence::AuxPacketKind;
use tickvault_trading::candles::LiveCandleState;
use tickvault_trading::candles::tf_index::TfIndex;

/// 2026-05-22 10:00:00 IST, in IST epoch seconds (Dhan's LTT convention).
const LTT_1000_IST: u32 = 1_779_321_600 + 10 * 3_600;

/// The receipt matching an IST epoch second, in UTC nanoseconds.
fn receipt_utc_nanos(ist_secs: u32) -> i64 {
    (i64::from(ist_secs) - i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS))
        * 1_000_000_000
}

fn ingest() -> LiveIngest {
    LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
}

fn header(code: u8, len: usize, segment: u8, security_id: u32) -> Vec<u8> {
    let mut p = vec![0u8; len];
    p[0] = code;
    p[1..3].copy_from_slice(&u16::try_from(len).unwrap_or(0).to_le_bytes());
    p[3] = segment;
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p
}

fn oi_packet(security_id: u32, oi: u32) -> Vec<u8> {
    let mut p = header(
        tickvault_common::constants::RESPONSE_CODE_OI,
        tickvault_common::constants::OI_PACKET_SIZE,
        2, // NSE_FNO
        security_id,
    );
    p[8..12].copy_from_slice(&oi.to_le_bytes());
    p
}

fn prev_close_packet(security_id: u32, close: f32, prev_oi: u32) -> Vec<u8> {
    let mut p = header(
        tickvault_common::constants::RESPONSE_CODE_PREVIOUS_CLOSE,
        tickvault_common::constants::PREVIOUS_CLOSE_PACKET_SIZE,
        0, // IDX_I
        security_id,
    );
    p[8..12].copy_from_slice(&close.to_le_bytes());
    p[12..16].copy_from_slice(&prev_oi.to_le_bytes());
    p
}

fn market_status_packet(security_id: u32) -> Vec<u8> {
    header(
        tickvault_common::constants::RESPONSE_CODE_MARKET_STATUS,
        tickvault_common::constants::MARKET_STATUS_PACKET_SIZE,
        1, // NSE_EQ
        security_id,
    )
}

fn disconnect_packet(code: u16) -> Vec<u8> {
    let mut p = header(
        tickvault_common::constants::RESPONSE_CODE_DISCONNECT,
        tickvault_common::constants::DISCONNECT_PACKET_SIZE,
        0,
        0,
    );
    p[8..10].copy_from_slice(&code.to_le_bytes());
    p
}

fn ticker_packet(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
    let mut p = header(
        tickvault_common::constants::RESPONSE_CODE_TICKER,
        tickvault_common::constants::TICKER_PACKET_SIZE,
        0, // IDX_I
        security_id,
    );
    p[8..12].copy_from_slice(&ltp.to_le_bytes());
    p[12..16].copy_from_slice(&ltt.to_le_bytes());
    p
}

/// Builds the row for one packet exactly as the drain does.
fn row_for(packet: &[u8], frame_seq: u64, index: u32, receipt: i64) -> AuxPacketRow {
    let parsed = dispatch_frame(packet, receipt).expect("packet must parse");
    aux_row_for_non_tick(&parsed, packet, frame_seq, index, receipt)
        .expect("sequence fits")
        .expect("a non-tick packet builds a row")
}

fn drain(ingest: &mut LiveIngest, seq: u64, bytes: Vec<u8>, receipt: i64) -> FrameOutcome {
    drain_main_feed_frame(
        ingest,
        &CapturedFrame {
            seq,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at: std::time::Instant::now(),
            received_at_nanos: receipt,
            bytes: bytes.into(),
        },
        receipt,
        1_000,
        counters(),
    )
}

/// The capture sequence a packet at `index` of frame `seq` must carry.
fn seq_of(frame_seq: u64, index: u32) -> i64 {
    tickvault_storage::ws_frame_spill::packet_capture_seq(frame_seq, u64::from(index))
        .and_then(capture_seq_from_frame_seq)
        .expect("fits")
}

#[test]
fn test_aux_row_for_non_tick_open_interest_code_5() {
    let receipt = receipt_utc_nanos(LTT_1000_IST);
    let row = row_for(&oi_packet(52_175, 1_234_500), 9_000, 3, receipt);
    assert_eq!(row.kind, AuxPacketKind::OpenInterest);
    assert_eq!((row.security_id, row.segment), (52_175, "NSE_FNO"));
    assert_eq!(row.oi, Some(1_234_500));
    assert_eq!(row.capture_seq, seq_of(9_000, 3));
    let ist = receipt + tickvault_common::constants::IST_UTC_OFFSET_NANOS;
    assert_eq!(row.ts_ist_nanos, ist, "designated ts is the receipt in IST");
    assert_eq!(row.received_at_ist_nanos, Some(ist));
    // A zero OI is the packet's whole payload and is written as received.
    let zero = row_for(&oi_packet(52_175, 0), 9_000, 3, receipt);
    assert_eq!(zero.oi, Some(0));
}

#[test]
fn test_aux_row_for_non_tick_prev_close_code_6_carries_prev_oi() {
    let receipt = receipt_utc_nanos(LTT_1000_IST);
    let row = row_for(&prev_close_packet(13, 23_090.35, 4_200), 9_001, 0, receipt);
    assert_eq!(row.kind, AuxPacketKind::PrevClose);
    assert_eq!((row.security_id, row.segment), (13, "IDX_I"));
    assert_eq!(row.prev_close, Some(23_090.35), "clean 2-dp widening");
    assert_eq!(row.prev_oi, Some(4_200));
    assert_eq!(row.capture_seq, seq_of(9_001, 0));
}

#[test]
fn test_aux_row_for_non_tick_market_status_code_7() {
    let receipt = receipt_utc_nanos(LTT_1000_IST);
    let row = row_for(&market_status_packet(2_885), 9_002, 1, receipt);
    assert_eq!(row.kind, AuxPacketKind::MarketStatus);
    assert_eq!((row.security_id, row.segment), (2_885, "NSE_EQ"));
    assert_eq!(row.capture_seq, seq_of(9_002, 1));
    assert_eq!(
        (row.oi, row.prev_close, row.reason_code),
        (None, None, None)
    );
}

#[test]
fn test_aux_row_for_non_tick_disconnect_code_50_keeps_the_reason() {
    let receipt = receipt_utc_nanos(LTT_1000_IST);
    let row = row_for(&disconnect_packet(805), 9_003, 0, receipt);
    assert_eq!(row.kind, AuxPacketKind::Disconnect);
    assert_eq!(row.reason_code, Some(805));
    assert_eq!(row.capture_seq, seq_of(9_003, 0));
}

#[test]
fn test_aux_row_for_non_tick_skips_a_tick_and_refuses_an_unfit_sequence() {
    let receipt = receipt_utc_nanos(LTT_1000_IST);
    let tick = ticker_packet(13, 100.0, LTT_1000_IST);
    let parsed = dispatch_frame(&tick, receipt).expect("parse");
    assert_eq!(
        aux_row_for_non_tick(&parsed, &tick, 1, 0, receipt),
        Ok(None),
        "a tick is the fold's, never an aux row"
    );
    let oi = oi_packet(1, 1);
    let parsed = dispatch_frame(&oi, receipt).expect("parse");
    assert_eq!(
        aux_row_for_non_tick(&parsed, &oi, u64::MAX, 0, receipt),
        Err(AuxSeqRefused),
        "an unrepresentable sequence is refused, never re-minted"
    );
}

/// The live drain appends one row per non-tick packet class, alongside the
/// existing counters and the previous-close store write.
#[test]
fn ingest_non_tick_at_the_live_drain_appends_one_row_per_non_tick_packet() {
    let receipt = receipt_utc_nanos(LTT_1000_IST);
    let mut bytes = oi_packet(52_175, 10);
    bytes.extend_from_slice(&prev_close_packet(13, 23_090.35, 4_200));
    bytes.extend_from_slice(&market_status_packet(2_885));
    bytes.extend_from_slice(&disconnect_packet(805));
    let mut ingest = ingest();
    let out = drain(&mut ingest, 42, bytes, receipt);
    assert_eq!(out.disconnects, 1);
    assert_eq!(out.folded, 0, "no packet here is a tick");
    assert_eq!(ingest.writer.aux_rows_appended(), 4, "one row per packet");
    assert_eq!(ingest.writer.pending(), 4);
    assert_eq!(ingest.pending_rows(), 4, "the size trigger sees them");
    assert_eq!(
        ingest.prev_close().tracked(),
        1,
        "the previous-close store write is unchanged"
    );
}

/// A WAL replay of the same frame appends the same number of rows, and the
/// rows it builds are byte-for-byte the live ones: same `(frame_seq, packet
/// index)`, same receipt, same builder — so the DEDUP key collapses them.
#[test]
fn ingest_non_tick_at_a_replay_reproduces_the_live_rows() {
    let receipt = receipt_utc_nanos(LTT_1000_IST);
    let packets = [
        oi_packet(52_175, 10),
        prev_close_packet(13, 23_090.35, 4_200),
        market_status_packet(2_885),
        disconnect_packet(807),
    ];
    let bytes: Vec<u8> = packets.iter().flatten().copied().collect();

    let mut live = ingest();
    let _ = drain(&mut live, 77, bytes.clone(), receipt);
    let mut replay = ingest();
    let frames = vec![(
        77u64,
        receipt,
        WalEndpoint::MainFeed,
        bytes::Bytes::from(bytes),
    )];
    let out = refold_wal_frames(&mut replay, &frames, &[]);
    assert_eq!(out.non_tick, 4);
    assert_eq!(
        live.writer.aux_rows_appended(),
        replay.writer.aux_rows_appended(),
        "the replay must re-offer every row the live drain wrote"
    );
    assert_eq!(replay.writer.aux_rows_appended(), 4);

    // The rows themselves: index i of frame 77 at the same receipt.
    for (i, p) in packets.iter().enumerate() {
        let index = u32::try_from(i).unwrap_or(0);
        let a = row_for(p, 77, index, receipt);
        let b = row_for(p, 77, index, receipt);
        assert_eq!(a, b);
        assert_eq!(a.capture_seq, seq_of(77, index), "distinct per packet");
    }

    // Both arms pass the frame's own sequence, index and receipt.
    let src = include_str!("../dhan_feed_stack.rs");
    let prod = src
        .split_once("#[cfg(test)]\nmod tests")
        .map_or(src, |(p, _)| p);
    assert!(
        prod.contains(
            "frame.seq,\n                    packets,\n                    received_at_nanos,"
        ),
        "the live arms must pass the frame's own sequence, index and receipt"
    );
    assert!(
        prod.contains("*frame_seq,\n                        packets,\n                        *wal_received_at_nanos,"),
        "the replay arm must pass the record's own sequence, index and receipt"
    );
}

/// The prior-day connect snapshot never reaches `ticks`: the one buffered row
/// is the aux row, and the fold is untouched.
#[test]
fn a_prior_day_connect_snapshot_is_kept_in_feed_aux_and_never_reaches_ticks() {
    let receipt = receipt_utc_nanos(LTT_1000_IST);
    let yesterday = LTT_1000_IST - 86_400;
    let mut ingest = ingest();
    let out = drain(
        &mut ingest,
        5,
        ticker_packet(66_422, 142.5, yesterday),
        receipt,
    );
    assert_eq!(out.folded, 0, "no `ticks` row");
    assert_eq!(
        (ingest.writer.pending(), ingest.writer.aux_rows_appended()),
        (1, 1),
        "the one buffered row is the aux row"
    );
    assert!(
        sealed(&mut ingest).is_empty(),
        "the snapshot folded into no candle"
    );
}

/// A tick stamped after 15:40 IST is refused by the `ticks` window and
/// WRITTEN to `feed_aux_packets` (before item 45h it was dropped).
#[test]
fn a_tick_after_1540_is_written_to_feed_aux() {
    let late = 1_779_321_600 + 15 * 3_600 + 45 * 60; // 15:45:00 IST
    let receipt = receipt_utc_nanos(late);
    let mut ingest = ingest();
    let _ = drain(&mut ingest, 6, ticker_packet(13, 23_100.0, late), receipt);
    assert_eq!(
        ingest.writer.aux_rows_appended(),
        1,
        "kept as out_of_window_tick"
    );
    assert_eq!(ingest.writer.pending(), 1, "and not as a `ticks` row too");
}

fn sealed(ingest: &mut LiveIngest) -> Vec<(u64, u8, TfIndex, LiveCandleState)> {
    let mut out = Vec::new();
    let _ = ingest
        .aggregator
        .force_seal_all(|_feed, sid, seg, tf, state| out.push((sid, seg, tf, state)));
    out
}

/// The candle is unchanged: the same tick with and without the non-tick
/// packets for the SAME instrument folds to identical bars on every frame.
#[test]
fn non_tick_packets_leave_every_candle_unchanged() {
    let receipt = receipt_utc_nanos(LTT_1000_IST);
    let tick = ticker_packet(13, 23_146.45, LTT_1000_IST);

    let mut baseline = ingest();
    let _ = drain(&mut baseline, 1, tick.clone(), receipt);

    let mut with_aux = ingest();
    let mut bytes = tick;
    bytes.extend_from_slice(&prev_close_packet(13, 23_090.35, 4_200));
    bytes.extend_from_slice(&market_status_packet(13));
    let mut oi = oi_packet(13, 99);
    oi[3] = 0; // same instrument, IDX_I
    bytes.extend_from_slice(&oi);
    let _ = drain(&mut with_aux, 1, bytes, receipt);
    assert_eq!(with_aux.writer.aux_rows_appended(), 3);

    for tf in TfIndex::ALL {
        assert_eq!(
            baseline
                .aggregator
                .snapshot(Feed::Dhan, 13, 0, tf)
                .map(|s| format!("{s:?}")),
            with_aux
                .aggregator
                .snapshot(Feed::Dhan, 13, 0, tf)
                .map(|s| format!("{s:?}")),
            "open bar differs on {tf:?}"
        );
    }
    let a = sealed(&mut baseline);
    let b = sealed(&mut with_aux);
    assert!(!a.is_empty(), "the baseline tick must actually fold");
    assert_eq!(a, b, "every sealed bar must be identical");
}
