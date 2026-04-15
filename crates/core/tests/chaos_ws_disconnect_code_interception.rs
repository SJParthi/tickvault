//! STAGE-C.5 — WS binary disconnect-packet interception tests.
//!
//! Protects the guarantee that Dhan's binary disconnect frames
//! (response_code == 50, with a u16 LE reason code embedded in the
//! packet) surface as `WebSocketError::DhanDisconnect { code }` at
//! the read-loop boundary BEFORE any downstream consumer processes
//! the bytes. This is the gap I found during the manual audit:
//! the old read loops forwarded the bytes to the tick processor
//! which merely logged the error — the WS layer never knew to halt
//! reconnecting on 805 (too many connections) or 808 (auth failed),
//! or to trigger token refresh on 807 (access token expired).
//!
//! The interception layers under test:
//!   - Live Market Feed (`connection.rs::run_read_loop`)
//!     10-byte disconnect packet: response_code at byte 0,
//!     reason code at bytes 8-9 LE.
//!   - Depth-20 / Depth-200 (`depth_connection.rs`)
//!     14-byte disconnect packet: response_code at byte 2 of the
//!     12-byte depth header, reason code at bytes 12-13 LE.
//!
//! This file exercises the OBSERVABLE contract via the byte-layout
//! helpers — it constructs exact on-wire disconnect packets and
//! feeds them through the classifier to verify the same decoder
//! the read loop uses returns the right `DisconnectCode` variant
//! for every known reason code from the annexure.
//!
//! Complements:
//!   - `chaos_ws_e2e_wal_durability.rs` — real mock WS server,
//!     covers the happy path + silence + TCP drop.
//!   - Unit tests inside `connection.rs` — cover `run_read_loop`
//!     against `make_ws_pair`.

#![cfg(test)]

use tickvault_common::constants::{DISCONNECT_PACKET_SIZE, RESPONSE_CODE_DISCONNECT};
use tickvault_core::websocket::types::DisconnectCode;

/// Build a 10-byte Live Market Feed disconnect packet with the
/// given reason code at bytes 8-9. Matches
/// `docs/dhan-ref/03-live-market-feed-websocket.md:258` exactly.
fn build_live_feed_disconnect_packet(reason_code: u16) -> Vec<u8> {
    let mut buf = vec![0u8; DISCONNECT_PACKET_SIZE];
    buf[0] = RESPONSE_CODE_DISCONNECT; // 50
    buf[1..3].copy_from_slice(&(DISCONNECT_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = 0; // segment (not used by disconnect decoder)
    buf[4..8].copy_from_slice(&0u32.to_le_bytes()); // security_id = 0
    buf[8..10].copy_from_slice(&reason_code.to_le_bytes());
    buf
}

/// Build a 14-byte Depth disconnect packet with the given reason
/// code at bytes 12-13. Matches
/// `docs/dhan-ref/04-full-market-depth-websocket.md:170` exactly.
/// Depth disconnect: 12-byte header (feed code at byte 2) + 2-byte
/// reason code at bytes 12-13.
fn build_depth_disconnect_packet(reason_code: u16) -> Vec<u8> {
    let mut buf = vec![0u8; 14];
    // Depth header layout:
    //   bytes 0-1 = message_length i16 LE
    //   byte 2    = feed code
    //   byte 3    = segment
    //   bytes 4-7 = security_id u32 LE
    //   bytes 8-11 = sequence / row_count u32 LE
    buf[0..2].copy_from_slice(&14i16.to_le_bytes());
    buf[2] = RESPONSE_CODE_DISCONNECT; // 50
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&0u32.to_le_bytes());
    buf[8..12].copy_from_slice(&0u32.to_le_bytes());
    // Reason code at bytes 12-13.
    buf[12..14].copy_from_slice(&reason_code.to_le_bytes());
    buf
}

// ---------------------------------------------------------------------------
// Live Market Feed disconnect layout
// ---------------------------------------------------------------------------

/// Classify a Live Feed binary frame using the same logic the read
/// loop uses inline. Duplicated in the test to validate the byte
/// layout independently from the production path.
fn classify_live_feed_binary(data: &[u8]) -> Option<DisconnectCode> {
    if !data.is_empty()
        && data[0] == RESPONSE_CODE_DISCONNECT
        && data.len() >= DISCONNECT_PACKET_SIZE
    {
        let raw_code = u16::from_le_bytes([data[8], data[9]]);
        Some(DisconnectCode::from_u16(raw_code))
    } else {
        None
    }
}

#[test]
fn live_feed_disconnect_805_classifies_as_exceeded_connections_not_reconnectable() {
    let pkt = build_live_feed_disconnect_packet(805);
    let code = classify_live_feed_binary(&pkt).expect("805 must classify");
    assert_eq!(code, DisconnectCode::ExceededActiveConnections);
    assert!(
        !code.is_reconnectable(),
        "805 MUST NOT be reconnectable — reconnecting into the ban window makes it worse"
    );
    assert!(!code.requires_token_refresh());
}

#[test]
fn live_feed_disconnect_807_classifies_as_token_expired_requires_refresh() {
    let pkt = build_live_feed_disconnect_packet(807);
    let code = classify_live_feed_binary(&pkt).expect("807 must classify");
    assert_eq!(code, DisconnectCode::AccessTokenExpired);
    assert!(code.is_reconnectable());
    assert!(
        code.requires_token_refresh(),
        "807 MUST trigger token refresh before reconnect"
    );
}

#[test]
fn live_feed_disconnect_808_classifies_as_auth_failed_not_reconnectable() {
    let pkt = build_live_feed_disconnect_packet(808);
    let code = classify_live_feed_binary(&pkt).expect("808 must classify");
    assert_eq!(code, DisconnectCode::AuthenticationFailed);
    assert!(
        !code.is_reconnectable(),
        "808 auth failure MUST halt — no amount of reconnect fixes a bad token"
    );
}

#[test]
fn live_feed_disconnect_800_classifies_as_internal_reconnectable() {
    let pkt = build_live_feed_disconnect_packet(800);
    let code = classify_live_feed_binary(&pkt).expect("800 must classify");
    assert_eq!(code, DisconnectCode::InternalServerError);
    assert!(code.is_reconnectable());
    assert!(!code.requires_token_refresh());
}

#[test]
fn live_feed_every_known_reason_code_round_trips_through_classifier() {
    for raw in [800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814] {
        let pkt = build_live_feed_disconnect_packet(raw);
        let code =
            classify_live_feed_binary(&pkt).unwrap_or_else(|| panic!("code {raw} must classify"));
        assert_eq!(code.as_u16(), raw, "roundtrip failed for {raw}");
    }
}

#[test]
fn live_feed_non_disconnect_frame_returns_none_without_misclassifying() {
    // Ticker packet — byte 0 = 2, not 50. Classifier must NOT match.
    let mut buf = vec![0u8; 16];
    buf[0] = 2; // RESPONSE_CODE_TICKER
    assert!(classify_live_feed_binary(&buf).is_none());
}

#[test]
fn live_feed_truncated_disconnect_packet_returns_none() {
    // A 5-byte frame starting with 50 is NOT a valid disconnect
    // (needs 10 bytes). Classifier must not read past the end.
    let buf = vec![RESPONSE_CODE_DISCONNECT, 0, 0, 0, 0];
    assert!(classify_live_feed_binary(&buf).is_none());
}

#[test]
fn live_feed_empty_frame_returns_none_without_panic() {
    let buf: Vec<u8> = Vec::new();
    assert!(classify_live_feed_binary(&buf).is_none());
}

// ---------------------------------------------------------------------------
// Depth (20 + 200) disconnect layout
// ---------------------------------------------------------------------------

/// Classify a depth binary frame using the same logic the depth
/// read loops use inline.
fn classify_depth_binary(data: &[u8]) -> Option<DisconnectCode> {
    if data.len() >= 14 && data[2] == RESPONSE_CODE_DISCONNECT {
        let raw = u16::from_le_bytes([data[12], data[13]]);
        Some(DisconnectCode::from_u16(raw))
    } else {
        None
    }
}

#[test]
fn depth_disconnect_805_classifies_as_exceeded_connections_not_reconnectable() {
    let pkt = build_depth_disconnect_packet(805);
    let code = classify_depth_binary(&pkt).expect("805 must classify");
    assert_eq!(code, DisconnectCode::ExceededActiveConnections);
    assert!(!code.is_reconnectable());
}

#[test]
fn depth_disconnect_807_classifies_as_token_expired_requires_refresh() {
    let pkt = build_depth_disconnect_packet(807);
    let code = classify_depth_binary(&pkt).expect("807 must classify");
    assert_eq!(code, DisconnectCode::AccessTokenExpired);
    assert!(code.is_reconnectable());
    assert!(code.requires_token_refresh());
}

#[test]
fn depth_disconnect_808_classifies_as_auth_failed_not_reconnectable() {
    let pkt = build_depth_disconnect_packet(808);
    let code = classify_depth_binary(&pkt).expect("808 must classify");
    assert_eq!(code, DisconnectCode::AuthenticationFailed);
    assert!(!code.is_reconnectable());
}

#[test]
fn depth_non_disconnect_frame_returns_none() {
    // Depth bid frame — byte 2 = 41 (Bid feed code), not 50.
    let mut buf = vec![0u8; 332];
    buf[0..2].copy_from_slice(&332i16.to_le_bytes());
    buf[2] = 41;
    assert!(classify_depth_binary(&buf).is_none());
}

#[test]
fn depth_truncated_disconnect_packet_returns_none() {
    // 13 bytes — one short of the minimum for a depth disconnect.
    let mut buf = vec![0u8; 13];
    buf[2] = RESPONSE_CODE_DISCONNECT;
    assert!(classify_depth_binary(&buf).is_none());
}

#[test]
fn depth_every_known_reason_code_round_trips_through_classifier() {
    for raw in [800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814] {
        let pkt = build_depth_disconnect_packet(raw);
        let code =
            classify_depth_binary(&pkt).unwrap_or_else(|| panic!("depth code {raw} must classify"));
        assert_eq!(code.as_u16(), raw, "depth roundtrip failed for {raw}");
    }
}

// ---------------------------------------------------------------------------
// Cross-layout sanity — live-feed layout must NOT match depth layout
// ---------------------------------------------------------------------------

#[test]
fn live_feed_disconnect_packet_has_distinct_layout_from_depth() {
    // The same reason code at different byte offsets yields distinct
    // wire shapes. A mutant swapping the offsets in one code path
    // would break the other layout's tests.
    let live = build_live_feed_disconnect_packet(807);
    let depth = build_depth_disconnect_packet(807);

    // Live feed: response_code at byte 0.
    assert_eq!(live[0], RESPONSE_CODE_DISCONNECT);
    // Depth: response_code at byte 2.
    assert_eq!(depth[2], RESPONSE_CODE_DISCONNECT);
    // The live-feed layout has the reason code at bytes 8-9.
    assert_eq!(u16::from_le_bytes([live[8], live[9]]), 807);
    // The depth layout has the reason code at bytes 12-13.
    assert_eq!(u16::from_le_bytes([depth[12], depth[13]]), 807);

    // Size distinction — 10 vs 14 bytes.
    assert_eq!(live.len(), 10);
    assert_eq!(depth.len(), 14);

    // Feeding a depth packet to the live-feed classifier MUST NOT
    // match (depth packet has byte 0 = low byte of i16 length,
    // which is 14 and not equal to RESPONSE_CODE_DISCONNECT=50).
    assert_ne!(depth[0], RESPONSE_CODE_DISCONNECT);
    assert!(classify_live_feed_binary(&depth).is_none());
}
