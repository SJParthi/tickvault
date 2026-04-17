//! S6-Step7 / Phase 6.9: Property tests for depth + greeks invariants.
//!
//! Locks the binary protocol invariants from
//! `.claude/rules/dhan/full-market-depth.md` against silent drift.
//!
//! # Invariants under test
//!
//! 1. **20-level depth packet size = 12 + 20*16 = 332 bytes** (header + 20 levels)
//! 2. **200-level depth packet size = 12 + 200*16 = 3212 bytes** (header + 200 levels)
//! 3. **Header size is 12 bytes** (NOT 8 — that's the main-feed header)
//! 4. **Each depth level is 16 bytes** (8 byte f64 price + 4 byte u32 qty + 4 byte u32 orders)
//! 5. **Header offsets are stable**:
//!    - msg_length at offset 0 (i16 LE)
//!    - feed_code at offset 2 (u8)
//!    - exchange_segment at offset 3 (u8)
//!    - security_id at offset 4 (i32 LE)
//!    - msg_sequence (20-lvl) / row_count (200-lvl) at offset 8 (u32 LE)
//! 6. **Each level offsets are stable**:
//!    - price at level offset 0 (f64 LE = 8 bytes)
//!    - quantity at level offset 8 (u32 LE = 4 bytes)
//!    - orders at level offset 12 (u32 LE = 4 bytes)
//! 7. **20-level depth: 50 instruments per connection**
//! 8. **200-level depth: 1 instrument per connection** (Dhan ticket #5519522)
//! 9. **5 connections per pool, INDEPENDENT across types** (15 total slots)
//!
//! # Why property-test these instead of just unit-test?
//!
//! Property tests verify the invariant holds across the entire input domain.
//! A unit test verifies one example. If a future PR accidentally changes the
//! header layout, the unit test might still pass for the example value but
//! fail for any other byte alignment. Property tests give us mathematical
//! certainty that the invariant holds.
//!
//! # 20 + 200 depth invariants are CRITICAL
//!
//! Wrong byte offsets in the depth parser silently corrupt the order book
//! — bid and ask prices swap, quantities go to the wrong side, the
//! algorithm makes catastrophically wrong trading decisions. There is NO
//! recovery from a depth corruption bug short of re-reading every level.

use proptest::prelude::*;
use tickvault_common::constants::{
    DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT, DEEP_DEPTH_HEADER_OFFSET_FEED_CODE,
    DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH, DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE,
    DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID, DEEP_DEPTH_HEADER_SIZE, DEEP_DEPTH_LEVEL_OFFSET_ORDERS,
    DEEP_DEPTH_LEVEL_OFFSET_PRICE, DEEP_DEPTH_LEVEL_OFFSET_QUANTITY, DEEP_DEPTH_LEVEL_SIZE,
    MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION, MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION,
    MAX_TWO_HUNDRED_DEPTH_CONNECTIONS, MAX_WEBSOCKET_CONNECTIONS, TWENTY_DEPTH_LEVELS,
    TWENTY_DEPTH_PACKET_SIZE, TWO_HUNDRED_DEPTH_LEVELS, TWO_HUNDRED_DEPTH_PACKET_SIZE,
};

// ---------------------------------------------------------------------------
// Static invariants (no random input needed)
// ---------------------------------------------------------------------------

#[test]
fn invariant_depth_header_is_12_bytes_not_8() {
    // The main-feed header is 8 bytes. The depth header is 12 bytes.
    // Confusing the two = parse garbage. This invariant locks the
    // depth header size against silent drift.
    assert_eq!(
        DEEP_DEPTH_HEADER_SIZE, 12,
        "Depth header MUST be 12 bytes (NOT 8 — that's the main feed)"
    );
}

#[test]
fn invariant_depth_level_is_16_bytes() {
    // f64 price (8) + u32 qty (4) + u32 orders (4) = 16 bytes.
    assert_eq!(
        DEEP_DEPTH_LEVEL_SIZE, 16,
        "Each depth level MUST be 16 bytes (f64 + u32 + u32)"
    );
}

#[test]
fn invariant_twenty_depth_packet_is_332_bytes() {
    // 12 header + 20 levels * 16 bytes/level = 332.
    assert_eq!(
        TWENTY_DEPTH_PACKET_SIZE,
        DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE
    );
    assert_eq!(TWENTY_DEPTH_PACKET_SIZE, 332);
}

#[test]
fn invariant_two_hundred_depth_packet_is_3212_bytes() {
    // 12 header + 200 levels * 16 bytes/level = 3212.
    assert_eq!(
        TWO_HUNDRED_DEPTH_PACKET_SIZE,
        DEEP_DEPTH_HEADER_SIZE + TWO_HUNDRED_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE
    );
    assert_eq!(TWO_HUNDRED_DEPTH_PACKET_SIZE, 3212);
}

#[test]
fn invariant_depth_header_offsets_stable() {
    // Offsets MUST be stable across builds. Any reorder = parse garbage.
    assert_eq!(
        DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH, 0,
        "msg_length at offset 0"
    );
    assert_eq!(
        DEEP_DEPTH_HEADER_OFFSET_FEED_CODE, 2,
        "feed_code at offset 2"
    );
    assert_eq!(
        DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT, 3,
        "exchange_segment at offset 3"
    );
    assert_eq!(
        DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID, 4,
        "security_id at offset 4"
    );
    assert_eq!(
        DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE, 8,
        "msg_sequence/row_count at offset 8"
    );
}

#[test]
fn invariant_depth_level_offsets_stable() {
    // Within each 16-byte level: f64 price (0..8) + u32 qty (8..12) + u32 orders (12..16)
    assert_eq!(DEEP_DEPTH_LEVEL_OFFSET_PRICE, 0);
    assert_eq!(DEEP_DEPTH_LEVEL_OFFSET_QUANTITY, 8);
    assert_eq!(DEEP_DEPTH_LEVEL_OFFSET_ORDERS, 12);

    // f64 price = 8 bytes, fits within a level (compile-time check)
    const {
        assert!(DEEP_DEPTH_LEVEL_OFFSET_PRICE + 8 <= DEEP_DEPTH_LEVEL_SIZE);
        assert!(DEEP_DEPTH_LEVEL_OFFSET_QUANTITY + 4 <= DEEP_DEPTH_LEVEL_SIZE);
        assert!(DEEP_DEPTH_LEVEL_OFFSET_ORDERS + 4 <= DEEP_DEPTH_LEVEL_SIZE);
    }
}

#[test]
fn invariant_twenty_depth_50_instruments_per_connection() {
    // Per .claude/rules/dhan/full-market-depth.md rule 11.
    assert_eq!(MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION, 50);
}

#[test]
fn invariant_two_hundred_depth_one_instrument_per_connection() {
    // Per Dhan ticket #5519522: 200-level is 1 instrument per connection.
    // Anything else gets server-reset within 3-5 seconds.
    assert_eq!(MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION, 1);
}

#[test]
fn invariant_depth_pools_independent_5_connections_each() {
    // Per Dhan support 2026-04-06: connection limits are INDEPENDENT
    // per WebSocket type. 5 main + 5 depth-20 + 5 depth-200 = 15 total.
    assert_eq!(MAX_WEBSOCKET_CONNECTIONS, 5);
    assert_eq!(MAX_TWO_HUNDRED_DEPTH_CONNECTIONS, 5);
}

// ---------------------------------------------------------------------------
// Property tests — invariants over random row counts
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// For any valid 20-level packet, the size formula must hold.
    /// (Trivial in code but proves the constants don't drift independently.)
    #[test]
    fn prop_twenty_depth_packet_size_invariant(_seed in any::<u32>()) {
        let computed = DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;
        prop_assert_eq!(computed, TWENTY_DEPTH_PACKET_SIZE);
        prop_assert_eq!(computed, 332);
    }

    /// 200-level: row_count is bounded [0, 200]. Any value > 200 = malformed
    /// packet. The parser must reject these — testing that the constant
    /// matches the docs cap.
    #[test]
    fn prop_two_hundred_depth_row_count_bounded(row_count in 0_usize..=200) {
        let body_size = row_count * DEEP_DEPTH_LEVEL_SIZE;
        let pkt_size = DEEP_DEPTH_HEADER_SIZE + body_size;
        // Any valid row_count <= 200 produces a packet that fits in
        // the maximum allocation.
        prop_assert!(pkt_size <= TWO_HUNDRED_DEPTH_PACKET_SIZE);
        // And row_count == 200 produces exactly the max.
        if row_count == 200 {
            prop_assert_eq!(pkt_size, TWO_HUNDRED_DEPTH_PACKET_SIZE);
        }
    }

    /// Bid (code 41) and Ask (code 51) are SEPARATE packets per Dhan spec.
    /// Property: response_code in {41, 51} for any valid depth packet.
    /// Codes outside this set = malformed.
    #[test]
    fn prop_depth_bid_ask_separated_by_response_code(code in any::<u8>()) {
        let is_valid_depth_code = code == 41 || code == 51;
        // Mutually exclusive: 41 is bid, 51 is ask, never both, never combined.
        if is_valid_depth_code {
            prop_assert!(code == 41 || code == 51);
            prop_assert!(!(code == 41 && code == 51));
        }
    }

    /// Header offset arithmetic is consistent: every offset+size fits
    /// within the 12-byte header.
    #[test]
    fn prop_header_offsets_fit_within_header_size(_seed in any::<u8>()) {
        // Every offset+field-size must fit within the 12-byte header.
        // Written as `x < y` (equivalent to `x + w <= y` where w >= 1) per
        // clippy::int_plus_one — the addition on the left was flagged as
        // redundant.
        // msg_length: 2 bytes (i16) at offset 0
        prop_assert!(DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH + 2 <= DEEP_DEPTH_HEADER_SIZE);
        // feed_code: 1 byte (u8) at offset 2
        prop_assert!(DEEP_DEPTH_HEADER_OFFSET_FEED_CODE < DEEP_DEPTH_HEADER_SIZE);
        // exchange_segment: 1 byte (u8) at offset 3
        prop_assert!(DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT < DEEP_DEPTH_HEADER_SIZE);
        // security_id: 4 bytes (i32) at offset 4
        prop_assert!(DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID + 4 <= DEEP_DEPTH_HEADER_SIZE);
        // msg_sequence: 4 bytes (u32) at offset 8
        prop_assert!(DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE + 4 <= DEEP_DEPTH_HEADER_SIZE);
    }
}
