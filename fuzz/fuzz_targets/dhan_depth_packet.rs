//! Fuzz target: Dhan depth-20 / depth-200 binary packet parser.
//!
//! # Why this target exists
//!
//! The existing `tick_parser` target fuzzes `dispatch_frame`, which is the
//! MAIN-feed entry point. The depth feeds are a completely separate wire
//! shape and are unreachable from it — `crates/core/src/parser/mod.rs` says
//! so verbatim:
//!
//! > They are parsed by `depth.rs` via `split_depth_frame()` +
//! > `dispatch_depth_packet()` — NEVER by `dispatch_frame()`, whose 8-byte
//! > header would mis-read every field.
//!
//! `depth.rs` was re-added 2026-08-09 for the authorized live-WebSocket
//! revival (which raised the connection lock to ≤16: 5 main-feed + 5
//! depth-20 + 5 depth-200 + 1 order-update), so up to TEN of the sixteen
//! authorized sockets feed this parser and none of them had a fuzz target.
//!
//! # The property under test
//!
//! `depth.rs` claims totality in its module header:
//!
//! > Every function here is TOTAL over arbitrary bytes: truncated,
//! > oversized, zero-length and garbage input all return a typed
//! > `ParseError` (or terminate the iterator with a recorded
//! > `DepthSplitStop`). Nothing in this module panics, indexes a slice
//! > unchecked, or allocates.
//!
//! **Any panic is a bug**, and the iterator must also TERMINATE — the docs
//! promise "every step advances by at least `DEEP_DEPTH_HEADER_SIZE` bytes
//! [...] It can neither loop forever nor panic." A zero-advance bug would
//! hang the fuzzer, which libFuzzer reports as a timeout.
//!
//! Two hazards make this parser unusually worth fuzzing:
//!
//! 1. **The 1-based/0-based header hazard.** Dhan's header table is 1-based
//!    while the packet tables are 0-based; an off-by-one yields
//!    plausible-but-wrong prices rather than a crash. Fuzzing cannot catch a
//!    wrong-but-valid price (the golden vectors in `depth.rs` do that), but
//!    it does catch the out-of-bounds reads a bad offset can produce.
//! 2. **Attacker-controlled length arithmetic.** `depth_packet_len` derives
//!    a length from a header row-count field, and depth-200's row count is
//!    read straight off the wire. `12 + row_count * 16` is exactly the shape
//!    that overflows or over-reads if the bound is missed.
//!
//! # Coverage strategy
//!
//! The feed kind is NOT derivable from the packet bytes (the caller supplies
//! it from the connection it owns), so both kinds are driven over every
//! input. Byte 0 is also folded into the valid response-code set on a second
//! pass so the fuzzer reaches the level-walk instead of stalling on
//! `UnknownResponseCode`.

#![no_main]

use libfuzzer_sys::fuzz_target;

use tickvault_core::parser::{
    DepthFeedKind, DepthLevelBuffer, depth_level_count, depth_packet_len, parse_depth_header,
    parse_depth_packet, split_depth_frame,
};

/// Valid depth-feed response codes: 41 = Bid, 51 = Ask, 50 = disconnect.
/// The extra bytes keep the `UnknownResponseCode` refusal arm exercised.
const FEED_CODES: &[u8] = &[41, 51, 50, 0, 8, 255];

/// Drives every depth entry point over one candidate frame for one kind.
fn exercise(raw: &[u8], kind: DepthFeedKind, buf: &mut DepthLevelBuffer) {
    // Header parse — must be total over any length, including 0..12.
    if let Ok(header) = parse_depth_header(raw) {
        // Length + level-count derivation from an ATTACKER-CONTROLLED row
        // count. These must refuse (InvalidRowCount) rather than overflow or
        // return a length that later over-reads.
        let _ = depth_packet_len(&header, kind);
        let _ = depth_level_count(&header, kind);
    }

    // Single-packet parse into the caller-owned buffer.
    let _ = parse_depth_packet(raw, kind, buf);

    // Stacked-frame iteration — the real wire shape per `16-fmd:169`:
    // `[I1 Bid][I1 Ask][I2 Bid][I2 Ask]…`. The iterator must terminate on
    // ANY input; a non-advancing step would hang here and surface as a
    // libFuzzer timeout.
    let mut iter = split_depth_frame(raw, kind);
    // Bounded drain: a correct iterator advances >= 12 bytes per step, so it
    // cannot yield more than len/12 packets. The cap converts a
    // hypothetical infinite-loop regression into a fast, obvious failure
    // instead of a fuzz timeout.
    let max_packets = raw.len() / 12 + 1;
    let mut seen = 0usize;
    for pkt in iter.by_ref() {
        // Re-parse each split-out packet: `split_depth_frame` only slices,
        // `parse_depth_packet` decodes.
        let _ = parse_depth_packet(pkt, kind, buf);
        seen += 1;
        assert!(
            seen <= max_packets,
            "split_depth_frame yielded {seen} packets from {} bytes — a step \
             advanced by less than the 12-byte header, which the module docs \
             say is impossible. This is a non-termination bug.",
            raw.len()
        );
    }
    // Stop reason must be recorded once exhausted, never silently swallowed.
    let _ = iter.stop_reason();
    let _ = iter.length_field_mismatches();
}

fuzz_target!(|data: &[u8]| {
    // One buffer, reused across every call — mirrors the live connection
    // task, which allocates one per socket and reuses it for the socket's
    // lifetime.
    let mut buf = DepthLevelBuffer::new();

    // ---- Arm 1: raw bytes, both feed kinds ------------------------------
    exercise(data, DepthFeedKind::Twenty, &mut buf);
    exercise(data, DepthFeedKind::TwoHundred, &mut buf);

    if data.is_empty() {
        return;
    }

    // ---- Arm 2: steered response code ------------------------------------
    // Raw fuzzer bytes almost never carry 41/51/50 at offset 2, so without
    // steering the budget is spent on the UnknownResponseCode early return
    // and the level walk is never reached.
    let mut steered = data.to_vec();
    if steered.len() > 2 {
        // `data` is non-empty and `FEED_CODES` is non-empty: both safe.
        steered[2] = FEED_CODES[usize::from(data[0]) % FEED_CODES.len()];
        exercise(&steered, DepthFeedKind::Twenty, &mut buf);
        exercise(&steered, DepthFeedKind::TwoHundred, &mut buf);
    }

    // ---- Arm 3: steered depth-200 row count -------------------------------
    // The row count lives at header bytes 8..12 and is the authoritative
    // level count for depth-200 — `12 + row_count * 16`. Driving it to
    // boundary and absurd values is the highest-value case in this target:
    // a missed bound is an out-of-bounds read of up to 4 GiB.
    if steered.len() >= 12 {
        for rows in [0u32, 1, 20, 199, 200, 201, 65_535, u32::MAX] {
            steered[8..12].copy_from_slice(&rows.to_le_bytes());
            exercise(&steered, DepthFeedKind::TwoHundred, &mut buf);
            exercise(&steered, DepthFeedKind::Twenty, &mut buf);
        }
    }
});
