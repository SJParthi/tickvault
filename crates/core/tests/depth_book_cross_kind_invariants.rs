//! Cross-kind depth agreement + property coverage for the depth parser.
//!
//! # Why this file exists (2026-08-25)
//!
//! This morning a live query proved the candle volumes were inflated ~9.2x:
//! the same trading day tiled five ways summed to 40.40B / 40.15B / 40.53B /
//! 41.22B / 4.37B. A fully green suite never noticed, because every test
//! asserted that each PART works and none asserted that the parts AGREE.
//!
//! The depth path carries the identical blind spot on two axes:
//!
//! 1. **Cross-kind.** Three depth kinds — d5 (inline in a Full main-feed
//!    packet), d20 and d200 (the separate depth sockets) — write ONE table
//!    distinguished only by a `depth_kind` discriminator. Nothing anywhere
//!    asserted that d5 levels 1-5 and d20 levels 1-5 for the same instrument
//!    and the same second AGREE. That is "the same day tiled five ways" on a
//!    different axis, on the largest payload in the system.
//! 2. **Property coverage.** `proptest_parser.rs` covers ticker, quote, oi,
//!    prev_close, full and disconnect. `parse_depth_packet`,
//!    `split_depth_frame` and `depth_packet_len` had NONE. The fuzz target
//!    proves no-panic, weekly — which is not correctness.
//!
//! The per-side ORDERING invariants (bids non-increasing, asks
//! non-decreasing, populated levels contiguous, book uncrossed) live in the
//! `depth.rs` test module, next to the golden vectors they guard.
//!
//! # Honest limits (do not delete)
//!
//! * **d5 is f32, d20/d200 are f64.** The two kinds cannot be compared with
//!   `==` in general — a price that is exact in f64 is not necessarily exact
//!   in f32. The agreement tests therefore use prices that are exactly
//!   representable in BOTH (dyadic rationals such as 100.5, 0.25 steps), and
//!   compare through an explicit widening. A cross-kind comparison over
//!   arbitrary rupee prices would need a tick-size tolerance, and any future
//!   RUNTIME comparator must carry one — asserting bit-equality on live d5 vs
//!   d20 would fire constantly and mean nothing.
//! * **d5 order counts are u16; d20/d200 are u32.** The fixtures stay inside
//!   `u16::MAX` so the comparison is meaningful rather than saturated.
//! * These tests compare CONSTRUCTED packets for the same logical book. They
//!   prove the two decoders agree on an identical input book; they do NOT
//!   prove the live vendor sends agreeing books on the two sockets. That is
//!   UNVERIFIED-LIVE and is exactly what a runtime cross-kind comparator
//!   would have to measure.

use proptest::prelude::*;
use tickvault_common::constants::{
    DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID, DEEP_DEPTH_HEADER_SIZE,
    DEEP_DEPTH_LEVEL_SIZE, DEPTH_LEVEL_OFFSET_ASK_ORDERS, DEPTH_LEVEL_OFFSET_ASK_PRICE,
    DEPTH_LEVEL_OFFSET_ASK_QTY, DEPTH_LEVEL_OFFSET_BID_ORDERS, DEPTH_LEVEL_OFFSET_BID_PRICE,
    DEPTH_LEVEL_OFFSET_BID_QTY, FULL_OFFSET_DEPTH_START, FULL_QUOTE_PACKET_SIZE,
    MARKET_DEPTH_LEVEL_SIZE, MARKET_DEPTH_LEVELS, RESPONSE_CODE_DISCONNECT, RESPONSE_CODE_FULL,
    TWENTY_DEPTH_LEVELS, TWO_HUNDRED_DEPTH_LEVELS,
};
use tickvault_common::tick_types::DeepDepthLevel;
use tickvault_core::parser::depth::{
    DEPTH_DISCONNECT_PACKET_SIZE, DepthFeedKind, DepthLevelBuffer, DepthPayload, DepthSide,
    depth_level_count, depth_packet_len, parse_depth_header, parse_depth_packet, split_depth_frame,
};
use tickvault_core::parser::full_packet::parse_full_packet;
use tickvault_core::parser::types::PacketHeader;

// ---------------------------------------------------------------------------
// One logical book, expressed once — then emitted as d5 and as d20
// ---------------------------------------------------------------------------

/// One price level of the shared source-of-truth book.
///
/// Prices are dyadic (`x.5`, `x.25`) so they are EXACT in both f32 and f64 —
/// see the honest-limits note in the module docs.
#[derive(Debug, Clone, Copy)]
struct BookLevel {
    bid_price: f64,
    bid_qty: u32,
    bid_orders: u16,
    ask_price: f64,
    ask_qty: u32,
    ask_orders: u16,
}

/// The shared book: 5 levels, bids descending from 100.50, asks ascending
/// from 100.75, all quarters — exact in f32 and f64 alike.
fn shared_book() -> [BookLevel; MARKET_DEPTH_LEVELS] {
    [
        BookLevel {
            bid_price: 100.50,
            bid_qty: 1000,
            bid_orders: 10,
            ask_price: 100.75,
            ask_qty: 500,
            ask_orders: 5,
        },
        BookLevel {
            bid_price: 100.25,
            bid_qty: 800,
            bid_orders: 8,
            ask_price: 101.00,
            ask_qty: 400,
            ask_orders: 4,
        },
        BookLevel {
            bid_price: 100.00,
            bid_qty: 600,
            bid_orders: 6,
            ask_price: 101.25,
            ask_qty: 300,
            ask_orders: 3,
        },
        BookLevel {
            bid_price: 99.75,
            bid_qty: 400,
            bid_orders: 4,
            ask_price: 101.50,
            ask_qty: 200,
            ask_orders: 2,
        },
        BookLevel {
            bid_price: 99.50,
            bid_qty: 200,
            bid_orders: 2,
            ask_price: 101.75,
            ask_qty: 100,
            ask_orders: 1,
        },
    ]
}

/// Emits the shared book as the 5 inline depth levels of a Full packet.
#[allow(clippy::arithmetic_side_effects)]
fn emit_as_full_packet(book: &[BookLevel; MARKET_DEPTH_LEVELS]) -> Vec<u8> {
    let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
    buf[0] = RESPONSE_CODE_FULL;
    buf[1..3].copy_from_slice(&(FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = 1; // NSE_EQ
    buf[4..8].copy_from_slice(&1333_u32.to_le_bytes());
    for (i, lvl) in book.iter().enumerate() {
        let base = FULL_OFFSET_DEPTH_START + i * MARKET_DEPTH_LEVEL_SIZE;
        buf[base + DEPTH_LEVEL_OFFSET_BID_QTY..base + DEPTH_LEVEL_OFFSET_BID_QTY + 4]
            .copy_from_slice(&lvl.bid_qty.to_le_bytes());
        buf[base + DEPTH_LEVEL_OFFSET_ASK_QTY..base + DEPTH_LEVEL_OFFSET_ASK_QTY + 4]
            .copy_from_slice(&lvl.ask_qty.to_le_bytes());
        buf[base + DEPTH_LEVEL_OFFSET_BID_ORDERS..base + DEPTH_LEVEL_OFFSET_BID_ORDERS + 2]
            .copy_from_slice(&lvl.bid_orders.to_le_bytes());
        buf[base + DEPTH_LEVEL_OFFSET_ASK_ORDERS..base + DEPTH_LEVEL_OFFSET_ASK_ORDERS + 2]
            .copy_from_slice(&lvl.ask_orders.to_le_bytes());
        buf[base + DEPTH_LEVEL_OFFSET_BID_PRICE..base + DEPTH_LEVEL_OFFSET_BID_PRICE + 4]
            .copy_from_slice(&(lvl.bid_price as f32).to_le_bytes());
        buf[base + DEPTH_LEVEL_OFFSET_ASK_PRICE..base + DEPTH_LEVEL_OFFSET_ASK_PRICE + 4]
            .copy_from_slice(&(lvl.ask_price as f32).to_le_bytes());
    }
    buf
}

/// Emits ONE side of the shared book as a depth-20 packet (levels 5..19 are
/// vendor zero-padding, exactly as the wire does).
#[allow(clippy::arithmetic_side_effects)]
fn emit_as_twenty_packet(book: &[BookLevel; MARKET_DEPTH_LEVELS], side: DepthSide) -> Vec<u8> {
    let total = DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;
    let mut buf = vec![0u8; total];
    buf[0..2].copy_from_slice(&(total as u16).to_le_bytes());
    buf[2] = side.as_feed_code();
    buf[3] = 1; // NSE_EQ
    buf[4..8].copy_from_slice(&1333_u32.to_le_bytes());
    buf[8..12].copy_from_slice(&0_u32.to_le_bytes());
    for (i, lvl) in book.iter().enumerate() {
        let base = DEEP_DEPTH_HEADER_SIZE + i * DEEP_DEPTH_LEVEL_SIZE;
        let (price, qty, orders) = match side {
            DepthSide::Bid => (lvl.bid_price, lvl.bid_qty, u32::from(lvl.bid_orders)),
            DepthSide::Ask => (lvl.ask_price, lvl.ask_qty, u32::from(lvl.ask_orders)),
        };
        buf[base..base + 8].copy_from_slice(&price.to_le_bytes());
        buf[base + 8..base + 12].copy_from_slice(&qty.to_le_bytes());
        buf[base + 12..base + 16].copy_from_slice(&orders.to_le_bytes());
    }
    buf
}

/// Parses a depth-20 packet and ASSERTS the decoded side is the one the
/// emitter asked for.
///
/// The side check is load-bearing, not decoration: the emitter writes the
/// feed code via `DepthSide::as_feed_code`, so without this assertion an
/// inversion of `DepthSide::from_feed_code` (the 41/51 hazard the module docs
/// warn about) would round-trip invisibly through the level comparison —
/// verified by mutating `from_feed_code` and watching this catch it.
fn parse_twenty(
    raw: &[u8],
    buf: &mut DepthLevelBuffer,
    expected_side: DepthSide,
) -> Vec<DeepDepthLevel> {
    let packet = parse_depth_packet(raw, DepthFeedKind::Twenty, buf).expect("depth-20 must parse");
    match packet.payload {
        DepthPayload::Levels { levels, side } => {
            assert_eq!(
                side,
                expected_side,
                "SIDE INVERSION: emitted code {} decoded as {side:?}, expected {expected_side:?}",
                expected_side.as_feed_code()
            );
            levels.to_vec()
        }
        DepthPayload::Disconnect { .. } => panic!("expected levels"),
    }
}

// ---------------------------------------------------------------------------
// Cross-kind agreement: d5 (Full) vs d20 (depth socket)
// ---------------------------------------------------------------------------

#[test]
fn d5_and_d20_agree_on_levels_one_to_five_for_the_same_instrument() {
    let book = shared_book();

    let full_raw = emit_as_full_packet(&book);
    let header = PacketHeader {
        response_code: RESPONSE_CODE_FULL,
        message_length: FULL_QUOTE_PACKET_SIZE as u16,
        exchange_segment_code: 1,
        security_id: 1333,
    };
    let (_, d5) = parse_full_packet(&full_raw, &header, 0).expect("full packet must parse");

    let mut b1 = DepthLevelBuffer::new();
    let mut b2 = DepthLevelBuffer::new();
    let d20_bids = parse_twenty(
        &emit_as_twenty_packet(&book, DepthSide::Bid),
        &mut b1,
        DepthSide::Bid,
    );
    let d20_asks = parse_twenty(
        &emit_as_twenty_packet(&book, DepthSide::Ask),
        &mut b2,
        DepthSide::Ask,
    );

    // The d20 socket sends a FIXED 20 levels; only the first 5 are comparable.
    assert_eq!(d20_bids.len(), TWENTY_DEPTH_LEVELS);
    assert_eq!(d20_asks.len(), TWENTY_DEPTH_LEVELS);

    for i in 0..MARKET_DEPTH_LEVELS {
        // Prices: f32 (d5) widened to f64 (d20). Dyadic fixtures make this
        // exact; see the module-level honest-limits note.
        assert_eq!(
            f64::from(d5[i].bid_price),
            d20_bids[i].price,
            "CROSS-KIND DISAGREEMENT: level {i} bid price d5={} d20={}",
            d5[i].bid_price,
            d20_bids[i].price
        );
        assert_eq!(
            f64::from(d5[i].ask_price),
            d20_asks[i].price,
            "CROSS-KIND DISAGREEMENT: level {i} ask price d5={} d20={}",
            d5[i].ask_price,
            d20_asks[i].price
        );
        assert_eq!(
            d5[i].bid_quantity, d20_bids[i].quantity,
            "CROSS-KIND DISAGREEMENT: level {i} bid quantity"
        );
        assert_eq!(
            d5[i].ask_quantity, d20_asks[i].quantity,
            "CROSS-KIND DISAGREEMENT: level {i} ask quantity"
        );
        assert_eq!(
            u32::from(d5[i].bid_orders),
            d20_bids[i].orders,
            "CROSS-KIND DISAGREEMENT: level {i} bid orders"
        );
        assert_eq!(
            u32::from(d5[i].ask_orders),
            d20_asks[i].orders,
            "CROSS-KIND DISAGREEMENT: level {i} ask orders"
        );
    }
}

#[test]
fn cross_kind_agreement_check_would_catch_a_level_index_drift() {
    // Bite-proof, permanent: corrupt the d20 emission by shifting every level
    // down one index — the exact shape of a level-index drift in the
    // fixed-offset walk — and prove the comparison REJECTS it. Without this,
    // the test above could silently degrade into comparing a book to itself.
    let book = shared_book();
    let mut drifted = book;
    drifted.rotate_left(1);

    let full_raw = emit_as_full_packet(&book);
    let header = PacketHeader {
        response_code: RESPONSE_CODE_FULL,
        message_length: FULL_QUOTE_PACKET_SIZE as u16,
        exchange_segment_code: 1,
        security_id: 1333,
    };
    let (_, d5) = parse_full_packet(&full_raw, &header, 0).expect("full packet must parse");

    let mut b1 = DepthLevelBuffer::new();
    let d20_bids = parse_twenty(
        &emit_as_twenty_packet(&drifted, DepthSide::Bid),
        &mut b1,
        DepthSide::Bid,
    );

    let disagreements = (0..MARKET_DEPTH_LEVELS)
        .filter(|&i| f64::from(d5[i].bid_price) != d20_bids[i].price)
        .count();
    assert!(
        disagreements >= 4,
        "a one-level drift must disagree on nearly every level, got {disagreements}"
    );
}

#[test]
fn cross_kind_agreement_check_would_catch_a_side_inversion() {
    // The 41/51 hazard the depth module docs warn about: if the side codes
    // were swapped, d20's "bid" packet would carry ask prices. d5 has no such
    // ambiguity (bid and ask sit at distinct offsets in one level), so the
    // cross-kind comparison is the ONLY place that can catch it.
    let book = shared_book();
    let full_raw = emit_as_full_packet(&book);
    let header = PacketHeader {
        response_code: RESPONSE_CODE_FULL,
        message_length: FULL_QUOTE_PACKET_SIZE as u16,
        exchange_segment_code: 1,
        security_id: 1333,
    };
    let (_, d5) = parse_full_packet(&full_raw, &header, 0).expect("full packet must parse");

    let mut b = DepthLevelBuffer::new();
    // Deliberately compare d5 BIDS against the d20 ASK packet.
    let d20_asks = parse_twenty(
        &emit_as_twenty_packet(&book, DepthSide::Ask),
        &mut b,
        DepthSide::Ask,
    );
    let disagreements = (0..MARKET_DEPTH_LEVELS)
        .filter(|&i| f64::from(d5[i].bid_price) != d20_asks[i].price)
        .count();
    assert_eq!(
        disagreements, MARKET_DEPTH_LEVELS,
        "an inverted side must disagree on EVERY level"
    );
}

// ---------------------------------------------------------------------------
// Property coverage: level count, packet length, frame splitting
// ---------------------------------------------------------------------------

/// Builds an arbitrary but well-formed depth packet body of `rows` levels.
#[allow(clippy::arithmetic_side_effects)]
fn build_packet(code: u8, sid: u32, segment: u8, rows: u32, level_count: usize) -> Vec<u8> {
    let total = DEEP_DEPTH_HEADER_SIZE + level_count * DEEP_DEPTH_LEVEL_SIZE;
    let mut buf = vec![0u8; total];
    buf[0..2].copy_from_slice(&(total as u16).to_le_bytes());
    buf[2] = code;
    buf[3] = segment;
    buf[4..8].copy_from_slice(&sid.to_le_bytes());
    buf[8..12].copy_from_slice(&rows.to_le_bytes());
    buf
}

proptest! {
    /// The parsed level count must EQUAL the count declared for that depth
    /// kind — a fixed 20 for depth-20, the header row count for depth-200 —
    /// and never silently truncate or over-read.
    #[test]
    fn parsed_level_count_equals_declared_count_for_the_kind(
        rows in 0_u32..=200,
        sid in any::<u32>(),
        segment in any::<u8>(),
        is_bid in any::<bool>(),
    ) {
        let code = if is_bid { DEEP_DEPTH_FEED_CODE_BID } else { DEEP_DEPTH_FEED_CODE_ASK };

        // depth-200: the row count IS the level count.
        let raw = build_packet(code, sid, segment, rows, rows as usize);
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::TwoHundred, &mut buf)
            .map_err(|e| TestCaseError::fail(format!("depth-200 {e:?}")))?;
        match packet.payload {
            DepthPayload::Levels { levels, side } => {
                prop_assert_eq!(levels.len(), rows as usize);
                prop_assert_eq!(side == DepthSide::Bid, is_bid);
                prop_assert_eq!(packet.header.security_id, u64::from(sid));
            }
            DepthPayload::Disconnect { .. } => prop_assert!(false, "expected levels"),
        }

        // depth-20: ALWAYS 20 levels, whatever the header bytes 8..12 say
        // (there they are a "message sequence, to be ignored").
        let raw20 = build_packet(code, sid, segment, rows, TWENTY_DEPTH_LEVELS);
        let mut buf20 = DepthLevelBuffer::new();
        let p20 = parse_depth_packet(&raw20, DepthFeedKind::Twenty, &mut buf20)
            .map_err(|e| TestCaseError::fail(format!("depth-20 {e:?}")))?;
        match p20.payload {
            DepthPayload::Levels { levels, .. } => {
                prop_assert_eq!(levels.len(), TWENTY_DEPTH_LEVELS);
            }
            DepthPayload::Disconnect { .. } => prop_assert!(false, "expected levels"),
        }
    }

    /// `depth_packet_len` must equal `header + levels * 16` for every legal
    /// row count and both kinds — the number the frame splitter advances by.
    /// A drift here mis-frames every packet after the first.
    #[test]
    fn depth_packet_len_equals_header_plus_levels_times_level_size(
        rows in 0_u32..=200,
        is_bid in any::<bool>(),
    ) {
        let code = if is_bid { DEEP_DEPTH_FEED_CODE_BID } else { DEEP_DEPTH_FEED_CODE_ASK };
        let raw = build_packet(code, 1333, 1, rows, rows as usize);
        let header = parse_depth_header(&raw)
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;

        let count200 = depth_level_count(&header, DepthFeedKind::TwoHundred)
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(count200, rows as usize);
        let len200 = depth_packet_len(&header, DepthFeedKind::TwoHundred)
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(len200, DEEP_DEPTH_HEADER_SIZE + count200 * DEEP_DEPTH_LEVEL_SIZE);

        let count20 = depth_level_count(&header, DepthFeedKind::Twenty)
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(count20, TWENTY_DEPTH_LEVELS);
        let len20 = depth_packet_len(&header, DepthFeedKind::Twenty)
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(len20, DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE);
    }

    /// A row count above 200 must be REFUSED, never clamped — a clamp would
    /// mis-frame the rest of the stacked frame while looking successful.
    #[test]
    fn depth_two_hundred_refuses_row_counts_above_the_maximum(
        rows in 201_u32..=100_000,
    ) {
        let raw = build_packet(DEEP_DEPTH_FEED_CODE_BID, 1333, 1, rows, 0);
        let header = parse_depth_header(&raw)
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert!(depth_level_count(&header, DepthFeedKind::TwoHundred).is_err());
        prop_assert!(depth_packet_len(&header, DepthFeedKind::TwoHundred).is_err());
        prop_assert!(rows as usize > TWO_HUNDRED_DEPTH_LEVELS);
    }

    /// Bytes consumed by the frame splitter must equal the SUM of the
    /// `depth_packet_len` of the packets it yielded — the splitter and the
    /// length function must never disagree about where a packet ends.
    #[test]
    fn frame_splitter_consumes_exactly_the_sum_of_packet_lengths(
        packet_count in 0_usize..=6,
        is_bid in any::<bool>(),
    ) {
        let code = if is_bid { DEEP_DEPTH_FEED_CODE_BID } else { DEEP_DEPTH_FEED_CODE_ASK };
        let mut frame = Vec::new();
        for i in 0..packet_count {
            frame.extend_from_slice(&build_packet(code, i as u32, 1, 0, TWENTY_DEPTH_LEVELS));
        }
        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        let mut summed = 0_usize;
        let mut yielded = 0_usize;
        for packet in iter.by_ref() {
            let header = parse_depth_header(packet)
                .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
            let len = depth_packet_len(&header, DepthFeedKind::Twenty)
                .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
            prop_assert_eq!(packet.len(), len, "yielded slice must be exactly one packet");
            summed += len;
            yielded += 1;
        }
        prop_assert_eq!(yielded, packet_count);
        prop_assert_eq!(iter.consumed_bytes(), summed);
        prop_assert_eq!(summed, frame.len());
    }

    /// A depth-feed disconnect is sized independently of the level count and
    /// must never be confused with a levels packet on either kind.
    #[test]
    fn depth_disconnect_len_is_kind_independent(rows in any::<u32>()) {
        let raw = build_packet(RESPONSE_CODE_DISCONNECT, 1333, 1, rows, 0);
        let header = parse_depth_header(&raw)
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        for kind in [DepthFeedKind::Twenty, DepthFeedKind::TwoHundred] {
            let len = depth_packet_len(&header, kind)
                .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
            prop_assert_eq!(len, DEPTH_DISCONNECT_PACKET_SIZE);
        }
    }

    /// Any response code that is not 41 / 51 / 50 must be REFUSED. The
    /// splitter cannot know where the next packet starts, so guessing would
    /// mis-frame everything after it.
    #[test]
    fn unknown_response_codes_are_refused_not_guessed(code in any::<u8>()) {
        prop_assume!(code != DEEP_DEPTH_FEED_CODE_BID
            && code != DEEP_DEPTH_FEED_CODE_ASK
            && code != RESPONSE_CODE_DISCONNECT);
        let raw = build_packet(code, 1333, 1, 0, TWENTY_DEPTH_LEVELS);
        let header = parse_depth_header(&raw)
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert!(depth_packet_len(&header, DepthFeedKind::Twenty).is_err());
        prop_assert!(depth_packet_len(&header, DepthFeedKind::TwoHundred).is_err());
    }
}
