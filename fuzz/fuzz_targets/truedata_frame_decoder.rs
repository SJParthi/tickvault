//! Fuzz target: TrueData frame decoder (binary + JSON transport modes).
//!
//! # Why this target exists
//!
//! Before this file, the entire `crates/core/src/parser/truedata*.rs` tree
//! (~7,062 LoC across six modules) had NO fuzz target at all — while its own
//! scope-lock, `.claude/rules/project/truedata-feed-scope-2026-07-24.md`,
//! demands totality in §11.7 ratchet #6
//! (`truedata_tolerates_all_message_types` — "decode dispatch handles
//! touchline/bidask/bidaskL2/bar/greeks/marketstatus/heartbeat without
//! panic") and §11.7 #12 (`D` must be REFUSED, never guessed).
//!
//! # The property under test
//!
//! `route_frame` is documented as TOTAL:
//!
//! > O(1) dispatch, zero allocation. Total — every input either routes or
//! > yields a typed `RouteRefusal`; there is no panic and no silent drop.
//!
//! **Any panic here is a bug.** That includes: slice index out of range,
//! integer overflow in a length computation, `unwrap`/`expect` on a
//! malformed field, and UTF-8 validation panics on the borrowed `&str`
//! fields of `TruedataAuth` / `TruedataMarketStatus` / `TruedataRecordList`.
//!
//! This matters more here than for a well-formed feed: TrueData frames may
//! arrive LZ4-compressed (§11.1), so a corrupt decompression yields
//! arbitrary bytes with a plausible leading msg code — exactly this
//! target's input distribution.
//!
//! # Coverage strategy
//!
//! Raw fuzzer bytes rarely start with one of the thirteen valid msg codes,
//! so a naive target would spend almost all its budget on the
//! `UnknownMsgCode` early-return and never reach a field decoder. The first
//! input byte is therefore folded into the documented code set, which steers
//! the remaining bytes into the real decode paths while still exercising the
//! unknown-code and empty-frame arms via the fall-through.

#![no_main]

use libfuzzer_sys::fuzz_target;

use tickvault_core::parser::truedata::{
    check_seq, classify_frame, decode_auth_binary, decode_heartbeat_binary, decode_trade_binary,
};
use tickvault_core::parser::truedata_aux::{
    decode_bar_binary, decode_bidask_binary, decode_bidask_l2_binary, decode_greeks_binary,
    decode_record_list,
};
use tickvault_core::parser::truedata_json::{
    decode_bidask_json, decode_greeks_json, decode_trade_json, json_msg_type,
};
use tickvault_core::parser::truedata_router::route_frame;

/// Every documented TrueData msg code (§11.1: thirteen exist), plus the JSON
/// sniff byte, plus two deliberately-invalid bytes so the refusal arms stay
/// exercised.
const CODES: &[u8] = &[
    b'A', // auth
    b'T', // trade, with bid/ask
    b'W', // trade, without bid/ask
    b'B', // bidask
    b'D', // bidask L2 — MUST be refused, never guessed (§11.7 #12)
    b'G', // greeks
    b'O', // bar 1min
    b'F', // bar 5min
    b'S', // symbols added
    b'L', // touchline
    b'R', // symbols removed
    b'H', // heartbeat
    b'M', // market status
    b'{', // JSON sniff byte
    b'Z', // unknown — refusal arm
    0x00, // NUL — refusal arm
];

fuzz_target!(|data: &[u8]| {
    // ---- Arm 1: raw bytes, exactly as they arrive off the wire ----------
    // Covers the empty frame and the unknown-code fall-through.
    let _ = route_frame(data);
    let _ = classify_frame(data);

    if data.is_empty() {
        return;
    }

    // ---- Arm 2: steered code, so field decoders are actually reached ----
    // Fold byte 0 into the documented code set and re-drive the router. The
    // remaining bytes are untouched fuzzer entropy.
    let mut steered = data.to_vec();
    // `data` is non-empty and `CODES` is non-empty, so both indexes are safe.
    let code = CODES[usize::from(data[0]) % CODES.len()];
    steered[0] = code;

    // The router is the production entry point — it must never panic.
    let _ = route_frame(&steered);

    // ---- Arm 3: each decoder directly ------------------------------------
    // The router refuses on a code mismatch before reaching most decoders,
    // so hitting them directly is the only way to fuzz their length and
    // field handling against a frame whose code does NOT match.
    let _ = decode_trade_binary(&steered);
    let _ = decode_auth_binary(&steered);
    let _ = decode_heartbeat_binary(&steered);
    let _ = decode_bidask_binary(&steered);
    let _ = decode_greeks_binary(&steered);
    let _ = decode_bar_binary(&steered);
    let _ = decode_record_list(&steered);
    // `D`: the layout is UNRESOLVABLE from the vendor sources (§11.6), so
    // this must return `UnresolvedLayout` rather than guess. Either way it
    // must not panic.
    let _ = decode_bidask_l2_binary(&steered);

    // ---- Arm 4: the JSON transport mode ----------------------------------
    // JSON is a first-class transport (§11.1 — `compression=False` is the
    // vendor SDK's own default), and its positional scanner walks arbitrary
    // text, so it needs the same totality guarantee as the binary path.
    let _ = json_msg_type(data);
    let _ = decode_trade_json(data);
    let _ = decode_bidask_json(data);
    let _ = decode_greeks_json(data);

    // ---- Arm 5: the sequence-gap detector --------------------------------
    // TD-GAP-01's loss proof does arithmetic on two attacker-controlled
    // i32s; a naive `current - previous` would overflow-panic in a debug
    // build. Drive it with the widest possible pairs.
    if steered.len() >= 8 {
        let a = i32::from_le_bytes([steered[0], steered[1], steered[2], steered[3]]);
        let b = i32::from_le_bytes([steered[4], steered[5], steered[6], steered[7]]);
        let _ = check_seq(None, a);
        let _ = check_seq(Some(a), b);
        let _ = check_seq(Some(b), a);
        let _ = check_seq(Some(i32::MIN), a);
        let _ = check_seq(Some(i32::MAX), b);
        let _ = check_seq(Some(a), i32::MIN);
        let _ = check_seq(Some(b), i32::MAX);
    }
});
