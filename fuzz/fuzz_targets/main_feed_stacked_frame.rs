//! Fuzz target: STACKED Dhan main-feed frames and the socket-side frame
//! classifier.
//!
//! # Why this target exists
//!
//! `tick_parser` hands the whole input to `dispatch_frame`, which decodes ONE
//! packet and ignores everything after it. A real Dhan frame stacks many
//! packets, and two production walks read those bytes packet by packet: the
//! frame drain (`main_feed_packet_len` + `dispatch_frame` on each exact slice)
//! and `classify_frame`, which looks for a disconnect stacked behind data on
//! the socket read task. Neither walk was fuzzed, and neither was the
//! classifier on any endpoint.
//!
//! # Properties asserted (not only no-panic)
//!
//! 1. Every slice the walk cuts at a known code decodes `Ok`; a trailing
//!    partial packet is refused with `InsufficientBytes` (never decoded); an
//!    unknown code is refused (never decoded). The length table and the
//!    dispatcher must agree on what is known.
//! 2. `stacked_disconnect_reason` reports a reason only for a frame that walks
//!    end to end, and then exactly the FIRST disconnect's reason.
//! 3. `classify_frame` on every endpoint agrees with that walk: main feed =
//!    the stacked reason; order update = only an exact 10-byte code-50 frame;
//!    depth endpoints = the exact 14-byte form or the depth splitter's walk.
//! 4. A second, STRUCTURED frame is built from the input (a selector byte picks
//!    a known code, the next bytes fill that packet), so the fuzzer composes
//!    long valid stacks; with a disconnect appended it must be found, with the
//!    exact reason, by both the walk and `classify_frame(MainFeed)`. Every
//!    prefix of a small structured frame must walk to exactly its complete
//!    packets.
//!
//! Needs only `tickvault-core` (already a dependency of this crate).

#![no_main]

use libfuzzer_sys::fuzz_target;

use tickvault_core::parser::depth::{DepthFeedKind, stacked_depth_disconnect_reason};
use tickvault_core::parser::dispatcher::{
    dispatch_frame, main_feed_packet_len, stacked_disconnect_reason,
};
use tickvault_core::parser::{ParseError, ParsedFrame};
use tickvault_core::websocket::connection::{FrameClass, classify_frame};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::types::DisconnectCode;

/// Main-feed codes with a fixed size: 1/2 ticker, 4 quote, 5 OI, 6 previous
/// close, 7 market status, 8 full, 50 disconnect.
const KNOWN_CODES: [u8; 8] = [1, 2, 4, 5, 6, 7, 8, 50];
const MAX_PACKETS: u32 = 70_000;

struct WalkOutcome {
    packets: usize,
    complete: bool,
    first_disconnect: Option<u16>,
}

/// The frame drain's walk, asserting property 1 at every step.
fn walk(frame: &[u8]) -> WalkOutcome {
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
fn check_frame(frame: &[u8]) {
    let w = walk(frame);
    let stacked = stacked_disconnect_reason(frame, MAX_PACKETS);
    if w.complete {
        assert_eq!(
            stacked, w.first_disconnect,
            "stacked walk disagrees with the drain walk"
        );
    } else {
        assert_eq!(
            stacked, None,
            "a disconnect reported from a frame that does not walk"
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

/// Property 4: compose a valid stacked frame from the input stream. Returns
/// the frame and the end offset of every packet in it.
fn compose(data: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut frame = Vec::new();
    let mut ends = Vec::new();
    let mut i = 0usize;
    while i < data.len() && ends.len() < 64 {
        let code = KNOWN_CODES[usize::from(data[i]) % KNOWN_CODES.len()];
        i += 1;
        let len = main_feed_packet_len(&[code]).expect("known code has a size");
        let mut packet = vec![0u8; len];
        for (slot, b) in packet.iter_mut().skip(1).zip(data.iter().skip(i)) {
            *slot = *b;
        }
        i += len - 1;
        packet[0] = code;
        frame.extend_from_slice(&packet);
        ends.push(frame.len());
    }
    (frame, ends)
}

fuzz_target!(|data: &[u8]| {
    // ---- Arm 1: raw bytes ---------------------------------------------------
    check_frame(data);

    // ---- Arm 2: structured stack -------------------------------------------
    let (frame, ends) = compose(data);
    let w = walk(&frame);
    assert!(
        w.complete,
        "a composed frame of known codes must walk end to end"
    );
    assert_eq!(w.packets, ends.len());
    check_frame(&frame);

    // Every prefix walks to exactly its complete packets (bounded work).
    if frame.len() <= 2048 {
        for cut in 0..=frame.len() {
            let w = walk(&frame[..cut]);
            let complete = ends.iter().take_while(|&&e| e <= cut).count();
            assert_eq!(w.packets, complete, "cut {cut}");
            assert_eq!(w.complete, cut == 0 || ends.contains(&cut), "cut {cut}");
        }
    }

    // ---- Arm 3: a disconnect stacked behind the composed data --------------
    let reason = data.get(..2).map_or(805, |b| {
        u16::from_le_bytes([b[0], b.get(1).copied().unwrap_or(0)])
    });
    let mut with_disc = frame.clone();
    let mut disc = [0u8; 10];
    disc[0] = 50;
    disc[1..3].copy_from_slice(&10u16.to_le_bytes());
    disc[8..10].copy_from_slice(&reason.to_le_bytes());
    with_disc.extend_from_slice(&disc);
    let expected = w.first_disconnect.unwrap_or(reason);
    assert_eq!(
        stacked_disconnect_reason(&with_disc, MAX_PACKETS),
        Some(expected)
    );
    assert_eq!(
        classify_frame(DhanEndpointType::MainFeed, &with_disc),
        FrameClass::Disconnect(DisconnectCode::from_u16(expected))
    );
    check_frame(&with_disc);
});
