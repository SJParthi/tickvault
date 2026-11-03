//! Shared stacked-frame fuzz assertions, also exercised by deterministic
//! integration tests at the production packet cap and one packet above it.

use tickvault_common::constants::MAX_PACKETS_PER_FRAME as MAX_PACKETS;

use tickvault_core::parser::depth::{DepthFeedKind, stacked_depth_disconnect_reason};
use tickvault_core::parser::dispatcher::{
    dispatch_frame, main_feed_packet_len, stacked_disconnect_reason,
};
use tickvault_core::parser::{ParseError, ParsedFrame};
use tickvault_core::websocket::connection::{FrameClass, classify_frame};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::types::DisconnectCode;

/// Packet count, completion and first disconnect from the independent drain walk.
pub(crate) struct WalkOutcome {
    pub(crate) packets: usize,
    pub(crate) complete: bool,
    pub(crate) first_disconnect: Option<u16>,
}

/// The frame drain's walk, asserting property 1 at every step.
pub(crate) fn walk(frame: &[u8]) -> WalkOutcome {
    let mut offset = 0usize;
    let mut packets = 0usize;
    let mut first_disconnect = None;
    while let Some(rest) = frame.get(offset..).filter(|r| !r.is_empty()) {
        let Some(len) = main_feed_packet_len(rest) else {
            assert!(
                dispatch_frame(rest, 0).is_err(),
                "dispatcher decoded a code the length table calls unknown"
            );
            return WalkOutcome {
                packets,
                complete: false,
                first_disconnect,
            };
        };
        let Some(packet) = rest.get(..len) else {
            match dispatch_frame(rest, 0) {
                Err(ParseError::InsufficientBytes { actual, .. }) => {
                    assert_eq!(actual, rest.len(), "refusal names the bytes it saw");
                }
                other => panic!(
                    "a {}-byte partial packet was not refused: {other:?}",
                    rest.len()
                ),
            }
            return WalkOutcome {
                packets,
                complete: false,
                first_disconnect,
            };
        };
        match dispatch_frame(packet, 0) {
            Ok(ParsedFrame::Disconnect(code)) => {
                if first_disconnect.is_none() {
                    first_disconnect = Some(code.as_u16());
                }
            }
            Ok(_) => {}
            Err(e) => panic!("complete {len}-byte packet refused: {e:?}"),
        }
        packets += 1;
        offset += len;
    }
    WalkOutcome {
        packets,
        complete: true,
        first_disconnect,
    }
}

/// Properties 2 and 3 for one frame.
pub(crate) fn check_frame(frame: &[u8]) {
    let w = walk(frame);
    let stacked = stacked_disconnect_reason(frame, MAX_PACKETS);
    if w.complete && u32::try_from(w.packets).is_ok_and(|count| count <= MAX_PACKETS) {
        assert_eq!(
            stacked, w.first_disconnect,
            "stacked walk disagrees with the drain walk"
        );
    } else {
        assert_eq!(
            stacked, None,
            "a disconnect reported from an incomplete or over-cap frame"
        );
    }

    let main_expected = stacked.map_or(FrameClass::Data, |r| {
        FrameClass::Disconnect(DisconnectCode::from_u16(r))
    });
    assert_eq!(
        classify_frame(DhanEndpointType::MainFeed, frame),
        main_expected
    );

    let order_expected = match frame {
        [50, _, _, _, _, _, _, _, lo, hi] => {
            FrameClass::Disconnect(DisconnectCode::from_u16(u16::from_le_bytes([*lo, *hi])))
        }
        _ => FrameClass::Data,
    };
    assert_eq!(
        classify_frame(DhanEndpointType::OrderUpdate, frame),
        order_expected
    );

    for (ep, kind) in [
        (DhanEndpointType::Depth20, DepthFeedKind::Twenty),
        (DhanEndpointType::Depth200, DepthFeedKind::TwoHundred),
    ] {
        let expected = if frame.len() == 14 {
            match frame.get(2) {
                Some(50) => FrameClass::Disconnect(DisconnectCode::from_u16(u16::from_le_bytes([
                    frame[12], frame[13],
                ]))),
                _ => FrameClass::Data,
            }
        } else {
            stacked_depth_disconnect_reason(frame, kind, MAX_PACKETS)
                .map_or(FrameClass::Data, |r| {
                    FrameClass::Disconnect(DisconnectCode::from_u16(r))
                })
        };
        assert_eq!(classify_frame(ep, frame), expected, "{ep:?}");
    }
}
