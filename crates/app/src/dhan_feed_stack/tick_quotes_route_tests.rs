//! Plan item M5 (2026-10-27, owner Quotes 41 and 43): the drain routes a
//! main-feed packet whose trade time is more than 2 s before its WAL receipt
//! to `tick_quotes`, every other packet to `ticks`, and a WAL replay writes a
//! byte-identical row to the same table. A sibling file, to keep
//! `dhan_feed_stack.rs` under its size pin.

use super::*;
use proptest::prelude::*;
use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, QUOTE_PACKET_SIZE, RESPONSE_CODE_QUOTE};
use tickvault_common::feed::Feed;
use tickvault_storage::tick_persistence::FlushBatch;

/// 2026-10-28 10:00:00 IST, IST epoch seconds (the first routed day).
const TEN_AM_IST: u32 = 1_793_181_600;

/// One Quote-mode (code 4) packet on NSE_FNO.
fn quote_packet(security_id: u32, ltt_ist_secs: u32, volume: u32) -> Vec<u8> {
    let mut p = vec![0u8; QUOTE_PACKET_SIZE];
    p[0] = RESPONSE_CODE_QUOTE;
    p[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
    p[3] = 2; // NSE_FNO
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[8..12].copy_from_slice(&101.5_f32.to_le_bytes());
    p[12..14].copy_from_slice(&75_u16.to_le_bytes());
    p[14..18].copy_from_slice(&ltt_ist_secs.to_le_bytes());
    p[18..22].copy_from_slice(&100.25_f32.to_le_bytes());
    p[22..26].copy_from_slice(&volume.to_le_bytes());
    p[26..30].copy_from_slice(&9_000_u32.to_le_bytes());
    p[30..34].copy_from_slice(&10_000_u32.to_le_bytes());
    p[34..38].copy_from_slice(&99.0_f32.to_le_bytes());
    p[38..42].copy_from_slice(&98.0_f32.to_le_bytes());
    p[42..46].copy_from_slice(&104.0_f32.to_le_bytes());
    p[46..50].copy_from_slice(&97.5_f32.to_le_bytes());
    p
}

/// One packet of a stream: key, trade time, cumulative volume, the WAL
/// record's receipt (UTC nanos) and the frame sequence it was captured with.
#[derive(Debug, Clone, Copy)]
struct Packet {
    security_id: u32,
    ltt: u32,
    volume: u32,
    receipt_utc_nanos: i64,
    frame_seq: u64,
}

/// The table one packet's row went to (`true` = `tick_quotes`) and the row's
/// exact ILP text; `None` when it reached neither table or both.
type Row = Option<(bool, Vec<u8>)>;

/// UTC nanos of an IST receipt `secs` after 10:00:00 IST, plus `sub` nanos.
fn receipt(secs: u32, sub: i64) -> i64 {
    (i64::from(TEN_AM_IST + secs) - i64::from(IST_UTC_OFFSET_SECONDS)) * 1_000_000_000 + sub
}

fn frame_seq(i: usize) -> u64 {
    (1_u64 << 40) + ((i as u64) << 17)
}

fn offload_ingest() -> (LiveIngest, std::sync::mpsc::Receiver<FlushBatch>) {
    let (producer, sink, rx) = TickWriter::for_test(Feed::Dhan).split_for_offload();
    // The sink is never run: each packet's batch is read off the queue.
    drop(sink);
    (LiveIngest::new(producer, 8), rx)
}

/// Flushes what one packet appended and reads its row back.
fn take_row(
    ingest: &mut LiveIngest,
    rx: &std::sync::mpsc::Receiver<FlushBatch>,
    quotes_before: u64,
) -> Row {
    let quotes = ingest
        .writer
        .quote_rows_appended()
        .checked_sub(quotes_before)?;
    ingest.writer.flush().ok()?;
    let batch = rx.try_recv().ok()?;
    if batch.rows() != 1 || rx.try_recv().is_ok() {
        return None;
    }
    let text = batch.ilp_bytes().to_vec();
    let table = match quotes {
        1 if text.starts_with(b"tick_quotes,") => true,
        0 if text.starts_with(b"ticks,") => false,
        _ => return None,
    };
    Some((table, text))
}

/// Routes each packet through the LIVE drain (`drain_main_feed_frame`).
fn route_live(packets: &[Packet]) -> Vec<Row> {
    let (mut ingest, rx) = offload_ingest();
    packets
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let before = ingest.writer.quote_rows_appended();
            let _out = drain_main_feed_frame(
                &mut ingest,
                &CapturedFrame {
                    seq: p.frame_seq,
                    endpoint: DhanEndpointType::MainFeed,
                    connection_index: 0,
                    received_at: std::time::Instant::now(),
                    received_at_nanos: p.receipt_utc_nanos,
                    wal_backed: true,
                    bytes: quote_packet(p.security_id, p.ltt, p.volume).into(),
                },
                // What the drain passes for a WAL-stamped frame; an unknown
                // stamp gets a wall-clock reading instead (see below).
                if p.receipt_utc_nanos == 0 {
                    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
                } else {
                    p.receipt_utc_nanos
                },
                1_000 + i as u64,
                counters(),
            );
            take_row(&mut ingest, &rx, before)
        })
        .collect()
}

/// Routes each packet through the WAL REPLAY (`refold_wal_frames`).
fn route_replay(packets: &[Packet]) -> Vec<Row> {
    let (mut ingest, rx) = offload_ingest();
    packets
        .iter()
        .map(|p| {
            let before = ingest.writer.quote_rows_appended();
            let frame = (
                p.frame_seq,
                p.receipt_utc_nanos,
                WalEndpoint::MainFeed,
                bytes::Bytes::from(quote_packet(p.security_id, p.ltt, p.volume)),
            );
            let _out = refold_wal_frames(&mut ingest, &[frame], &[], None);
            take_row(&mut ingest, &rx, before)
        })
        .collect()
}

fn packet(i: usize, security_id: u32, age_secs: u32) -> Packet {
    let at = 60 + i as u32;
    Packet {
        security_id,
        ltt: TEN_AM_IST + at - age_secs,
        volume: 4_500 + i as u32,
        receipt_utc_nanos: receipt(at, 250_000_000),
        frame_seq: frame_seq(i),
    }
}

/// Regression (2026-10-27, plan item M5, Quote 43): a packet whose trade time
/// is more than 2 s before its receipt goes to `tick_quotes`; 2 s or less
/// stays in `ticks`. Before the split every packet went to `ticks`.
#[test]
fn test_regression_drain_routes_a_stale_trade_time_to_tick_quotes() {
    let packets = [
        packet(0, 52_175, 0),
        packet(1, 52_175, 2),
        packet(2, 52_175, 3),
        packet(3, 52_175, 3_600),
    ];
    let tables: Vec<Option<bool>> = route_live(&packets)
        .into_iter()
        .map(|r| r.map(|(t, _)| t))
        .collect();
    assert_eq!(tables, [Some(false), Some(false), Some(true), Some(true)]);
}

/// Regression (2026-10-27, coordinator follow-up): the `tick_quotes` row's
/// `ts` is the WAL receipt, never a reading taken at processing time. The
/// same packet and WAL record processed live and then on a replay (at a
/// later wall time) give a byte-identical ILP row, so the DEDUP key collapses
/// the replayed copy.
#[test]
fn test_regression_tick_quotes_ts_is_identical_live_and_on_replay() {
    let packets = [packet(0, 52_175, 600)];
    let live = route_live(&packets);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let replay = route_replay(&packets);
    assert!(
        matches!(live[0], Some((true, _))),
        "the stale packet must reach tick_quotes live: {live:?}"
    );
    let row = live[0].as_ref().map(|(_, r)| r.clone()).unwrap_or_default();
    let arrival_ist =
        packets[0].receipt_utc_nanos + i64::from(IST_UTC_OFFSET_SECONDS) * 1_000_000_000;
    assert!(
        String::from_utf8_lossy(&row)
            .trim_end()
            .ends_with(&format!(" {arrival_ist}")),
        "ts must be the WAL receipt in IST"
    );
    assert_eq!(replay, live, "the replayed row must be byte-identical");
}

/// A frame the WAL recorded with no receipt stamp gets a wall-clock reading
/// live, which a replay cannot reproduce, so its packets stay in `ticks` both
/// ways (and `ticks` keys on the trade time, so the copies still collapse).
#[test]
fn test_unknown_wal_receipt_stays_in_ticks_live_and_on_replay() {
    let mut p = packet(0, 52_175, 3_600);
    p.receipt_utc_nanos = 0;
    let live = route_live(&[p]);
    let replay = route_replay(&[p]);
    // Live substitutes the WALL clock for the unknown receipt, so whether the
    // writer's session gate keeps the row depends on the real date the test
    // runs on (a trade dated 2026-10-28 against a 2026-10-27 wall clock is a
    // future trade and is refused). The property under test is the TABLE:
    // whatever is written, live or on replay, is never a `tick_quotes` row.
    assert!(!matches!(live[0], Some((true, _))), "{live:?}");
    assert!(matches!(replay[0], Some((false, _))), "{replay:?}");
}

/// Mixed stream: same table and the same bytes for every packet.
#[test]
fn test_replay_routes_like_live() {
    let packets = [
        packet(0, 7, 3_600),
        packet(1, 7, 1),
        packet(2, 8, 3),
        packet(3, 7, 2),
        packet(4, 8, 2_000),
    ];
    let live = route_live(&packets);
    assert!(live.iter().all(Option::is_some), "{live:?}");
    assert_eq!(route_replay(&packets), live);
}

/// A generated stream: several contracts interleaved, each packet with a
/// trade time from 10 s before to 5 s after its receipt, receipts rising.
fn packet_stream() -> impl Strategy<Value = Vec<Packet>> {
    proptest::collection::vec((0_u32..4, -10_i64..6, 0_i64..1_000_000_000), 1..48).prop_map(
        |steps| {
            steps
                .into_iter()
                .enumerate()
                .map(|(i, (key, rel, sub))| {
                    let at = 60 + i as u32;
                    let ltt = i64::from(TEN_AM_IST + at) + rel;
                    Packet {
                        security_id: 40_000 + key,
                        ltt: u32::try_from(ltt).unwrap_or(TEN_AM_IST),
                        volume: 1_000 + i as u32,
                        receipt_utc_nanos: receipt(at, sub),
                        frame_seq: frame_seq(i),
                    }
                })
                .collect()
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Every packet lands in EXACTLY one of `ticks` and `tick_quotes`, never
    /// both and never neither; the table is exactly Quote 43's rule; and a
    /// replay that starts at ANY point of the stream (a restart part-way
    /// through) writes the same table and the same bytes for every packet it
    /// reads.
    #[test]
    fn proptest_every_packet_lands_in_exactly_one_table_live_and_on_replay(
        packets in packet_stream(),
        restart in 0_usize..48,
    ) {
        let live = route_live(&packets);
        for (p, row) in packets.iter().zip(&live) {
            let receipt_ist_secs = (p.receipt_utc_nanos
                + i64::from(IST_UTC_OFFSET_SECONDS) * 1_000_000_000)
                .div_euclid(1_000_000_000);
            let expected = receipt_ist_secs - i64::from(p.ltt)
                > crate::tick_quote_route::TICK_QUOTES_STALE_TRADE_SECS;
            prop_assert_eq!(row.as_ref().map(|(t, _)| *t), Some(expected), "{:?}", p);
        }
        let from = restart.min(packets.len());
        let replay = route_replay(&packets[from..]);
        prop_assert_eq!(&replay[..], &live[from..]);
    }
}
