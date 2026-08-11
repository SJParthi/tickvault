//! DHAT zero-allocation ratchet for the Dhan DEPTH-feed binary parsers.
//!
//! # Why this file exists (the gap it closes)
//!
//! `crates/core/tests/dhat_allocation.rs` and `dhat_ws_reader_zero_alloc.rs`
//! already pin the MAIN-feed decode path, but both drive it exclusively
//! through `dispatch_frame()`. The depth-20 / depth-200 parsers are
//! **unreachable from `dispatch_frame`** — `parser/mod.rs` states it
//! verbatim:
//!
//! > They are parsed by `depth.rs` via `split_depth_frame()` +
//! > `dispatch_depth_packet()` — NEVER by `dispatch_frame()`, whose 8-byte
//! > header would mis-read every field.
//!
//! So `depth.rs` (re-added 2026-08-09 for the authorized live-WebSocket
//! revival, 1,534 LoC) carried a documented zero-allocation claim with NO
//! DHAT test behind it. Its own module header asserts:
//!
//! > O(1) per level, zero heap allocation. [...] writing into a
//! > caller-owned `DepthLevelBuffer` that the connection task allocates once
//! > and reuses for the life of the socket.
//!
//! An unguarded zero-alloc claim on a revival hot path is exactly what
//! Principle #1 forbids, so this file mechanically enforces it.
//!
//! # What is measured
//!
//! 1. `split_depth_frame()` — the stacked-packet iterator. Items borrow the
//!    input frame; a regression to `Vec<DepthPacket>` would allocate.
//! 2. `parse_depth_packet()` for depth-20 (fixed 20 levels).
//! 3. `parse_depth_packet()` for depth-200 (row count from the header).
//! 4. The depth-feed disconnect packet (14 bytes, its own arm).
//! 5. A realistic stacked frame — `[I1 Bid][I1 Ask][I2 Bid][I2 Ask]` per
//!    `16-fmd:169` — driven end to end through the iterator.
//!
//! The `DepthLevelBuffer` is allocated ONCE before the measurement window,
//! exactly as the live connection task does, so any allocation inside the
//! window is a real per-packet regression.
//!
//! # One profiler per binary
//!
//! `dhat::Profiler` is process-global and PANICS if a second is built while
//! one is alive. Every assertion below therefore lives in a SINGLE `#[test]`
//! — the same consolidation `dhat_ws_reader_zero_alloc.rs` documents.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::hint::black_box;

use tickvault_common::constants::{
    DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID, DEEP_DEPTH_HEADER_SIZE,
    DEEP_DEPTH_LEVEL_SIZE, EXCHANGE_SEGMENT_NSE_FNO, RESPONSE_CODE_DISCONNECT, TWENTY_DEPTH_LEVELS,
};
use tickvault_core::parser::{
    DepthFeedKind, DepthLevelBuffer, DepthPayload, DepthSide, parse_depth_packet, split_depth_frame,
};

mod dhat_support;

/// Number of levels used for the depth-200 vector. Deliberately NOT 200 so
/// the header row-count path is exercised with a value the packet length must
/// actually follow (a hardcoded 200 would hide a row-count bug).
const DEPTH_200_ROWS: usize = 137;

/// Builds one depth packet: 12-byte header + `level_count` × 16-byte levels.
///
/// Header layout (0-based, per `depth.rs` module docs):
/// `msg_length(u16)@0 | feed_code(u8)@2 | exchange_segment(u8)@3 |
///  security_id(u32)@4 | sequence_or_row_count(u32)@8`
///
/// Level layout: `price(f64)@0 | quantity(u32)@8 | orders(u32)@12`.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test builder, constant offsets bounded by the allocated Vec
fn make_depth_packet(
    feed_code: u8,
    security_id: u32,
    level_count: usize,
    seq_or_rows: u32,
    base_price: f64,
) -> Vec<u8> {
    let total = DEEP_DEPTH_HEADER_SIZE + level_count * DEEP_DEPTH_LEVEL_SIZE;
    let mut buf = vec![0u8; total];

    buf[0..2].copy_from_slice(&(total as u16).to_le_bytes());
    buf[2] = feed_code;
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&seq_or_rows.to_le_bytes());

    for i in 0..level_count {
        let base = DEEP_DEPTH_HEADER_SIZE + i * DEEP_DEPTH_LEVEL_SIZE;
        // Distinct, finite, monotonically-stepping prices so the vacuity
        // assertions below can prove the levels were really decoded.
        let price = base_price + (i as f64) * 0.05;
        buf[base..base + 8].copy_from_slice(&price.to_le_bytes());
        buf[base + 8..base + 12].copy_from_slice(&((i as u32 + 1) * 25).to_le_bytes());
        buf[base + 12..base + 16].copy_from_slice(&((i as u32 + 1) * 3).to_le_bytes());
    }
    buf
}

/// Builds a depth-feed disconnect packet: 12-byte header + `u16` reason.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test builder, constant offsets bounded by the allocated Vec
fn make_depth_disconnect(security_id: u32, reason_code: u16) -> Vec<u8> {
    let total = DEEP_DEPTH_HEADER_SIZE + 2;
    let mut buf = vec![0u8; total];
    buf[0..2].copy_from_slice(&(total as u16).to_le_bytes());
    buf[2] = RESPONSE_CODE_DISCONNECT;
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[DEEP_DEPTH_HEADER_SIZE..total].copy_from_slice(&reason_code.to_le_bytes());
    buf
}

/// Zero-allocation verification for the depth-20 / depth-200 decode path.
///
/// Budget: EXACTLY 0 blocks. Every input packet and the reusable
/// `DepthLevelBuffer` are built BEFORE the measurement window, mirroring the
/// live connection task (which allocates one buffer per socket and reuses it
/// forever).
#[test]
fn dhat_depth_packet_decode_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // ---- Pre-allocate everything OUTSIDE the measured window ------------
    let mut buf = DepthLevelBuffer::new();

    let bid_20 = make_depth_packet(
        DEEP_DEPTH_FEED_CODE_BID,
        52432,
        TWENTY_DEPTH_LEVELS,
        7,
        24_500.10,
    );
    let ask_20 = make_depth_packet(
        DEEP_DEPTH_FEED_CODE_ASK,
        52432,
        TWENTY_DEPTH_LEVELS,
        8,
        24_501.25,
    );
    let bid_200 = make_depth_packet(
        DEEP_DEPTH_FEED_CODE_BID,
        1333,
        DEPTH_200_ROWS,
        DEPTH_200_ROWS as u32,
        1_450.75,
    );
    let disconnect = make_depth_disconnect(52432, 805);

    // A realistic STACKED frame per `16-fmd:169`:
    //   [I1 Bid][I1 Ask][I2 Bid][I2 Ask]
    let mut stacked: Vec<u8> = Vec::new();
    stacked.extend_from_slice(&bid_20);
    stacked.extend_from_slice(&ask_20);
    stacked.extend_from_slice(&make_depth_packet(
        DEEP_DEPTH_FEED_CODE_BID,
        99991,
        TWENTY_DEPTH_LEVELS,
        9,
        512.40,
    ));
    stacked.extend_from_slice(&make_depth_packet(
        DEEP_DEPTH_FEED_CODE_ASK,
        99991,
        TWENTY_DEPTH_LEVELS,
        10,
        512.90,
    ));

    // Warm up any one-time lazy init (counters, statics) off-clock so the
    // measured delta reflects only per-packet work.
    {
        let warm = parse_depth_packet(&bid_20, DepthFeedKind::Twenty, &mut buf);
        assert!(warm.is_ok(), "warm-up parse must succeed");
    }

    // Vacuity accumulators — proven non-trivial AFTER the window. A
    // zero-alloc test over a workload that decoded nothing passes trivially
    // and is worthless, so these must reach exact, hand-derived values.
    let mut packets_parsed: u32 = 0;
    let mut levels_seen: u64 = 0;
    let mut price_checksum: f64 = 0.0;
    let mut disconnects_seen: u32 = 0;
    let mut bid_sides: u32 = 0;
    let mut ask_sides: u32 = 0;

    // ---- Measurement window --------------------------------------------
    let (_, allocs_during) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        // `stabilize` is a no-op: the workload closure resets every
        // accumulator at its own top, so each retry attempt (see
        // dhat_support/mod.rs) re-derives exact per-attempt counts without a
        // second mutable borrow of the same locals.
        || {},
        || {
            packets_parsed = 0;
            levels_seen = 0;
            price_checksum = 0.0;
            disconnects_seen = 0;
            bid_sides = 0;
            ask_sides = 0;

            // 1. depth-20 bid.
            let pkt = parse_depth_packet(
                black_box(bid_20.as_slice()),
                black_box(DepthFeedKind::Twenty),
                &mut buf,
            )
            .expect("depth-20 bid must parse");
            assert_eq!(pkt.header.security_id, 52432);
            if let DepthPayload::Levels { side, levels } = pkt.payload {
                assert_eq!(side, DepthSide::Bid);
                levels_seen += levels.len() as u64;
                price_checksum += levels.first().map_or(0.0, |l| l.price);
                bid_sides += 1;
            } else {
                panic!("depth-20 bid must yield Levels");
            }
            packets_parsed += 1;

            // 2. depth-20 ask.
            let pkt = parse_depth_packet(
                black_box(ask_20.as_slice()),
                black_box(DepthFeedKind::Twenty),
                &mut buf,
            )
            .expect("depth-20 ask must parse");
            if let DepthPayload::Levels { side, levels } = pkt.payload {
                assert_eq!(side, DepthSide::Ask);
                levels_seen += levels.len() as u64;
                price_checksum += levels.first().map_or(0.0, |l| l.price);
                ask_sides += 1;
            } else {
                panic!("depth-20 ask must yield Levels");
            }
            packets_parsed += 1;

            // 3. depth-200 (row count driven from the header).
            let pkt = parse_depth_packet(
                black_box(bid_200.as_slice()),
                black_box(DepthFeedKind::TwoHundred),
                &mut buf,
            )
            .expect("depth-200 bid must parse");
            assert_eq!(pkt.header.security_id, 1333);
            if let DepthPayload::Levels { side, levels } = pkt.payload {
                assert_eq!(side, DepthSide::Bid);
                levels_seen += levels.len() as u64;
                price_checksum += levels.first().map_or(0.0, |l| l.price);
                bid_sides += 1;
            } else {
                panic!("depth-200 must yield Levels");
            }
            packets_parsed += 1;

            // 4. Disconnect packet (its own arm, no level walk).
            let pkt = parse_depth_packet(
                black_box(disconnect.as_slice()),
                black_box(DepthFeedKind::Twenty),
                &mut buf,
            )
            .expect("depth disconnect must parse");
            if let DepthPayload::Disconnect { reason_code } = pkt.payload {
                assert_eq!(reason_code, 805);
                disconnects_seen += 1;
            } else {
                panic!("disconnect packet must yield Disconnect");
            }
            packets_parsed += 1;

            // 5. Stacked frame — the real wire shape (`16-fmd:169`). Drives
            //    the borrowing iterator end to end.
            let mut iter = split_depth_frame(black_box(stacked.as_slice()), DepthFeedKind::Twenty);
            for raw_pkt in iter.by_ref() {
                let parsed = parse_depth_packet(raw_pkt, DepthFeedKind::Twenty, &mut buf)
                    .expect("stacked packet must parse");
                if let DepthPayload::Levels { side, levels } = parsed.payload {
                    levels_seen += levels.len() as u64;
                    price_checksum += levels.first().map_or(0.0, |l| l.price);
                    match side {
                        DepthSide::Bid => bid_sides += 1,
                        DepthSide::Ask => ask_sides += 1,
                    }
                }
                packets_parsed += 1;
            }
            black_box(iter.stop_reason());
        },
    );

    // ---- VACUITY: prove the workload really decoded ----------------------
    // 4 standalone packets + 4 stacked = 8.
    assert_eq!(
        packets_parsed, 8,
        "VACUITY FAILURE: expected 8 parsed depth packets (4 standalone + 4 \
         stacked), got {packets_parsed}. A zero-alloc result over a workload \
         that parsed nothing proves nothing."
    );
    // Levels: 20 (bid20) + 20 (ask20) + 137 (depth200) + 4 × 20 (stacked) = 257.
    let expected_levels =
        (TWENTY_DEPTH_LEVELS * 2 + DEPTH_200_ROWS + TWENTY_DEPTH_LEVELS * 4) as u64;
    assert_eq!(
        levels_seen, expected_levels,
        "VACUITY FAILURE: expected {expected_levels} decoded depth levels, got \
         {levels_seen} — the level walk did not run as expected."
    );
    assert_eq!(disconnects_seen, 1, "VACUITY: disconnect arm must have run");
    assert_eq!(bid_sides, 4, "VACUITY: expected 4 bid-side packets");
    assert_eq!(ask_sides, 3, "VACUITY: expected 3 ask-side packets");
    // Prices must be real decoded f64s, not zeros from an untouched buffer.
    assert!(
        price_checksum.is_finite() && price_checksum > 1_000.0,
        "VACUITY FAILURE: decoded best-price checksum was {price_checksum} — \
         the f64 price field was not actually read (a zeroed buffer sums to 0)."
    );

    // ---- The ratchet -----------------------------------------------------
    assert_eq!(
        allocs_during, 0,
        "Depth-feed decode allocated {allocs_during} blocks — PRINCIPLE #1 \
         VIOLATED.\n\
         `depth.rs` documents 'O(1) per level, zero heap allocation ... writing \
         into a caller-owned DepthLevelBuffer'. A non-zero count means the \
         decode path started allocating per packet — most likely levels being \
         collected into a Vec instead of written into the reusable buffer, a \
         String/format! in an error path being constructed on the happy path, \
         or `split_depth_frame` copying instead of borrowing the frame."
    );
}
