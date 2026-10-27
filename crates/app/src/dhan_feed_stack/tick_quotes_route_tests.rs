//! Plan item M5 (2026-10-27, owner Quote 41): the drain routes a main-feed
//! packet with no new trade to `tick_quotes` and every other packet to
//! `ticks`, the same way live and on a WAL replay. A sibling file, to keep
//! `dhan_feed_stack.rs` under its size pin.

use super::*;
use proptest::prelude::*;
use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, QUOTE_PACKET_SIZE, RESPONSE_CODE_QUOTE};
use tickvault_common::feed::Feed;

/// 2026-10-27 10:00:00 IST, IST epoch seconds.
const TEN_AM_IST: u32 = 1_793_095_200;
/// 09:15:00 IST the same day.
const OPEN_IST: u32 = TEN_AM_IST - 45 * 60;

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

/// One packet of a generated stream: key, trade time, cumulative volume, and
/// receipt (UTC nanos).
#[derive(Debug, Clone, Copy)]
struct Packet {
    security_id: u32,
    ltt: u32,
    volume: u32,
    receipt_utc_nanos: i64,
}

/// Which table a packet's row went to, read from the writer's counters around
/// one ingest: `None` when it reached neither or both.
fn table_of(before: (usize, u64), after: (usize, u64)) -> Option<bool> {
    let rows = after.0.checked_sub(before.0)?;
    let quotes = after.1.checked_sub(before.1)?;
    match (rows, quotes) {
        (1, 1) => Some(true),
        (1, 0) => Some(false),
        _ => None,
    }
}

fn counts(ingest: &LiveIngest) -> (usize, u64) {
    (ingest.writer.pending(), ingest.writer.quote_rows_appended())
}

fn frame_seq(i: usize) -> u64 {
    (1_u64 << 40) + ((i as u64) << 17)
}

/// Routes each packet through the LIVE drain (`drain_main_feed_frame`).
fn route_live(packets: &[Packet]) -> Vec<Option<bool>> {
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    packets
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let before = counts(&ingest);
            let _out = drain_main_feed_frame(
                &mut ingest,
                &CapturedFrame {
                    seq: frame_seq(i),
                    endpoint: DhanEndpointType::MainFeed,
                    connection_index: 0,
                    received_at: std::time::Instant::now(),
                    received_at_nanos: p.receipt_utc_nanos,
                    wal_backed: true,
                    bytes: quote_packet(p.security_id, p.ltt, p.volume).into(),
                },
                p.receipt_utc_nanos,
                1_000 + i as u64,
                counters(),
            );
            table_of(before, counts(&ingest))
        })
        .collect()
}

/// Routes each packet through the WAL REPLAY (`refold_wal_frames`).
fn route_replay(packets: &[Packet]) -> Vec<Option<bool>> {
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    packets
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let before = counts(&ingest);
            let frame = (
                frame_seq(i),
                p.receipt_utc_nanos,
                WalEndpoint::MainFeed,
                bytes::Bytes::from(quote_packet(p.security_id, p.ltt, p.volume)),
            );
            let _out = refold_wal_frames(&mut ingest, &[frame], &[], None);
            table_of(before, counts(&ingest))
        })
        .collect()
}

/// The owner's rule, written independently of the detector: no new trade =
/// same trade time and cumulative as the last packet of the key, or a first
/// packet whose trade time is before 09:15 IST.
fn expected_routes(packets: &[Packet]) -> Vec<Option<bool>> {
    let mut last: std::collections::HashMap<u32, (u32, u32)> = std::collections::HashMap::new();
    packets
        .iter()
        .map(|p| {
            let quote = match last.insert(p.security_id, (p.ltt, p.volume)) {
                Some(prev) => prev == (p.ltt, p.volume),
                None => p.ltt < OPEN_IST,
            };
            Some(quote)
        })
        .collect()
}

fn receipt(ist_secs: u32) -> i64 {
    (i64::from(ist_secs) - i64::from(IST_UTC_OFFSET_SECONDS)) * 1_000_000_000
}

/// Regression (2026-10-27, plan item M5): a repeat of the last trade goes to
/// `tick_quotes`; the trade itself and the next new trade go to `ticks`.
/// Before the split every packet went to `ticks`.
#[test]
fn test_regression_drain_routes_a_repeat_quote_to_tick_quotes() {
    let at = |s: u32| receipt(TEN_AM_IST + s);
    let packets = [
        Packet {
            security_id: 52_175,
            ltt: TEN_AM_IST,
            volume: 4_500,
            receipt_utc_nanos: at(0),
        },
        Packet {
            security_id: 52_175,
            ltt: TEN_AM_IST,
            volume: 4_500,
            receipt_utc_nanos: at(30),
        },
        Packet {
            security_id: 52_175,
            ltt: TEN_AM_IST,
            volume: 4_500,
            receipt_utc_nanos: at(60),
        },
        Packet {
            security_id: 52_175,
            ltt: TEN_AM_IST + 61,
            volume: 4_575,
            receipt_utc_nanos: at(61),
        },
    ];
    let live = route_live(&packets);
    assert_eq!(live, [Some(false), Some(true), Some(true), Some(false)]);
}

/// A first packet carrying a pre-open trade time goes to `tick_quotes`; a
/// first packet with an in-session trade time goes to `ticks`.
#[test]
fn test_first_packet_routes_by_trade_time_against_the_session_open() {
    let packets = [
        Packet {
            security_id: 1,
            ltt: OPEN_IST - 300,
            volume: 10,
            receipt_utc_nanos: receipt(TEN_AM_IST),
        },
        Packet {
            security_id: 2,
            ltt: TEN_AM_IST - 1,
            volume: 10,
            receipt_utc_nanos: receipt(TEN_AM_IST),
        },
    ];
    assert_eq!(route_live(&packets), [Some(true), Some(false)]);
}

/// The same packets in the same order reach the same table live and on a WAL
/// replay.
#[test]
fn test_replay_routes_like_live() {
    let at = |s: u32| receipt(TEN_AM_IST + s);
    let packets = [
        Packet {
            security_id: 7,
            ltt: OPEN_IST - 60,
            volume: 0,
            receipt_utc_nanos: at(0),
        },
        Packet {
            security_id: 7,
            ltt: OPEN_IST - 60,
            volume: 0,
            receipt_utc_nanos: at(1),
        },
        Packet {
            security_id: 8,
            ltt: TEN_AM_IST,
            volume: 5,
            receipt_utc_nanos: at(2),
        },
        Packet {
            security_id: 7,
            ltt: TEN_AM_IST + 3,
            volume: 25,
            receipt_utc_nanos: at(3),
        },
        Packet {
            security_id: 8,
            ltt: TEN_AM_IST,
            volume: 5,
            receipt_utc_nanos: at(4),
        },
    ];
    let live = route_live(&packets);
    assert_eq!(live, expected_routes(&packets));
    assert_eq!(route_replay(&packets), live);
}

/// One generated step: which of three keys, and what moved since its last packet.
fn packet_stream() -> impl Strategy<Value = Vec<Packet>> {
    let first = proptest::collection::vec(any::<bool>(), 3);
    let steps = proptest::collection::vec((0_u32..3, 0_u8..4, 1_u32..4, 1_u32..200), 1..60);
    (first, steps).prop_map(|(pre_open, steps)| {
        let mut state: Vec<(u32, u32)> = pre_open
            .iter()
            .map(|p| (if *p { OPEN_IST - 600 } else { TEN_AM_IST }, 1_000))
            .collect();
        let mut out = Vec::with_capacity(steps.len());
        for (i, (key, change, dt, dv)) in steps.into_iter().enumerate() {
            let k = key as usize;
            let (ltt, vol) = state[k];
            let next = match change {
                0 => (ltt, vol),                           // repeat quote
                1 => (TEN_AM_IST.max(ltt) + dt, vol + dv), // new trade
                2 => (TEN_AM_IST.max(ltt) + dt, vol),      // trade time moved only
                _ => (ltt, vol + dv),                      // volume moved only
            };
            state[k] = next;
            out.push(Packet {
                security_id: 40_000 + key,
                ltt: next.0,
                volume: next.1,
                // Receipt never before the trade time, one second per step.
                receipt_utc_nanos: receipt(TEN_AM_IST + 600 + i as u32),
            });
        }
        out
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Every packet lands in EXACTLY one of `ticks` and `tick_quotes`, never
    /// both and never neither, live and on replay; both match the owner's
    /// rule packet by packet.
    #[test]
    fn proptest_every_packet_lands_in_exactly_one_table_live_and_on_replay(
        packets in packet_stream()
    ) {
        let expected = expected_routes(&packets);
        let live = route_live(&packets);
        prop_assert!(live.iter().all(Option::is_some), "live: {live:?}");
        prop_assert_eq!(&live, &expected);
        let replay = route_replay(&packets);
        prop_assert!(replay.iter().all(Option::is_some), "replay: {replay:?}");
        prop_assert_eq!(&replay, &live);
    }
}
