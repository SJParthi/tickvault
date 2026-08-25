//! DHAT zero-allocation ratchet for the TrueData BINARY decode path.
//!
//! # Why this file exists
//!
//! `.claude/rules/project/truedata-feed-scope-2026-07-24.md` §11.7 lists this
//! test by name as **ratchet #8**, `dhat_truedata_decode_zero_alloc`, and
//! §10.7 carries it forward unchanged. It did not exist: before this file,
//! the entire `crates/core/src/parser/truedata*.rs` tree (~7,062 LoC across
//! six modules) had ZERO integration tests, ZERO DHAT coverage and ZERO fuzz
//! targets, despite its own rule file mandating all three.
//!
//! # What the lock actually promises (and what this pins)
//!
//! §11.3 of that rule file is deliberately precise about complexity, and this
//! test is written to match it rather than to overclaim:
//!
//! | Stage | Claim |
//! |---|---|
//! | Msg-code dispatch | **O(1)** |
//! | Field extraction (binary) | **O(1)** — ~20 fixed-offset `from_le_bytes` |
//! | Seq-gap check | **O(1)** |
//! | Whole frame | **O(frame bytes), fixed bound, ZERO heap allocation** |
//!
//! §11.3 binds the wording: *"the sanctioned phrase for either transport mode
//! is 'zero-allocation, amortized-constant per tick (O(frame bytes) decode,
//! fixed bound)'"*. This test therefore asserts the **zero-allocation** half
//! — the part that is mechanically checkable — and makes no O(1)-per-frame
//! claim anywhere.
//!
//! # What is measured
//!
//! 1. `classify_frame()` — the msg-code dispatch, over all 13 documented
//!    codes plus the JSON sniff byte plus an unknown code.
//! 2. `decode_trade_binary()` for `T` (90 bytes, WITH bid/ask).
//! 3. `decode_trade_binary()` for `W` (74 bytes, WITHOUT bid/ask) — the
//!    §11.2 discovery: `W` is a DISTINCT code, not a truncated `T`, and
//!    mis-handling it would silently drop every tick on a bid/ask-disabled
//!    account.
//! 4. `decode_heartbeat_binary()` (10 bytes).
//! 5. `decode_auth_binary()` (128 bytes) — returns BORROWED `&str` fields;
//!    a regression to owned `String` would allocate and fail here.
//! 6. `check_seq()` — the per-symbol vendor loss proof (TD-GAP-01).
//! 7. Error paths on malformed input (truncated / wrong-length / unknown
//!    code) — a typed error must not allocate a message.
//!
//! # One profiler per binary
//!
//! `dhat::Profiler` is process-global and PANICS if a second is built while
//! one is alive, so every assertion lives in a SINGLE `#[test]` — the same
//! consolidation `dhat_ws_reader_zero_alloc.rs` documents.
//!
//! # Honest scope
//!
//! This covers the BINARY path only. The LZ4-decompress stage (§11.1) and the
//! JSON positional scanner (`truedata_json.rs`) are separate surfaces with
//! their own allocation stories and are NOT claimed here.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::hint::black_box;

use tickvault_core::parser::truedata::{
    OFF_MSG_CODE, SeqCheck, TD_AUTH_BINARY_LEN, TD_HEARTBEAT_BINARY_LEN, TD_JSON_SNIFF_BYTE,
    TD_MSG_CODE_AUTH, TD_MSG_CODE_BAR_1MIN, TD_MSG_CODE_BAR_5MIN, TD_MSG_CODE_BIDASK,
    TD_MSG_CODE_BIDASK_L2, TD_MSG_CODE_GREEKS, TD_MSG_CODE_HEARTBEAT, TD_MSG_CODE_MARKET_STATUS,
    TD_MSG_CODE_SYMBOLS_ADDED, TD_MSG_CODE_SYMBOLS_REMOVED, TD_MSG_CODE_TOUCHLINE,
    TD_MSG_CODE_TRADE, TD_MSG_CODE_TRADE_NO_BIDASK, TD_TRADE_BINARY_LEN_FULL,
    TD_TRADE_BINARY_LEN_QUOTE_ONLY, TruedataFrameKind, check_seq, classify_frame,
    decode_auth_binary, decode_heartbeat_binary, decode_trade_binary,
};

mod dhat_support;

// Field offsets, re-derived here from the vendor layout (v2.6 p.16) so this
// test is an INDEPENDENT witness: if a private offset in `truedata.rs` is
// changed, the decoded values below stop matching and the vacuity assertions
// fire. Copying the module's own constants would make the test agree with
// whatever the module currently does, which proves nothing.
const T_OFF_SYMBOL_ID: usize = 1;
const T_OFF_TIMESTAMP: usize = 5;
const T_OFF_LTP: usize = 9;
const T_OFF_VOLUME: usize = 13;
const T_OFF_ATP: usize = 17;
const T_OFF_TOT_VOLUME: usize = 21;
const T_OFF_OPEN: usize = 29;
const T_OFF_HIGH: usize = 33;
const T_OFF_LOW: usize = 37;
const T_OFF_PREV_CLOSE: usize = 41;
const T_OFF_OI: usize = 45;
const T_OFF_PREV_OI: usize = 53;
const T_OFF_TURNOVER: usize = 61;
const T_OFF_OHL: usize = 69;
const T_OFF_SEQ_NO: usize = 70;
const T_OFF_BID: usize = 74;
const T_OFF_BID_QTY: usize = 78;
const T_OFF_ASK: usize = 82;
const T_OFF_ASK_QTY: usize = 86;

/// Builds a TrueData trade frame. `msg_code` selects the shape:
/// `T` -> 90 bytes with the bid/ask block, `W` -> 74 bytes without it.
///
/// All multi-byte fields are LITTLE-ENDIAN, confirmed in §11.1 from the
/// vendor SDK's own `'<'` unpack format strings.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test builder, constant offsets bounded by the allocated Vec
fn make_trade_frame(msg_code: u8, symbol_id: i32, seq_no: i32) -> Vec<u8> {
    let len = if msg_code == TD_MSG_CODE_TRADE {
        TD_TRADE_BINARY_LEN_FULL
    } else {
        TD_TRADE_BINARY_LEN_QUOTE_ONLY
    };
    let mut b = vec![0u8; len];
    b[OFF_MSG_CODE] = msg_code;
    b[T_OFF_SYMBOL_ID..T_OFF_SYMBOL_ID + 4].copy_from_slice(&symbol_id.to_le_bytes());
    b[T_OFF_TIMESTAMP..T_OFF_TIMESTAMP + 4].copy_from_slice(&1_754_000_000_i32.to_le_bytes());
    b[T_OFF_LTP..T_OFF_LTP + 4].copy_from_slice(&24_512.75_f32.to_le_bytes());
    b[T_OFF_VOLUME..T_OFF_VOLUME + 4].copy_from_slice(&75_i32.to_le_bytes());
    b[T_OFF_ATP..T_OFF_ATP + 4].copy_from_slice(&24_500.5_f32.to_le_bytes());
    b[T_OFF_TOT_VOLUME..T_OFF_TOT_VOLUME + 8].copy_from_slice(&9_876_543_i64.to_le_bytes());
    b[T_OFF_OPEN..T_OFF_OPEN + 4].copy_from_slice(&24_400.00_f32.to_le_bytes());
    b[T_OFF_HIGH..T_OFF_HIGH + 4].copy_from_slice(&24_600.25_f32.to_le_bytes());
    b[T_OFF_LOW..T_OFF_LOW + 4].copy_from_slice(&24_380.1_f32.to_le_bytes());
    b[T_OFF_PREV_CLOSE..T_OFF_PREV_CLOSE + 4].copy_from_slice(&24_350.00_f32.to_le_bytes());
    b[T_OFF_OI..T_OFF_OI + 8].copy_from_slice(&123_456_789_i64.to_le_bytes());
    b[T_OFF_PREV_OI..T_OFF_PREV_OI + 8].copy_from_slice(&123_000_000_i64.to_le_bytes());
    b[T_OFF_TURNOVER..T_OFF_TURNOVER + 8].copy_from_slice(&5_555_555.5_f64.to_le_bytes());
    b[T_OFF_OHL] = b'H';
    b[T_OFF_SEQ_NO..T_OFF_SEQ_NO + 4].copy_from_slice(&seq_no.to_le_bytes());
    if msg_code == TD_MSG_CODE_TRADE {
        b[T_OFF_BID..T_OFF_BID + 4].copy_from_slice(&24_512.5_f32.to_le_bytes());
        b[T_OFF_BID_QTY..T_OFF_BID_QTY + 4].copy_from_slice(&150_i32.to_le_bytes());
        b[T_OFF_ASK..T_OFF_ASK + 4].copy_from_slice(&24_513.00_f32.to_le_bytes());
        b[T_OFF_ASK_QTY..T_OFF_ASK_QTY + 4].copy_from_slice(&225_i32.to_le_bytes());
    }
    b
}

/// Builds the 10-byte heartbeat frame: `H`@0, status@1, epoch-ms `i64`@2.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test builder, constant offsets bounded by the allocated Vec
fn make_heartbeat_frame() -> Vec<u8> {
    let mut b = vec![0u8; TD_HEARTBEAT_BINARY_LEN];
    b[0] = TD_MSG_CODE_HEARTBEAT;
    b[1] = 1;
    b[2..10].copy_from_slice(&1_754_000_123_456_i64.to_le_bytes());
    b
}

/// Builds the 128-byte auth frame: `A`@0, status@1, message 31B@2,
/// segments 60B@33, `max_symbols` i32@93, subscription 20B@97,
/// validity 10B@117 (v2.6 p.10).
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test builder, constant offsets bounded by the allocated Vec
fn make_auth_frame() -> Vec<u8> {
    let mut b = vec![0u8; TD_AUTH_BINARY_LEN];
    b[0] = TD_MSG_CODE_AUTH;
    b[1] = 1;
    let put = |b: &mut [u8], at: usize, s: &str, cap: usize| {
        let bytes = s.as_bytes();
        let n = bytes.len().min(cap);
        b[at..at + n].copy_from_slice(&bytes[..n]);
    };
    put(&mut b, 2, "Welcome", 31);
    put(&mut b, 33, "NSEEQ,NSEFO", 60);
    b[93..97].copy_from_slice(&500_i32.to_le_bytes());
    put(&mut b, 97, "tick", 20);
    put(&mut b, 117, "2026-12-31", 10);
    b
}

/// Zero-allocation verification for the TrueData binary decode path.
///
/// Budget: EXACTLY 0 blocks. All frames are built BEFORE the measurement
/// window, mirroring the live read task (which decodes out of a buffer the
/// socket already owns).
#[test]
fn dhat_truedata_decode_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // ---- Pre-allocate every input OUTSIDE the measured window -----------
    let trade_t = make_trade_frame(TD_MSG_CODE_TRADE, 4_242, 1_000);
    let trade_w = make_trade_frame(TD_MSG_CODE_TRADE_NO_BIDASK, 4_243, 1_001);
    let heartbeat = make_heartbeat_frame();
    let auth = make_auth_frame();

    // A realistic burst: 200 sequential `T` frames, as a busy tick window.
    let burst: Vec<Vec<u8>> = (0..200)
        .map(|i| make_trade_frame(TD_MSG_CODE_TRADE, 5_000 + i, 2_000 + i))
        .collect();

    // Malformed inputs — the error paths must be alloc-free too (a typed
    // error must never build a message on the hot path).
    let empty: Vec<u8> = Vec::new();
    let truncated_t = trade_t[..40].to_vec(); // `T` but far too short
    let wrong_len_w = {
        // `W` carrying 90 bytes: malformed per §11.2, NOT a silent
        // reinterpretation as a `T`.
        let mut v = trade_t.clone();
        v[OFF_MSG_CODE] = TD_MSG_CODE_TRADE_NO_BIDASK;
        v
    };
    let unknown_code = vec![b'Z'; 32];
    let json_frame = br#"{"symbol":"NIFTY"}"#.to_vec();

    // Every documented msg code (§11.1: thirteen exist, the PDF tabulates
    // four) plus the JSON sniff byte plus an unknown byte.
    let all_codes: Vec<u8> = vec![
        TD_MSG_CODE_AUTH,
        TD_MSG_CODE_TRADE,
        TD_MSG_CODE_TRADE_NO_BIDASK,
        TD_MSG_CODE_BIDASK,
        TD_MSG_CODE_BIDASK_L2,
        TD_MSG_CODE_GREEKS,
        TD_MSG_CODE_BAR_1MIN,
        TD_MSG_CODE_BAR_5MIN,
        TD_MSG_CODE_SYMBOLS_ADDED,
        TD_MSG_CODE_SYMBOLS_REMOVED,
        TD_MSG_CODE_TOUCHLINE,
        TD_MSG_CODE_HEARTBEAT,
        TD_MSG_CODE_MARKET_STATUS,
        TD_JSON_SNIFF_BYTE,
        b'Z',
    ];

    // Warm up one-time lazy init off-clock.
    {
        let warm = decode_trade_binary(&trade_t);
        assert!(warm.is_ok(), "warm-up decode must succeed");
    }

    // ---- Vacuity accumulators -------------------------------------------
    let mut trades_decoded: u32 = 0;
    let mut ltp_sum: f64 = 0.0;
    let mut seq_gaps: u32 = 0;
    let mut seq_contiguous: u32 = 0;
    let mut classified_ok: u32 = 0;
    let mut errors_seen: u32 = 0;
    let mut auth_max_symbols: i32 = 0;

    // ---- Measurement window ---------------------------------------------
    let (_, allocs_during) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        // No-op: the workload resets its own accumulators at the top, so each
        // retry attempt (see dhat_support/mod.rs) re-derives exact counts.
        || {},
        || {
            trades_decoded = 0;
            ltp_sum = 0.0;
            seq_gaps = 0;
            seq_contiguous = 0;
            classified_ok = 0;
            errors_seen = 0;
            auth_max_symbols = 0;

            // 1. Msg-code dispatch over every documented code.
            for code in &all_codes {
                let probe = [*code, 0, 0, 0, 0, 0, 0, 0];
                if classify_frame(black_box(&probe)).is_ok() {
                    classified_ok += 1;
                }
            }

            // 2. `T` — 90 bytes, WITH bid/ask.
            let t = decode_trade_binary(black_box(trade_t.as_slice()))
                .expect("`T` trade frame must decode");
            assert_eq!(t.symbol_id, 4_242);
            assert_eq!(t.seq_no, 1_000);
            assert!(t.has_bid_ask(), "`T` must carry the bid/ask block");
            assert_eq!(t.bid_qty, Some(150));
            assert_eq!(t.ask_qty, Some(225));
            assert_eq!(t.special_tag, b'H');
            assert_eq!(t.oi, 123_456_789);
            ltp_sum += f64::from(t.ltp);
            trades_decoded += 1;

            // 3. `W` — 74 bytes, WITHOUT bid/ask (§11.2: a DISTINCT code).
            let w = decode_trade_binary(black_box(trade_w.as_slice()))
                .expect("`W` trade frame must decode — §11.2");
            assert_eq!(w.symbol_id, 4_243);
            assert!(!w.has_bid_ask(), "`W` must NOT carry the bid/ask block");
            assert_eq!(w.bid, None);
            assert_eq!(w.ask_qty, None);
            ltp_sum += f64::from(w.ltp);
            trades_decoded += 1;

            // 4. Heartbeat (epoch MILLISECONDS — §11.1 dual-width note).
            let hb = decode_heartbeat_binary(black_box(heartbeat.as_slice()))
                .expect("heartbeat must decode");
            assert!(hb.success);
            assert_eq!(hb.timestamp_ms, 1_754_000_123_456);

            // 5. Auth — BORROWED &str fields. An owned String would allocate.
            let a = decode_auth_binary(black_box(auth.as_slice())).expect("auth must decode");
            assert!(a.success);
            auth_max_symbols = a.max_symbols;
            assert!(
                a.message.starts_with("Welcome"),
                "auth message must decode, got {:?}",
                a.message
            );

            // 6. Burst + per-symbol sequence tracking (the vendor loss proof).
            let mut prev: Option<i32> = None;
            for frame in &burst {
                let tr = decode_trade_binary(black_box(frame.as_slice()))
                    .expect("burst frame must decode");
                match check_seq(prev, tr.seq_no) {
                    SeqCheck::Gap { .. } => seq_gaps += 1,
                    SeqCheck::Contiguous => seq_contiguous += 1,
                    _ => {}
                }
                prev = Some(tr.seq_no);
                ltp_sum += f64::from(tr.ltp);
                trades_decoded += 1;
            }

            // 7. Error paths — typed refusals must not allocate.
            for bad in [
                empty.as_slice(),
                truncated_t.as_slice(),
                wrong_len_w.as_slice(),
                unknown_code.as_slice(),
                json_frame.as_slice(),
            ] {
                if decode_trade_binary(black_box(bad)).is_err() {
                    errors_seen += 1;
                }
            }

            // A JSON frame must be CLASSIFIED as JSON, never decoded as bytes.
            assert_eq!(
                classify_frame(black_box(json_frame.as_slice())),
                Ok(TruedataFrameKind::Json),
                "a `{{`-prefixed frame must classify as JSON"
            );
        },
    );

    // ---- VACUITY: prove the workload really decoded ----------------------
    assert_eq!(
        trades_decoded, 202,
        "VACUITY FAILURE: expected 202 decoded trades (1 `T` + 1 `W` + 200 \
         burst), got {trades_decoded}. A zero-alloc result over a workload \
         that decoded nothing proves nothing."
    );
    assert_eq!(
        classified_ok, 14,
        "VACUITY FAILURE: expected 14 of 15 probe bytes to classify (13 \
         documented codes + the JSON sniff byte; `Z` must be refused), got \
         {classified_ok}."
    );
    assert_eq!(
        errors_seen, 5,
        "VACUITY FAILURE: all 5 malformed inputs must be typed errors, got \
         {errors_seen} — a malformed frame was silently accepted."
    );
    assert_eq!(
        seq_contiguous, 199,
        "VACUITY FAILURE: the 200-frame burst has strictly +1 sequence \
         numbers, so 199 contiguous steps were expected (the first is \
         FirstSeen), got {seq_contiguous}."
    );
    assert_eq!(seq_gaps, 0, "VACUITY: a +1 burst must produce no gaps");
    assert_eq!(
        auth_max_symbols, 500,
        "VACUITY: auth max_symbols must decode"
    );
    assert!(
        ltp_sum.is_finite() && ltp_sum > 1_000_000.0,
        "VACUITY FAILURE: summed LTP was {ltp_sum} — the f32 price field was \
         not actually read (an untouched buffer sums to 0)."
    );

    // ---- The ratchet ------------------------------------------------------
    assert_eq!(
        allocs_during, 0,
        "TrueData binary decode allocated {allocs_during} blocks — PRINCIPLE \
         #1 VIOLATED, and truedata-feed-scope-2026-07-24.md §11.7 ratchet #8 \
         BROKEN.\n\
         The binary path must be zero-allocation: fixed-offset `from_le_bytes` \
         field extraction into a stack `TruedataTrade`, borrowed `&str` on \
         `TruedataAuth`, and typed errors that build no message. A non-zero \
         count most likely means an owned `String` appeared on `TruedataAuth`, \
         an error variant started carrying a formatted message, or a decode \
         path began collecting into a Vec."
    );
}
