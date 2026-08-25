//! TrueData (feed #4) wire decoder — binary-first, JSON-fallback.
//!
//! **Ground truth:** `TrueData Market Data API Documentation v 2.6`
//! (20 Feb 2025) pages 10–18, plus the vendor's OFFICIAL reference client
//! (`truedata` 7.0.1, `truedata/websocket/`), as reconciled in
//! `.claude/rules/project/truedata-feed-scope-2026-07-24.md` §11.
//!
//! # Why two paths
//!
//! v2.6 documents EVERY message type in BOTH a JSON and a binary form
//! (p.10 auth, p.11 heartbeat/marketstatus, p.16 trade) but never says how a
//! client picks. **The SDK settles it: the choice is CLIENT-SIDE, not
//! negotiated** — `TD_live(..., compression=False)`, and the vendor's own
//! default is JSON (`TD_live.py:15`; `TD_ws.py:327`). So we support both and
//! dispatch on the first byte:
//!
//! - `b'{'` → JSON frame (zero-alloc positional scan in
//!   [`super::truedata_json`]).
//! - anything else → binary frame, dispatched on the msg-code byte @0 and
//!   validated by length. Fixed-offset `from_le_bytes`, zero-allocation.
//!
//! # ⚠ The binary wire is LZ4-COMPRESSED — decompress BEFORE calling this
//!
//! [`classify_frame`] and every `decode_*_binary` below expect the
//! **DECOMPRESSED** buffer. The SDK's `decompress_data` runs
//! `lz4.block.decompress(data, uncomp_length)` and only THEN reads the msg
//! code from `decompressed[:1]` (`utils.py:427-437`) — raw LZ4 block, no
//! size prefix. Handing raw socket bytes to `classify_frame` in binary mode
//! dispatches on an LZ4 header byte, not a message code.
//!
//! The capture chain is unaffected and deliberately ordered: the **RAW**
//! (still-compressed) frame is WAL'd at receipt, before any decompress or
//! parse, so a decoder bug can never cost us the bytes.
//!
//! # Complexity (honest labels — §11.3)
//!
//! | Operation | Complexity |
//! |---|---|
//! | LZ4 block decompress (binary mode) | **O(frame bytes)**, into a reusable scratch buffer |
//! | JSON positional scan (JSON mode) | **O(frame bytes)**, zero-alloc |
//! | frame-type + length dispatch | **O(1)** |
//! | binary trade FIELD EXTRACTION | **O(1)**, zero-alloc (20 fixed offsets, no loop) |
//!
//! The **field extraction** is O(1); the **frame** is not, in either mode.
//! The sanctioned phrase is "zero-allocation, amortized-constant per tick
//! (O(frame bytes) decode, fixed bound)" — frame size is bounded by the
//! fixed 90-byte shape. Calling the per-frame decode O(1) is a §5 REJECT.
//!
//! # Endianness — LITTLE, CONFIRMED
//!
//! Every `struct.unpack` in the vendor's own decoder uses the `'<'` prefix:
//! `<?` bool, `<i` int, `<Q` long, `<f` float, `<d` double
//! (`utils.py:393-405`). This was the highest-risk unknown in the whole
//! integration — a big-endian wire would have produced plausible-but-wrong
//! prices rather than failing loudly — and it is now closed from the
//! vendor's own format strings. [`TradeSanity::check`] is retained as a
//! runtime regression tripwire, no longer as a blocking probe gate.
//!
//! Note the SDK reads `long_var` as `<Q` (unsigned) where we read `i64`.
//! That is a deliberate, documented deviation (§11.4): the two decodings are
//! bit-identical for every reachable value and differ only when the top bit
//! is set, which for a traded quantity is impossible and therefore evidence
//! of corruption — `u64` would accept it silently, `i64` surfaces it.
//!
//! # No-panic contract
//!
//! Every entry point is total: malformed input returns a typed
//! [`TruedataDecodeError`], never a panic, never an `unwrap`. Length is
//! validated BEFORE any indexed read (the house `read_helpers` contract).

use super::read_helpers::{read_f32_le, read_f64_le, read_i32_le, read_i64_le};

// ---------------------------------------------------------------------------
// Message codes (v2.6 — the binary `Msg Code` byte at offset 0)
// ---------------------------------------------------------------------------

/// Binary message-code byte for the authentication response (v2.6 p.10).
pub const TD_MSG_CODE_AUTH: u8 = b'A';
/// Binary message-code byte for a trade (tick) message (v2.6 p.16).
pub const TD_MSG_CODE_TRADE: u8 = b'T';
/// Binary message-code byte for a heartbeat (v2.6 p.11).
pub const TD_MSG_CODE_HEARTBEAT: u8 = b'H';
/// Binary message-code byte for a market-status push (v2.6 p.11-12).
pub const TD_MSG_CODE_MARKET_STATUS: u8 = b'M';

// The codes below are NOT in the v2.6 PDF's binary tables — they were
// recovered from the vendor's OFFICIAL reference client `msg_map`
// (truedata 7.0.1, `truedata/websocket/compression_map.py`), which is the
// authoritative decoder TrueData ships. Every offset in that map for `T`,
// `A`, `H` and `M` matches this module byte-for-byte, so the map is
// trustworthy for the codes the PDF omits.

/// Trade WITHOUT the bid/ask block — a DISTINCT message code, not a short
/// `T` frame.
///
/// This is the single most important discovery from the SDK map: when an
/// account has bid/ask disabled, TrueData does NOT send a truncated `T` —
/// it sends `W`. Treating the no-bid/ask case as a length variant of `T`
/// (which the PDF's tables alone would lead you to do) means every tick on
/// such an account is dropped as an unknown code.
pub const TD_MSG_CODE_TRADE_NO_BIDASK: u8 = b'W';
/// Standalone bid/ask (L1) message.
pub const TD_MSG_CODE_BIDASK: u8 = b'B';
/// BSE 5-level depth message.
pub const TD_MSG_CODE_BIDASK_L2: u8 = b'D';
/// Option greeks (requires backend enablement).
pub const TD_MSG_CODE_GREEKS: u8 = b'G';
/// 1-minute bar (requires backend enablement; we derive our own).
pub const TD_MSG_CODE_BAR_1MIN: u8 = b'O';
/// 5-minute bar (requires backend enablement; we derive our own).
pub const TD_MSG_CODE_BAR_5MIN: u8 = b'F';
/// `addsymbol` acknowledgement — carries the learned SymbolIDs.
pub const TD_MSG_CODE_SYMBOLS_ADDED: u8 = b'S';
/// Touchline snapshot.
pub const TD_MSG_CODE_TOUCHLINE: u8 = b'L';
/// `removesymbol` acknowledgement.
pub const TD_MSG_CODE_SYMBOLS_REMOVED: u8 = b'R';

/// First byte of a JSON frame — the JSON-vs-binary sniff (§10.6 #1).
pub const TD_JSON_SNIFF_BYTE: u8 = b'{';

// ---------------------------------------------------------------------------
// Frame sizes (v2.6, verbatim "Total Bytes" rows)
// ---------------------------------------------------------------------------

/// Auth response, binary form — 128 bytes (v2.6 p.10).
pub const TD_AUTH_BINARY_LEN: usize = 128;
/// Heartbeat, binary form — 10 bytes (v2.6 p.11).
pub const TD_HEARTBEAT_BINARY_LEN: usize = 10;
/// Market-status push, binary form — 62 bytes (v2.6 p.11-12).
pub const TD_MARKET_STATUS_BINARY_LEN: usize = 62;

/// Trade frame WITH bid/ask — 90 bytes (v2.6 p.16, "Total Bytes 90").
pub const TD_TRADE_BINARY_LEN_FULL: usize = 90;

/// Trade frame WITHOUT bid/ask — 74 bytes, message code `W`.
///
/// **CONFIRMED by the vendor's official reference client** (`truedata` 7.0.1,
/// `truedata/websocket/compression_map.py`): `msg_map["W"]` lists exactly
/// 16 fields ending at `seq_no` (offset 70, 4 bytes) — 74 total — with
/// byte-identical offsets to `T` for every shared field. v2.6 p.16
/// tabulates only the 90-byte `T` form, so this length was originally
/// INFERRED here; the SDK map upgrades it to Verified and, critically,
/// showed it arrives under its OWN code rather than as a short `T`.
pub const TD_TRADE_BINARY_LEN_QUOTE_ONLY: usize = 74;

// ---------------------------------------------------------------------------
// Trade Binary field offsets (v2.6 p.16 — verbatim "Location" column)
// ---------------------------------------------------------------------------

/// Offset of the `Msg Code` byte — identical across EVERY binary frame
/// (auth/trade/heartbeat/marketstatus), which is what makes the O(1)
/// first-byte dispatch in [`classify_frame`] valid.
pub const OFF_MSG_CODE: usize = 0; // char, 1B
const OFF_SYMBOL_ID: usize = 1; // int, 4B
const OFF_TIMESTAMP: usize = 5; // int, 4B  — epoch SECONDS (whole-second!)
const OFF_LTP: usize = 9; // float, 4B
const OFF_VOLUME: usize = 13; // int, 4B   — LTQ (this trade's qty)
const OFF_ATP: usize = 17; // float, 4B
const OFF_TOT_VOLUME: usize = 21; // long, 8B  — TTQ (day cumulative)
const OFF_OPEN: usize = 29; // float, 4B
const OFF_HIGH: usize = 33; // float, 4B
const OFF_LOW: usize = 37; // float, 4B
const OFF_PREV_CLOSE: usize = 41; // float, 4B
const OFF_OI: usize = 45; // long, 8B
const OFF_PREV_OI: usize = 53; // long, 8B
const OFF_TURNOVER: usize = 61; // double, 8B
const OFF_OHL: usize = 69; // char, 1B  — "O"/"H"/"L"/"OHL"/""
const OFF_SEQ_NO: usize = 70; // int, 4B   — the per-symbol loss proof
const OFF_BID: usize = 74; // float, 4B
const OFF_BID_QTY: usize = 78; // int, 4B
const OFF_ASK: usize = 82; // float, 4B
const OFF_ASK_QTY: usize = 86; // int, 4B

// Compile-time proof that the offset map exactly tiles the documented
// frame — the FULL chain, field by field.
//
// Asserting only the frame ends was not the proof the comment claimed: a
// typo in a middle offset (`OFF_HIGH = 34` instead of 33) still ends the
// frame at 90 and still passes, while silently reading High from the wrong
// bytes. That is precisely the silent-corruption class this module exists
// to prevent, so every adjacency is pinned.
const _: () = assert!(OFF_SYMBOL_ID == OFF_MSG_CODE + 1);
const _: () = assert!(OFF_TIMESTAMP == OFF_SYMBOL_ID + 4);
const _: () = assert!(OFF_LTP == OFF_TIMESTAMP + 4);
const _: () = assert!(OFF_VOLUME == OFF_LTP + 4);
const _: () = assert!(OFF_ATP == OFF_VOLUME + 4);
const _: () = assert!(OFF_TOT_VOLUME == OFF_ATP + 4);
const _: () = assert!(OFF_OPEN == OFF_TOT_VOLUME + 8);
const _: () = assert!(OFF_HIGH == OFF_OPEN + 4);
const _: () = assert!(OFF_LOW == OFF_HIGH + 4);
const _: () = assert!(OFF_PREV_CLOSE == OFF_LOW + 4);
const _: () = assert!(OFF_OI == OFF_PREV_CLOSE + 4);
const _: () = assert!(OFF_PREV_OI == OFF_OI + 8);
const _: () = assert!(OFF_TURNOVER == OFF_PREV_OI + 8);
const _: () = assert!(OFF_OHL == OFF_TURNOVER + 8);
const _: () = assert!(OFF_SEQ_NO == OFF_OHL + 1);
const _: () = assert!(OFF_BID == OFF_SEQ_NO + 4);
const _: () = assert!(OFF_BID_QTY == OFF_BID + 4);
const _: () = assert!(OFF_ASK == OFF_BID_QTY + 4);
const _: () = assert!(OFF_ASK_QTY == OFF_ASK + 4);
const _: () = assert!(OFF_ASK_QTY + 4 == TD_TRADE_BINARY_LEN_FULL);
const _: () = assert!(OFF_SEQ_NO + 4 == TD_TRADE_BINARY_LEN_QUOTE_ONLY);
const _: () = assert!(OFF_BID == TD_TRADE_BINARY_LEN_QUOTE_ONLY);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a TrueData frame could not be decoded. Total — never panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruedataDecodeError {
    /// Frame was empty (no msg-code byte to dispatch on).
    Empty,
    /// Frame length does not match any documented shape for its msg code.
    ///
    /// Carries the msg code and the observed length so the caller can log a
    /// coded warning naming both (doc-drift signal, never a panic).
    UnexpectedLength { msg_code: u8, len: usize },
    /// Msg-code byte is not one of the documented codes.
    ///
    /// Counted and dropped — a new vendor message type is a doc-drift
    /// signal, not a fatal error (annexure "no panic on unknown" precedent).
    UnknownMsgCode(u8),
    /// Frame is JSON (starts with `{`), so the binary decoder declined it.
    ///
    /// Not a failure: the caller routes to the JSON path.
    IsJson,
}

// ---------------------------------------------------------------------------
// Decoded trade
// ---------------------------------------------------------------------------

/// One decoded TrueData trade (tick).
///
/// Field types mirror the wire exactly (v2.6 p.16). Widening to storage
/// types happens at the persistence boundary, NOT here — in particular
/// every `f32` price MUST go through `f32_to_f64_clean` (STORAGE-GAP-02)
/// and `tot_volume` MUST be saturated, never `as u32` (§10 / scope §2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruedataTrade {
    /// TrueData numeric symbol id — a **session-scoped routing key only**.
    ///
    /// It is learned from the `addsymbol` reply and MAY be reassigned
    /// across reconnects (UNVERIFIED-LIVE). It is NEVER the persisted
    /// `security_id`; the caller resolves it to a stable name-derived id.
    pub symbol_id: i32,
    /// Exchange timestamp, epoch **SECONDS** (whole-second resolution —
    /// v2.6 p.16). Many trades therefore share one timestamp: the persisted
    /// DEDUP key MUST carry `capture_seq`, or same-second ticks collapse.
    pub timestamp_secs: i32,
    /// Last traded price.
    pub ltp: f32,
    /// Last traded quantity (this trade only) — the doc's `Volume` field.
    pub last_trade_qty: i32,
    /// Average traded price for the day.
    pub atp: f32,
    /// Total traded quantity for the day (cumulative). 8-byte signed.
    pub tot_volume: i64,
    /// Day open.
    pub open: f32,
    /// Day high.
    pub high: f32,
    /// Day low.
    pub low: f32,
    /// Previous day's close.
    pub prev_close: f32,
    /// Open interest.
    pub oi: i64,
    /// Previous day's open interest.
    pub prev_oi: i64,
    /// Day turnover.
    pub turnover: f64,
    /// Special tag byte: `O`/`H`/`L` (or 0 when blank) — flags that this
    /// trade set a new open/high/low.
    pub special_tag: u8,
    /// Per-symbol tick sequence number — the vendor's loss proof
    /// (v2.6 p.16e). Scope (per-symbol vs global), reset and wrap
    /// behaviour are UNDOCUMENTED (§10.6 #3) — the gap detector must treat
    /// a forward jump as a gap and a reset-to-near-zero as a reseed.
    pub seq_no: i32,
    /// Best bid price — `None` when the frame carried no bid/ask block.
    pub bid: Option<f32>,
    /// Best bid quantity — `None` when the frame carried no bid/ask block.
    pub bid_qty: Option<i32>,
    /// Best ask price — `None` when the frame carried no bid/ask block.
    pub ask: Option<f32>,
    /// Best ask quantity — `None` when the frame carried no bid/ask block.
    pub ask_qty: Option<i32>,
}

impl TruedataTrade {
    /// `true` when this frame carried the L1 bid/ask block.
    #[inline]
    #[must_use]
    pub const fn has_bid_ask(&self) -> bool {
        self.bid.is_some()
    }
}

// ---------------------------------------------------------------------------
// Frame classification
// ---------------------------------------------------------------------------

/// What kind of frame arrived, decided by ONE byte — O(1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruedataFrameKind {
    /// JSON frame (first byte `{`) — route to the JSON path.
    Json,
    /// Binary authentication response (128 B).
    AuthBinary,
    /// Binary trade/tick WITH bid/ask (90 B, code `T`).
    TradeBinary,
    /// Binary trade/tick WITHOUT bid/ask (74 B, code `W`) — a distinct
    /// code, not a truncated `T`.
    TradeNoBidAskBinary,
    /// Standalone L1 bid/ask (code `B`).
    BidAskBinary,
    /// BSE 5-level depth (code `D`).
    BidAskL2Binary,
    /// Option greeks (code `G`).
    GreeksBinary,
    /// 1-minute or 5-minute bar (codes `O` / `F`).
    BarBinary,
    /// `addsymbol` / `removesymbol` acknowledgement (codes `S` / `R`).
    SymbolAck,
    /// Touchline snapshot (code `L`).
    TouchlineBinary,
    /// Binary heartbeat (10 B).
    HeartbeatBinary,
    /// Binary market-status push (62 B).
    MarketStatusBinary,
}

/// Classifies a raw frame in O(1) by its first byte.
///
/// This is the single dispatch point that makes the decoder work whether
/// TrueData gives us binary or JSON — the open §10.6 #1 question.
///
/// # Errors
/// [`TruedataDecodeError::Empty`] for a zero-length frame,
/// [`TruedataDecodeError::UnknownMsgCode`] for an undocumented code byte.
#[inline]
pub fn classify_frame(raw: &[u8]) -> Result<TruedataFrameKind, TruedataDecodeError> {
    let Some(&first) = raw.first() else {
        return Err(TruedataDecodeError::Empty);
    };
    match first {
        TD_JSON_SNIFF_BYTE => Ok(TruedataFrameKind::Json),
        TD_MSG_CODE_AUTH => Ok(TruedataFrameKind::AuthBinary),
        TD_MSG_CODE_TRADE => Ok(TruedataFrameKind::TradeBinary),
        TD_MSG_CODE_HEARTBEAT => Ok(TruedataFrameKind::HeartbeatBinary),
        TD_MSG_CODE_MARKET_STATUS => Ok(TruedataFrameKind::MarketStatusBinary),
        TD_MSG_CODE_TRADE_NO_BIDASK => Ok(TruedataFrameKind::TradeNoBidAskBinary),
        TD_MSG_CODE_BIDASK => Ok(TruedataFrameKind::BidAskBinary),
        TD_MSG_CODE_BIDASK_L2 => Ok(TruedataFrameKind::BidAskL2Binary),
        TD_MSG_CODE_GREEKS => Ok(TruedataFrameKind::GreeksBinary),
        TD_MSG_CODE_BAR_1MIN | TD_MSG_CODE_BAR_5MIN => Ok(TruedataFrameKind::BarBinary),
        TD_MSG_CODE_SYMBOLS_ADDED | TD_MSG_CODE_SYMBOLS_REMOVED => Ok(TruedataFrameKind::SymbolAck),
        TD_MSG_CODE_TOUCHLINE => Ok(TruedataFrameKind::TouchlineBinary),
        other => Err(TruedataDecodeError::UnknownMsgCode(other)),
    }
}

// ---------------------------------------------------------------------------
// Trade decode — O(1), zero-alloc
// ---------------------------------------------------------------------------

/// Decodes a 90-byte (or 74-byte, bid/ask-less) TrueData Trade Binary frame.
///
/// **O(1), zero-allocation:** 20 fixed-offset reads, no loop, no heap.
/// Length is validated before any indexed read, so this is total.
///
/// Little-endian is assumed — see the module-level endianness note; pair
/// this with [`TradeSanity::check`] until the vendor confirms byte order.
///
/// # Errors
/// - [`TruedataDecodeError::IsJson`] if the frame is a JSON frame.
/// - [`TruedataDecodeError::UnknownMsgCode`] if byte 0 is not `T`.
/// - [`TruedataDecodeError::UnexpectedLength`] if the length is neither 90
///   nor 74.
pub fn decode_trade_binary(raw: &[u8]) -> Result<TruedataTrade, TruedataDecodeError> {
    // Classify first so a JSON frame is declined cleanly rather than being
    // read as garbage bytes.
    // BOTH trade codes are accepted: `T` carries bid/ask, `W` does not.
    // They are DISTINCT message codes (official SDK msg_map), not a length
    // variant of one code — rejecting `W` would silently drop every tick on
    // a bid/ask-disabled account.
    let msg_code = match classify_frame(raw)? {
        TruedataFrameKind::Json => return Err(TruedataDecodeError::IsJson),
        TruedataFrameKind::TradeBinary => TD_MSG_CODE_TRADE,
        TruedataFrameKind::TradeNoBidAskBinary => TD_MSG_CODE_TRADE_NO_BIDASK,
        _ => {
            // Non-trade binary frame handed to the trade decoder.
            let code = raw.first().copied().unwrap_or(0);
            return Err(TruedataDecodeError::UnknownMsgCode(code));
        }
    };

    let len = raw.len();
    // The code decides whether a bid/ask block EXISTS; the length is then
    // validated against what that code must carry. A `T` of 74 bytes or a
    // `W` of 90 is malformed, not a silent reinterpretation.
    let has_bid_ask = msg_code == TD_MSG_CODE_TRADE;
    let expected_len = if has_bid_ask {
        TD_TRADE_BINARY_LEN_FULL
    } else {
        TD_TRADE_BINARY_LEN_QUOTE_ONLY
    };
    if len != expected_len {
        return Err(TruedataDecodeError::UnexpectedLength { msg_code, len });
    }

    // Length is now proven >= 74, so every offset below is in bounds.
    let (bid, bid_qty, ask, ask_qty) = if has_bid_ask {
        (
            Some(read_f32_le(raw, OFF_BID)),
            Some(read_i32_le(raw, OFF_BID_QTY)),
            Some(read_f32_le(raw, OFF_ASK)),
            Some(read_i32_le(raw, OFF_ASK_QTY)),
        )
    } else {
        (None, None, None, None)
    };

    Ok(TruedataTrade {
        symbol_id: read_i32_le(raw, OFF_SYMBOL_ID),
        timestamp_secs: read_i32_le(raw, OFF_TIMESTAMP),
        ltp: read_f32_le(raw, OFF_LTP),
        last_trade_qty: read_i32_le(raw, OFF_VOLUME),
        atp: read_f32_le(raw, OFF_ATP),
        tot_volume: read_i64_le(raw, OFF_TOT_VOLUME),
        open: read_f32_le(raw, OFF_OPEN),
        high: read_f32_le(raw, OFF_HIGH),
        low: read_f32_le(raw, OFF_LOW),
        prev_close: read_f32_le(raw, OFF_PREV_CLOSE),
        oi: read_i64_le(raw, OFF_OI),
        prev_oi: read_i64_le(raw, OFF_PREV_OI),
        turnover: read_f64_le(raw, OFF_TURNOVER),
        special_tag: raw[OFF_OHL],
        seq_no: read_i32_le(raw, OFF_SEQ_NO),
        bid,
        bid_qty,
        ask,
        ask_qty,
    })
}

// ---------------------------------------------------------------------------
// Structural sanity tripwire (byte-order / framing regression guard)
// ---------------------------------------------------------------------------

/// Verdict of the cheap structural sanity check on a decoded trade.
///
/// A byte-order or framing mistake produces values that are wildly out of
/// range (or NaN) rather than failing loudly — the worst failure class,
/// because it looks like data. This is the tripwire that turns that silent
/// corruption into a detectable, coded refusal.
///
/// Endianness itself is no longer in question — the vendor's own decoder
/// unpacks every field little-endian (§11.1). This check therefore runs as
/// a permanent REGRESSION guard (a mis-set offset, a truncated frame, a
/// future wire change) rather than as the day-0 byte-order probe gate it
/// was originally written to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeSanity {
    /// Every structural invariant held.
    Plausible,
    /// A price field was NaN or infinite.
    NonFinitePrice,
    /// A price was negative (prices are non-negative by domain).
    NegativePrice,
    /// `high < low` — impossible for a real session.
    HighBelowLow,
    /// `ltp` outside `[low, high]` by more than the tolerance.
    LtpOutsideDayRange,
    /// Cumulative day volume was negative.
    NegativeTotVolume,
    /// Turnover was NaN or infinite.
    ///
    /// Turnover is the one `f64` on the frame and was previously exempt
    /// from the finiteness sweep, so a NaN there passed as `Plausible`.
    NonFiniteTurnover,
    /// Open interest (current or previous) was negative.
    ///
    /// Same reasoning as [`Self::NegativeTotVolume`]: the vendor sends OI
    /// unsigned, so a negative is a corrupt read. These two fields had no
    /// tripwire at all.
    NegativeOpenInterest,
}

impl TradeSanity {
    /// `true` only for [`TradeSanity::Plausible`].
    #[inline]
    #[must_use]
    pub const fn is_plausible(self) -> bool {
        matches!(self, Self::Plausible)
    }

    /// Runs the structural checks that a byte-order mistake would violate.
    ///
    /// Deliberately conservative — it must NEVER reject a legitimate frame:
    /// - a zero `high`/`low` (pre-open, no trades yet) is tolerated;
    /// - the `ltp`-in-range check allows a small tolerance for the
    ///   whole-second/last-trade skew.
    ///
    /// It is a TRIPWIRE, not a validator: passing does not prove the byte
    /// order is right, but a big-endian read will fail it with overwhelming
    /// probability on any real market frame.
    #[must_use]
    pub fn check(trade: &TruedataTrade) -> Self {
        for price in [
            trade.ltp,
            trade.open,
            trade.high,
            trade.low,
            trade.prev_close,
        ] {
            if !price.is_finite() {
                return Self::NonFinitePrice;
            }
            if price < 0.0 {
                return Self::NegativePrice;
            }
        }
        if !trade.turnover.is_finite() {
            return Self::NonFiniteTurnover;
        }
        if trade.tot_volume < 0 {
            return Self::NegativeTotVolume;
        }
        // The vendor sends OI unsigned; a negative here is a corrupt read.
        if trade.oi < 0 || trade.prev_oi < 0 {
            return Self::NegativeOpenInterest;
        }
        // Pre-open frames legitimately carry 0 high/low — only compare once
        // the session has actually traded.
        if trade.high > 0.0 && trade.low > 0.0 {
            if trade.high < trade.low {
                return Self::HighBelowLow;
            }
            if trade.ltp > 0.0 {
                // 1% tolerance absorbs the whole-second stamping skew
                // between the day range and this trade's price.
                let tolerance = trade.high * 0.01_f32;
                #[allow(clippy::arithmetic_side_effects)]
                // APPROVED: finite, non-negative, checked above
                let upper = trade.high + tolerance;
                #[allow(clippy::arithmetic_side_effects)]
                // APPROVED: finite, non-negative, checked above
                let lower = trade.low - tolerance;
                if trade.ltp > upper || trade.ltp < lower {
                    return Self::LtpOutsideDayRange;
                }
            }
        }
        Self::Plausible
    }
}

// ---------------------------------------------------------------------------
// Volume widening (never a silent `as u32`)
// ---------------------------------------------------------------------------

/// Saturating conversion of TrueData's 8-byte `Tot Volume` to the shared
/// `ParsedTick.volume` width (`u32`).
///
/// A liquid NIFTY future or index day exceeds `u32::MAX`, and a bare
/// `as u32` would silently wrap to a tiny wrong number. This returns
/// `(value, saturated)` so the caller can increment
/// `tv_truedata_volume_saturated_total` and emit a coded warning naming the
/// symbol — the capped candle is then never silently trusted.
///
/// The proper `u64` widening of the SHARED `ParsedTick.volume` is a
/// separately-scoped cross-feed change (Dhan/Groww regression + ILP DDL +
/// DHAT re-measure) and is deliberately NOT bundled here.
#[inline]
#[must_use]
pub const fn saturate_tot_volume(tot_volume: i64) -> (u32, bool) {
    if tot_volume < 0 {
        // NEGATIVE is nonsensical for cumulative volume. The vendor sends
        // this field unsigned, so a negative here means the top bit is set:
        // a corrupt or mis-framed read, not a real quantity. It is clamped
        // to 0 AND flagged, because a silent zero-volume tick with no
        // counter firing is exactly the invisible-corruption case the flag
        // exists to surface. (A genuine zero is handled below and is NOT
        // flagged — a symbol with no trades yet legitimately has zero.)
        (0, true)
    } else if tot_volume > u32::MAX as i64 {
        (u32::MAX, true)
    } else {
        (tot_volume as u32, false)
    }
}

/// Saturating conversion of TrueData's 4-byte `Volume` (the LTQ — this
/// trade's own quantity) to the shared `ParsedTick.last_trade_quantity`
/// width (`u16`).
///
/// **This is the narrowest conversion in the whole feed and the easiest to
/// get silently wrong.** `u16` tops out at 65,535. A single 100,000-share
/// equity print — an ordinary block trade — would wrap under a bare
/// `as u16` to **34,464**, and a negative (corrupt) value would wrap to
/// something arbitrary. Either way the number looks perfectly reasonable,
/// lands in a candle, and nothing anywhere says it was altered.
///
/// Returns `(value, saturated)` so the caller increments
/// `tv_truedata_ltq_saturated_total` and emits a coded warning naming the
/// symbol. A capped quantity is then a visible, counted degradation rather
/// than an invisible wrong number.
///
/// Widening the SHARED `ParsedTick.last_trade_quantity` to `u32` is the
/// real fix and is a separately-scoped cross-feed change (Dhan and Groww
/// both write this field, plus the ILP schema and a DHAT re-measure), so it
/// is deliberately NOT bundled here.
#[inline]
#[must_use]
pub const fn saturate_last_trade_qty(ltq: i32) -> (u16, bool) {
    if ltq < 0 {
        // The vendor sends this field as a traded quantity; negative means
        // a corrupt or mis-framed read. Clamped AND flagged, for the same
        // reason as `saturate_tot_volume`.
        (0, true)
    } else if ltq > u16::MAX as i32 {
        (u16::MAX, true)
    } else {
        (ltq as u16, false)
    }
}

/// Converts TrueData's signed 4-byte exchange timestamp to the shared
/// `ParsedTick.exchange_timestamp` width (`u32`).
///
/// A bare `as u32` turns a negative (corrupt) timestamp into roughly
/// 4.29 billion — **the year 2106**. The tick then lands in a far-future
/// partition where nobody will ever look at it, and no error is raised:
/// the row is simply gone from every practical query while appearing to
/// have been written successfully.
///
/// Returns `(value, rejected)`. `rejected` means the timestamp was not
/// representable and the caller must DROP the tick rather than persist it
/// — unlike the quantity saturations above, there is no meaningful clamped
/// value for a broken timestamp, and a tick with a fabricated time is worse
/// than no tick.
#[inline]
#[must_use]
pub const fn narrow_exchange_timestamp(timestamp_secs: i32) -> (u32, bool) {
    if timestamp_secs < 0 {
        (0, true)
    } else {
        (timestamp_secs as u32, false)
    }
}

// ---------------------------------------------------------------------------
// Sequence-gap detection (the vendor loss proof)
// ---------------------------------------------------------------------------

/// Outcome of comparing a freshly-decoded `seq_no` against the previous one
/// for the SAME symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqCheck {
    /// Exactly the next sequence number — nothing lost.
    Contiguous,
    /// First frame seen for this symbol this session — baseline established.
    FirstSeen,
    /// Sequence jumped forward: `missing` frames were never delivered.
    ///
    /// This is a REAL loss signal (TD-GAP-01) and is never suppressed.
    Gap { missing: u32 },
    /// Sequence went backwards to near zero — a vendor-side reseed
    /// (reconnect / new session / daily rollover), NOT a gap.
    ///
    /// Reset semantics are UNDOCUMENTED (§10.6 #3): the vendor never states
    /// whether `seq_no` is per-symbol or global, nor when it resets. We
    /// therefore only treat a backwards jump as a reseed, and ALWAYS treat a
    /// forward jump as a gap — the fail-loud direction.
    Reseed,
    /// Same sequence number twice — a duplicate delivery.
    Duplicate,
}

/// Threshold below which a backwards sequence jump is read as a reseed
/// rather than a wrap or reorder.
///
/// Conservative by design: a reseed starts at/near 1, so only the low end
/// qualifies. Anything else backwards is reported as [`SeqCheck::Duplicate`]
/// (out-of-order), never silently swallowed.
pub const TD_SEQ_RESEED_CEILING: i32 = 16;

/// Compares a new `seq_no` against the previous one for the same symbol.
///
/// O(1), allocation-free, total. `previous` is `None` on the first frame.
#[must_use]
pub fn check_seq(previous: Option<i32>, current: i32) -> SeqCheck {
    let Some(prev) = previous else {
        return SeqCheck::FirstSeen;
    };
    if current == prev {
        return SeqCheck::Duplicate;
    }
    if current < prev {
        // Backwards: a reseed starts near zero; anything else is a
        // reorder/duplicate, reported (never silently dropped).
        return if current <= TD_SEQ_RESEED_CEILING {
            SeqCheck::Reseed
        } else {
            SeqCheck::Duplicate
        };
    }
    // Forward. Contiguous iff exactly +1; otherwise a genuine gap.
    let delta = current.saturating_sub(prev);
    if delta == 1 {
        SeqCheck::Contiguous
    } else {
        // delta >= 2 here, so `delta - 1 >= 1` cannot underflow.
        SeqCheck::Gap {
            missing: delta.saturating_sub(1).unsigned_abs(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a synthetic 90-byte Trade Binary frame from the documented
    /// offsets, so the tests exercise the REAL offset map rather than a
    /// hand-copied duplicate of it.
    fn build_trade_frame(
        symbol_id: i32,
        timestamp: i32,
        ltp: f32,
        ltq: i32,
        atp: f32,
        tot_volume: i64,
        open: f32,
        high: f32,
        low: f32,
        prev_close: f32,
        oi: i64,
        prev_oi: i64,
        turnover: f64,
        ohl: u8,
        seq_no: i32,
        bid_ask: Option<(f32, i32, f32, i32)>,
    ) -> Vec<u8> {
        // The SDK's `msg_map` uses two DISTINCT codes, not one code with two
        // lengths: 'T' carries bid/ask (90 B), 'W' does not (74 B).
        let (len, code) = if bid_ask.is_some() {
            (TD_TRADE_BINARY_LEN_FULL, TD_MSG_CODE_TRADE)
        } else {
            (TD_TRADE_BINARY_LEN_QUOTE_ONLY, TD_MSG_CODE_TRADE_NO_BIDASK)
        };
        let mut f = vec![0_u8; len];
        f[OFF_MSG_CODE] = code;
        f[OFF_SYMBOL_ID..OFF_SYMBOL_ID + 4].copy_from_slice(&symbol_id.to_le_bytes());
        f[OFF_TIMESTAMP..OFF_TIMESTAMP + 4].copy_from_slice(&timestamp.to_le_bytes());
        f[OFF_LTP..OFF_LTP + 4].copy_from_slice(&ltp.to_le_bytes());
        f[OFF_VOLUME..OFF_VOLUME + 4].copy_from_slice(&ltq.to_le_bytes());
        f[OFF_ATP..OFF_ATP + 4].copy_from_slice(&atp.to_le_bytes());
        f[OFF_TOT_VOLUME..OFF_TOT_VOLUME + 8].copy_from_slice(&tot_volume.to_le_bytes());
        f[OFF_OPEN..OFF_OPEN + 4].copy_from_slice(&open.to_le_bytes());
        f[OFF_HIGH..OFF_HIGH + 4].copy_from_slice(&high.to_le_bytes());
        f[OFF_LOW..OFF_LOW + 4].copy_from_slice(&low.to_le_bytes());
        f[OFF_PREV_CLOSE..OFF_PREV_CLOSE + 4].copy_from_slice(&prev_close.to_le_bytes());
        f[OFF_OI..OFF_OI + 8].copy_from_slice(&oi.to_le_bytes());
        f[OFF_PREV_OI..OFF_PREV_OI + 8].copy_from_slice(&prev_oi.to_le_bytes());
        f[OFF_TURNOVER..OFF_TURNOVER + 8].copy_from_slice(&turnover.to_le_bytes());
        f[OFF_OHL] = ohl;
        f[OFF_SEQ_NO..OFF_SEQ_NO + 4].copy_from_slice(&seq_no.to_le_bytes());
        if let Some((bid, bid_qty, ask, ask_qty)) = bid_ask {
            f[OFF_BID..OFF_BID + 4].copy_from_slice(&bid.to_le_bytes());
            f[OFF_BID_QTY..OFF_BID_QTY + 4].copy_from_slice(&bid_qty.to_le_bytes());
            f[OFF_ASK..OFF_ASK + 4].copy_from_slice(&ask.to_le_bytes());
            f[OFF_ASK_QTY..OFF_ASK_QTY + 4].copy_from_slice(&ask_qty.to_le_bytes());
        }
        f
    }

    /// The v2.6 p.16 documented sample, as a frame.
    fn sample_frame() -> Vec<u8> {
        build_trade_frame(
            100_000_995,
            1_608_115_952,
            1472.8,
            635,
            1475.83,
            680_949,
            1475.05,
            1484.0,
            1463.0,
            1468.35,
            0,
            0,
            1_004_964_962.67,
            b'O',
            4775,
            Some((1472.8, 429, 1473.3, 34)),
        )
    }

    // --- frame classification ---

    #[test]
    fn test_classify_frame_empty_is_error() {
        assert_eq!(classify_frame(&[]), Err(TruedataDecodeError::Empty));
    }

    #[test]
    fn test_classify_frame_json_by_first_byte() {
        let json = br#"{"trade":["100000995"]}"#;
        assert_eq!(classify_frame(json), Ok(TruedataFrameKind::Json));
    }

    #[test]
    fn test_classify_frame_all_documented_binary_codes() {
        assert_eq!(
            classify_frame(&[TD_MSG_CODE_AUTH]),
            Ok(TruedataFrameKind::AuthBinary)
        );
        assert_eq!(
            classify_frame(&[TD_MSG_CODE_TRADE]),
            Ok(TruedataFrameKind::TradeBinary)
        );
        assert_eq!(
            classify_frame(&[TD_MSG_CODE_HEARTBEAT]),
            Ok(TruedataFrameKind::HeartbeatBinary)
        );
        assert_eq!(
            classify_frame(&[TD_MSG_CODE_MARKET_STATUS]),
            Ok(TruedataFrameKind::MarketStatusBinary)
        );
    }

    #[test]
    fn test_classify_frame_unknown_code_reported() {
        assert_eq!(
            classify_frame(b"Z"),
            Err(TruedataDecodeError::UnknownMsgCode(b'Z'))
        );
    }

    // --- trade decode ---

    /// The v2.6 p.16 sample as RAW BYTES, generated outside this codebase.
    ///
    /// Every other fixture writes fields through the same `OFF_*` constants
    /// the decoder reads back, so swapping two offsets in source keeps all
    /// of them green — the test proves the constants are self-consistent,
    /// not that they are RIGHT. This vector was produced independently
    /// (little-endian `struct.pack` against the documented offsets) and
    /// references no constant, so a swapped offset fails here and only
    /// here. Field values are deliberately all-distinct for the same
    /// reason: identical neighbours make a swap invisible.
    const GOLDEN_TRADE_90: [u8; 90] = [
        0x54, 0xe3, 0xe4, 0xf5, 0x05, 0xf0, 0xe6, 0xd9, 0x5f, 0x9a, //
        0x19, 0xb8, 0x44, 0x7b, 0x02, 0x00, 0x00, 0x8f, 0x7a, 0xb8, //
        0x44, 0xf5, 0x63, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x9a, //
        0x61, 0xb8, 0x44, 0x00, 0x80, 0xb9, 0x44, 0x00, 0xe0, 0xb6, //
        0x44, 0x33, 0x8b, 0xb7, 0x44, 0x6f, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0xde, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x8f, 0xc2, 0x55, 0x31, 0x46, 0xf3, 0xcd, 0x41, 0x4f, //
        0xa7, 0x12, 0x00, 0x00, 0x9a, 0x19, 0xb8, 0x44, 0xad, 0x01, //
        0x00, 0x00, 0x9a, 0x29, 0xb8, 0x44, 0x22, 0x00, 0x00, 0x00, //
    ];

    #[test]
    fn test_decode_trade_binary_golden_bytes_pin_every_offset() {
        let t = decode_trade_binary(&GOLDEN_TRADE_90).expect("golden frame must decode");
        assert_eq!(t.symbol_id, 100_000_995);
        assert_eq!(t.timestamp_secs, 1_608_115_952);
        assert!((t.ltp - 1472.8).abs() < 0.001, "ltp {}", t.ltp);
        assert_eq!(t.last_trade_qty, 635);
        assert!((t.atp - 1475.83).abs() < 0.001, "atp {}", t.atp);
        assert_eq!(t.tot_volume, 680_949);
        assert!((t.open - 1475.05).abs() < 0.001, "open {}", t.open);
        assert!((t.high - 1484.0).abs() < 0.001, "high {}", t.high);
        assert!((t.low - 1463.0).abs() < 0.001, "low {}", t.low);
        assert!(
            (t.prev_close - 1468.35).abs() < 0.001,
            "prev_close {}",
            t.prev_close
        );
        // Distinct values: an oi/prev_oi swap is visible.
        assert_eq!(t.oi, 111);
        assert_eq!(t.prev_oi, 222);
        assert!((t.turnover - 1_004_964_962.67).abs() < 0.01);
        assert_eq!(t.special_tag, b'O');
        assert_eq!(t.seq_no, 4775);
        assert_eq!(t.bid_qty, Some(429));
        assert_eq!(t.ask_qty, Some(34));
        assert!((t.bid.expect("bid") - 1472.8).abs() < 0.001);
        assert!((t.ask.expect("ask") - 1473.3).abs() < 0.001);
    }

    #[test]
    fn test_golden_trade_agrees_with_the_offset_built_fixture() {
        // Ties the two fixture styles together: if the builder and the
        // golden vector ever disagree, one of them is wrong and the build
        // says so instead of quietly trusting the builder.
        let built = build_trade_frame(
            100_000_995,
            1_608_115_952,
            1472.8,
            635,
            1475.83,
            680_949,
            1475.05,
            1484.0,
            1463.0,
            1468.35,
            111,
            222,
            1_004_964_962.67,
            b'O',
            4775,
            Some((1472.8, 429, 1473.3, 34)),
        );
        assert_eq!(
            built.as_slice(),
            GOLDEN_TRADE_90.as_slice(),
            "the offset-built frame must match the independently-generated bytes"
        );
    }

    #[test]
    fn test_decode_trade_binary_sample_roundtrips_fields() {
        let frame = sample_frame();
        let t = decode_trade_binary(&frame).expect("sample frame must decode");
        assert_eq!(t.symbol_id, 100_000_995);
        assert_eq!(t.timestamp_secs, 1_608_115_952);
        assert!((t.ltp - 1472.8).abs() < f32::EPSILON);
        assert_eq!(t.last_trade_qty, 635);
        assert!((t.atp - 1475.83).abs() < f32::EPSILON);
        assert_eq!(t.tot_volume, 680_949);
        assert!((t.open - 1475.05).abs() < f32::EPSILON);
        assert!((t.high - 1484.0).abs() < f32::EPSILON);
        assert!((t.low - 1463.0).abs() < f32::EPSILON);
        assert!((t.prev_close - 1468.35).abs() < f32::EPSILON);
        assert_eq!(t.oi, 0);
        assert_eq!(t.prev_oi, 0);
        assert!((t.turnover - 1_004_964_962.67).abs() < 0.01);
        assert_eq!(t.special_tag, b'O');
        assert_eq!(t.seq_no, 4775);
        assert_eq!(t.bid_qty, Some(429));
        assert_eq!(t.ask_qty, Some(34));
        assert!(t.has_bid_ask());
    }

    #[test]
    fn test_decode_trade_binary_74_byte_no_bid_ask() {
        let frame = build_trade_frame(
            42,
            1_700_000_000,
            100.0,
            1,
            100.0,
            10,
            100.0,
            101.0,
            99.0,
            100.0,
            0,
            0,
            0.0,
            0,
            7,
            None,
        );
        assert_eq!(frame.len(), TD_TRADE_BINARY_LEN_QUOTE_ONLY);
        let t = decode_trade_binary(&frame).expect("74-byte frame must decode");
        assert_eq!(t.seq_no, 7);
        assert!(!t.has_bid_ask());
        assert_eq!(t.bid, None);
        assert_eq!(t.bid_qty, None);
        assert_eq!(t.ask, None);
        assert_eq!(t.ask_qty, None);
    }

    #[test]
    fn test_decode_trade_binary_length_is_dispatched_by_msg_code_not_by_length() {
        // The SDK's msg_map is keyed by CODE: 'T' is ALWAYS 90 bytes, 'W' is
        // ALWAYS 74. A 74-byte 'T' or a 90-byte 'W' is a malformed frame, and
        // must be rejected rather than silently reinterpreted — otherwise a
        // truncated 'T' would decode with garbage bid/ask absent.
        let mut short_t = vec![0_u8; TD_TRADE_BINARY_LEN_QUOTE_ONLY];
        short_t[OFF_MSG_CODE] = TD_MSG_CODE_TRADE;
        assert_eq!(
            decode_trade_binary(&short_t),
            Err(TruedataDecodeError::UnexpectedLength {
                msg_code: TD_MSG_CODE_TRADE,
                len: TD_TRADE_BINARY_LEN_QUOTE_ONLY
            })
        );

        let mut long_w = vec![0_u8; TD_TRADE_BINARY_LEN_FULL];
        long_w[OFF_MSG_CODE] = TD_MSG_CODE_TRADE_NO_BIDASK;
        assert_eq!(
            decode_trade_binary(&long_w),
            Err(TruedataDecodeError::UnexpectedLength {
                msg_code: TD_MSG_CODE_TRADE_NO_BIDASK,
                len: TD_TRADE_BINARY_LEN_FULL
            })
        );
    }

    #[test]
    fn test_decode_trade_binary_rejects_wrong_length() {
        for len in [1_usize, 10, 62, 73, 75, 89, 91, 128, 200] {
            let mut f = vec![0_u8; len];
            f[0] = TD_MSG_CODE_TRADE;
            let got = decode_trade_binary(&f);
            assert_eq!(
                got,
                Err(TruedataDecodeError::UnexpectedLength {
                    msg_code: TD_MSG_CODE_TRADE,
                    len
                }),
                "length {len} must be rejected, not decoded"
            );
        }
    }

    #[test]
    fn test_decode_trade_binary_declines_json_frame() {
        let json = br#"{"trade":["100000995","2020-12-16T14:02:32"]}"#;
        assert_eq!(
            decode_trade_binary(json),
            Err(TruedataDecodeError::IsJson),
            "a JSON frame must be declined, never read as binary garbage"
        );
    }

    #[test]
    fn test_decode_trade_binary_declines_non_trade() {
        let mut auth = vec![0_u8; TD_AUTH_BINARY_LEN];
        auth[0] = TD_MSG_CODE_AUTH;
        assert_eq!(
            decode_trade_binary(&auth),
            Err(TruedataDecodeError::UnknownMsgCode(TD_MSG_CODE_AUTH))
        );
    }

    #[test]
    fn test_decode_trade_binary_empty_is_error() {
        assert_eq!(decode_trade_binary(&[]), Err(TruedataDecodeError::Empty));
    }

    /// Every possible byte value as msg code, and every length 0..=200:
    /// the decoder must return, never panic.
    #[test]
    fn test_decode_trade_binary_never_panics_arbitrary() {
        for code in 0_u16..=255 {
            for len in [0_usize, 1, 9, 74, 90, 128] {
                let mut f = vec![0xAB_u8; len];
                if len > 0 {
                    f[0] = code as u8;
                }
                // Must not panic; result is irrelevant here.
                let _ = decode_trade_binary(&f);
                let _ = classify_frame(&f);
            }
        }
    }

    // --- sanity tripwire (endianness) ---

    #[test]
    fn test_sanity_accepts_the_documented_sample() {
        let t = decode_trade_binary(&sample_frame()).expect("decode");
        assert_eq!(TradeSanity::check(&t), TradeSanity::Plausible);
        assert!(TradeSanity::check(&t).is_plausible());
    }

    #[test]
    fn test_sanity_catches_byte_order_mistake() {
        // Take the documented sample and read it with the WRONG byte order
        // by byte-swapping the price fields in place — this is what a
        // big-endian wire decoded as little-endian looks like.
        let mut frame = sample_frame();
        for off in [OFF_LTP, OFF_OPEN, OFF_HIGH, OFF_LOW, OFF_PREV_CLOSE] {
            frame[off..off + 4].reverse();
        }
        let t = decode_trade_binary(&frame).expect("still structurally decodable");
        assert!(
            !TradeSanity::check(&t).is_plausible(),
            "a byte-order mistake must trip the sanity check, not pass silently"
        );
    }

    #[test]
    fn test_sanity_rejects_non_finite_and_negative_prices() {
        let mut t = decode_trade_binary(&sample_frame()).expect("decode");
        t.ltp = f32::NAN;
        assert_eq!(TradeSanity::check(&t), TradeSanity::NonFinitePrice);
        t.ltp = f32::INFINITY;
        assert_eq!(TradeSanity::check(&t), TradeSanity::NonFinitePrice);
        t.ltp = -1.0;
        assert_eq!(TradeSanity::check(&t), TradeSanity::NegativePrice);
    }

    #[test]
    fn test_sanity_rejects_high_below_low() {
        let mut t = decode_trade_binary(&sample_frame()).expect("decode");
        t.high = 100.0;
        t.low = 200.0;
        assert_eq!(TradeSanity::check(&t), TradeSanity::HighBelowLow);
    }

    #[test]
    fn test_sanity_tolerates_pre_open_zero_high_low() {
        let mut t = decode_trade_binary(&sample_frame()).expect("decode");
        t.high = 0.0;
        t.low = 0.0;
        t.ltp = 0.0;
        assert_eq!(
            TradeSanity::check(&t),
            TradeSanity::Plausible,
            "a pre-open frame with no trades yet is legitimate"
        );
    }

    #[test]
    fn test_sanity_rejects_negative_total_volume() {
        let mut t = decode_trade_binary(&sample_frame()).expect("decode");
        t.tot_volume = -1;
        assert_eq!(TradeSanity::check(&t), TradeSanity::NegativeTotVolume);
    }

    // --- volume saturation ---

    #[test]
    fn test_tot_volume_saturates_instead_of_wrapping() {
        let over = i64::from(u32::MAX) + 1;
        assert_eq!(saturate_tot_volume(over), (u32::MAX, true));
        assert_eq!(saturate_tot_volume(i64::MAX), (u32::MAX, true));
    }

    #[test]
    fn test_tot_volume_passes_through_in_range_values() {
        assert_eq!(saturate_tot_volume(0), (0, false));
        assert_eq!(saturate_tot_volume(680_949), (680_949, false));
        assert_eq!(
            saturate_tot_volume(i64::from(u32::MAX)),
            (u32::MAX, false),
            "exactly u32::MAX is representable and must NOT be flagged"
        );
    }

    #[test]
    fn test_saturate_tot_volume_negative_clamps_to_zero_and_is_flagged() {
        // The vendor sends this field UNSIGNED, so a negative means the top
        // bit is set: a corrupt or mis-framed read, not a quantity. It used
        // to clamp to zero with `saturated = false`, which meant the
        // counter never fired and the tick persisted as a silent
        // zero-volume print — invisible corruption, the worst class.
        assert_eq!(saturate_tot_volume(-1), (0, true));
        assert_eq!(saturate_tot_volume(i64::MIN), (0, true));
    }

    #[test]
    fn test_saturate_last_trade_qty_caps_an_ordinary_block_trade_visibly() {
        // 100,000 shares is an ordinary equity block print, and it does NOT
        // fit in u16. A bare `as u16` gives 34,464 — a plausible-looking
        // number that would land in a candle with nothing flagging it.
        assert_eq!(saturate_last_trade_qty(100_000), (u16::MAX, true));
        assert_eq!(
            100_000_i32 as u16, 34_464,
            "documents the wrap this function exists to prevent"
        );
    }

    #[test]
    fn test_saturate_last_trade_qty_boundaries_and_corruption() {
        assert_eq!(saturate_last_trade_qty(0), (0, false));
        assert_eq!(saturate_last_trade_qty(1), (1, false));
        assert_eq!(
            saturate_last_trade_qty(i32::from(u16::MAX)),
            (u16::MAX, false),
            "exactly at the cap is NOT a saturation"
        );
        assert_eq!(
            saturate_last_trade_qty(i32::from(u16::MAX) + 1),
            (u16::MAX, true),
            "one past the cap IS"
        );
        assert_eq!(saturate_last_trade_qty(-1), (0, true));
        assert_eq!(saturate_last_trade_qty(i32::MIN), (0, true));
        assert_eq!(saturate_last_trade_qty(i32::MAX), (u16::MAX, true));
    }

    #[test]
    fn test_narrow_exchange_timestamp_rejects_rather_than_inventing_2106() {
        // `as u32` on a negative timestamp yields ~4.29e9 — the year 2106.
        // The tick then lands in a partition nobody queries, with no error:
        // effectively deleted while appearing written.
        assert_eq!(narrow_exchange_timestamp(-1), (0, true));
        assert_eq!(narrow_exchange_timestamp(i32::MIN), (0, true));
        assert_eq!(
            -1_i32 as u32, 4_294_967_295,
            "documents the far-future value this function exists to prevent"
        );
        // Real timestamps pass through untouched.
        assert_eq!(narrow_exchange_timestamp(0), (0, false));
        assert_eq!(
            narrow_exchange_timestamp(1_608_115_952),
            (1_608_115_952, false)
        );
        assert_eq!(
            narrow_exchange_timestamp(i32::MAX),
            (2_147_483_647, false),
            "the largest representable stamp is still valid, not rejected"
        );
    }

    #[test]
    fn test_narrow_exchange_timestamp_rejection_is_distinct_from_a_real_zero() {
        // Epoch zero is a legitimate (if odd) value and must not be
        // confused with the rejection sentinel — the flag carries the
        // verdict, never the value.
        let (value_ok, rejected_ok) = narrow_exchange_timestamp(0);
        let (value_bad, rejected_bad) = narrow_exchange_timestamp(-5);
        assert_eq!(value_ok, value_bad, "both clamp to the same value");
        assert!(!rejected_ok, "a real zero is accepted");
        assert!(rejected_bad, "a corrupt one is rejected");
    }

    #[test]
    fn test_saturate_tot_volume_genuine_zero_is_not_flagged() {
        // A symbol with no trades yet legitimately has zero volume; that
        // must NOT trip the corruption counter or the signal is noise.
        assert_eq!(saturate_tot_volume(0), (0, false));
    }

    // --- sequence gap detection ---

    #[test]
    fn test_check_seq_first_frame_establishes_baseline() {
        assert_eq!(check_seq(None, 4775), SeqCheck::FirstSeen);
    }

    #[test]
    fn test_check_seq_contiguous_when_exactly_next() {
        assert_eq!(check_seq(Some(10), 11), SeqCheck::Contiguous);
    }

    #[test]
    fn test_check_seq_forward_jump_is_always_gap() {
        assert_eq!(check_seq(Some(10), 12), SeqCheck::Gap { missing: 1 });
        assert_eq!(check_seq(Some(10), 20), SeqCheck::Gap { missing: 9 });
    }

    #[test]
    fn test_check_seq_duplicate_is_reported() {
        assert_eq!(check_seq(Some(10), 10), SeqCheck::Duplicate);
    }

    #[test]
    fn test_check_seq_reset_near_zero_is_reseed() {
        assert_eq!(check_seq(Some(999_999), 1), SeqCheck::Reseed);
        assert_eq!(
            check_seq(Some(999_999), TD_SEQ_RESEED_CEILING),
            SeqCheck::Reseed
        );
    }

    #[test]
    fn test_check_seq_backwards_not_silently_swallowed() {
        // A large backwards jump is a reorder, not a reseed — it must be
        // reported (as Duplicate/out-of-order), never treated as normal.
        assert_ne!(check_seq(Some(999_999), 500_000), SeqCheck::Reseed);
        assert_eq!(check_seq(Some(999_999), 500_000), SeqCheck::Duplicate);
    }

    #[test]
    fn test_check_seq_forward_gap_after_reseed_fires() {
        // The reseed-suppression must never mask a REAL gap that happens
        // right after a reset (the hostile-review finding).
        assert_eq!(check_seq(Some(1), 5), SeqCheck::Gap { missing: 3 });
    }

    #[test]
    fn test_check_seq_handles_i32_extremes() {
        // Near the 32-bit ceiling: must not panic or wrap.
        let _ = check_seq(Some(i32::MAX), i32::MIN);
        let _ = check_seq(Some(i32::MIN), i32::MAX);
        let _ = check_seq(Some(i32::MAX - 1), i32::MAX);
        assert_eq!(
            check_seq(Some(i32::MAX - 1), i32::MAX),
            SeqCheck::Contiguous
        );
    }

    // --- offset-map integrity ---

    #[test]
    fn test_offset_map_tiles_the_documented_90_byte_frame() {
        // Guards against an offset typo silently shifting every field.
        assert_eq!(OFF_ASK_QTY + 4, TD_TRADE_BINARY_LEN_FULL);
        assert_eq!(OFF_SEQ_NO + 4, TD_TRADE_BINARY_LEN_QUOTE_ONLY);
        assert_eq!(OFF_BID, TD_TRADE_BINARY_LEN_QUOTE_ONLY);
    }

    #[test]
    fn test_documented_frame_lengths_are_pinned() {
        // These are quoted verbatim from v2.6; a drift here means the
        // vendor changed the protocol and every offset needs re-reading.
        assert_eq!(TD_AUTH_BINARY_LEN, 128);
        assert_eq!(TD_HEARTBEAT_BINARY_LEN, 10);
        assert_eq!(TD_MARKET_STATUS_BINARY_LEN, 62);
        assert_eq!(TD_TRADE_BINARY_LEN_FULL, 90);
        assert_eq!(TD_TRADE_BINARY_LEN_QUOTE_ONLY, 74);
    }
}

// ---------------------------------------------------------------------------
// Auth response (binary, 128 B — v2.6 p.10)
// ---------------------------------------------------------------------------

const OFF_AUTH_STATUS: usize = 1; // bool, 1B
const OFF_AUTH_MESSAGE: usize = 2; // str, 31B
const OFF_AUTH_SEGMENT: usize = 33; // str, 60B
const OFF_AUTH_MAX_SYMBOLS: usize = 93; // int, 4B
const OFF_AUTH_SUBSCRIPTION: usize = 97; // str, 20B
const OFF_AUTH_VALIDITY: usize = 117; // str, 10B

/// Widest fixed-width string field in the whole TrueData binary surface —
/// the auth `Segment` field (60 bytes, v2.6 p.10).
///
/// This is the constant that makes the NUL scan in `fixed_str` a bounded
/// operation rather than growth in n, and it is asserted there.
pub const TD_MAX_FIXED_STR_LEN: usize = 60;

const LEN_AUTH_MESSAGE: usize = 31;
const LEN_AUTH_SEGMENT: usize = 60;
const LEN_AUTH_SUBSCRIPTION: usize = 20;
const LEN_AUTH_VALIDITY: usize = 10;

// The documented fields tile bytes 0..127; byte 127 is trailing padding.
const _: () = assert!(OFF_AUTH_VALIDITY + LEN_AUTH_VALIDITY <= TD_AUTH_BINARY_LEN);

/// Decoded TrueData authentication response (v2.6 p.10).
///
/// String fields BORROW from the input frame (zero-allocation); the caller
/// copies only what it keeps. `max_symbols` is the authoritative per-account
/// symbol cap and MUST bound the subscribe set (fail-closed, never silently
/// truncated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruedataAuth<'a> {
    /// `true` when login succeeded.
    pub success: bool,
    /// Server message, e.g. `TrueData Real Time Data Service`. On a
    /// duplicate login this carries `User Already Connected` — the
    /// 1-session-per-key refusal that the SSM lock exists to avoid.
    pub message: &'a str,
    /// Comma/quote-separated enabled segments, e.g. `"NSEEQ","NSEFO"`.
    pub segments: &'a str,
    /// Per-account maximum concurrent symbols.
    pub max_symbols: i32,
    /// Subscription composition, e.g. `tick` / `tick+1min`.
    pub subscription: &'a str,
    /// Subscription validity date (`yyyy-MM-dd`).
    pub validity: &'a str,
}

/// Trims a fixed-width TrueData string field: cuts at the first NUL and
/// strips surrounding whitespace. Lossy on non-UTF-8 bytes is NOT used —
/// invalid UTF-8 yields an empty field rather than a panic or an alloc.
#[inline]
pub(super) fn fixed_str(raw: &[u8], offset: usize, len: usize) -> &str {
    let end = offset.saturating_add(len).min(raw.len());
    if offset >= end {
        return "";
    }
    let field = &raw[offset..end];
    // O(1) EXEMPT: begin
    // Bounded NUL scan over a FIXED-WIDTH field. `len` is always one of the
    // compile-time constants (31 Message / 60 Segment / 20 Subscription /
    // 10 Validity, v2.6 p.10), never input-dependent, so the worst case is
    // 60 byte comparisons forever — a constant upper bound, not growth in
    // n. It is also COLD PATH: the only callers are the auth decode (once
    // per connect) and market-status (a handful per session). The tick hot
    // path is `decode_trade_binary`, which touches no strings at all.
    // Finding a NUL terminator inherently requires inspecting bytes; there
    // is no sub-linear alternative. The debug_assert makes the bound that
    // justifies this exemption fail loudly if a caller ever violates it.
    debug_assert!(
        len <= TD_MAX_FIXED_STR_LEN,
        "fixed_str called with an unbounded length — the O(1) EXEMPT here \
         only holds for the documented fixed-width fields"
    );
    let cut = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    core::str::from_utf8(&field[..cut]).unwrap_or("").trim()
    // O(1) EXEMPT: end
}

/// Decodes the 128-byte binary authentication response (v2.6 p.10).
///
/// O(1) — fixed offsets, zero heap allocation (string fields borrow).
///
/// # Errors
/// [`TruedataDecodeError::UnexpectedLength`] unless the frame is exactly
/// 128 bytes; [`TruedataDecodeError::UnknownMsgCode`] unless byte 0 is `A`.
pub fn decode_auth_binary(raw: &[u8]) -> Result<TruedataAuth<'_>, TruedataDecodeError> {
    let Some(&first) = raw.first() else {
        return Err(TruedataDecodeError::Empty);
    };
    if first == TD_JSON_SNIFF_BYTE {
        return Err(TruedataDecodeError::IsJson);
    }
    if first != TD_MSG_CODE_AUTH {
        return Err(TruedataDecodeError::UnknownMsgCode(first));
    }
    if raw.len() != TD_AUTH_BINARY_LEN {
        return Err(TruedataDecodeError::UnexpectedLength {
            msg_code: TD_MSG_CODE_AUTH,
            len: raw.len(),
        });
    }
    Ok(TruedataAuth {
        success: raw[OFF_AUTH_STATUS] != 0,
        message: fixed_str(raw, OFF_AUTH_MESSAGE, LEN_AUTH_MESSAGE),
        segments: fixed_str(raw, OFF_AUTH_SEGMENT, LEN_AUTH_SEGMENT),
        max_symbols: read_i32_le(raw, OFF_AUTH_MAX_SYMBOLS),
        subscription: fixed_str(raw, OFF_AUTH_SUBSCRIPTION, LEN_AUTH_SUBSCRIPTION),
        validity: fixed_str(raw, OFF_AUTH_VALIDITY, LEN_AUTH_VALIDITY),
    })
}

// ---------------------------------------------------------------------------
// Heartbeat (binary, 10 B — v2.6 p.11)
// ---------------------------------------------------------------------------

const OFF_HB_STATUS: usize = 1; // bool, 1B
const OFF_HB_TIMESTAMP_MS: usize = 2; // long, 8B — epoch MILLISECONDS

const _: () = assert!(OFF_HB_TIMESTAMP_MS + 8 == TD_HEARTBEAT_BINARY_LEN);

/// Decoded heartbeat (v2.6 p.11). Arrives every 5–6 seconds; silence is the
/// feed-stall signal.
///
/// NOTE the asymmetry: the heartbeat timestamp is epoch **MILLISECONDS**,
/// while the trade timestamp is epoch **SECONDS**. Do not share a parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruedataHeartbeat {
    /// Status flag.
    pub success: bool,
    /// Server time, epoch **milliseconds**.
    pub timestamp_ms: i64,
}

/// Decodes the 10-byte binary heartbeat (v2.6 p.11). O(1), zero-alloc.
///
/// # Errors
/// Length/msg-code mismatches as per [`TruedataDecodeError`].
pub fn decode_heartbeat_binary(raw: &[u8]) -> Result<TruedataHeartbeat, TruedataDecodeError> {
    let Some(&first) = raw.first() else {
        return Err(TruedataDecodeError::Empty);
    };
    if first == TD_JSON_SNIFF_BYTE {
        return Err(TruedataDecodeError::IsJson);
    }
    if first != TD_MSG_CODE_HEARTBEAT {
        return Err(TruedataDecodeError::UnknownMsgCode(first));
    }
    if raw.len() != TD_HEARTBEAT_BINARY_LEN {
        return Err(TruedataDecodeError::UnexpectedLength {
            msg_code: TD_MSG_CODE_HEARTBEAT,
            len: raw.len(),
        });
    }
    Ok(TruedataHeartbeat {
        success: raw[OFF_HB_STATUS] != 0,
        timestamp_ms: read_i64_le(raw, OFF_HB_TIMESTAMP_MS),
    })
}

// ---------------------------------------------------------------------------
// Market status (binary, 62 B — v2.6 p.11-12)
// ---------------------------------------------------------------------------

const OFF_MS_STATUS: usize = 1; // bool, 1B
const OFF_MS_DATA: usize = 2; // str, 60B
const LEN_MS_DATA: usize = 60;

const _: () = assert!(OFF_MS_DATA + LEN_MS_DATA == TD_MARKET_STATUS_BINARY_LEN);

/// Decoded market-status push (v2.6 p.11-12), e.g.
/// `NSE EQ Normal Market Start`, `NSE FO Normal Market End`,
/// `MCX Market Status: 1 Session No: 2`.
///
/// This is the vendor's own session-boundary signal — useful as a
/// cross-check against our IST trading calendar, never as a replacement
/// for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruedataMarketStatus<'a> {
    /// Status flag.
    pub success: bool,
    /// Free-text session message (borrowed, zero-alloc).
    pub data: &'a str,
}

/// Decodes the 62-byte binary market-status push. O(1), zero-alloc.
///
/// # Errors
/// Length/msg-code mismatches as per [`TruedataDecodeError`].
pub fn decode_market_status_binary(
    raw: &[u8],
) -> Result<TruedataMarketStatus<'_>, TruedataDecodeError> {
    let Some(&first) = raw.first() else {
        return Err(TruedataDecodeError::Empty);
    };
    if first == TD_JSON_SNIFF_BYTE {
        return Err(TruedataDecodeError::IsJson);
    }
    if first != TD_MSG_CODE_MARKET_STATUS {
        return Err(TruedataDecodeError::UnknownMsgCode(first));
    }
    if raw.len() != TD_MARKET_STATUS_BINARY_LEN {
        return Err(TruedataDecodeError::UnexpectedLength {
            msg_code: TD_MSG_CODE_MARKET_STATUS,
            len: raw.len(),
        });
    }
    Ok(TruedataMarketStatus {
        success: raw[OFF_MS_STATUS] != 0,
        data: fixed_str(raw, OFF_MS_DATA, LEN_MS_DATA),
    })
}

#[cfg(test)]
mod control_frame_tests {
    use super::*;

    fn build_auth_frame(
        success: bool,
        message: &str,
        segments: &str,
        max_symbols: i32,
        subscription: &str,
        validity: &str,
    ) -> Vec<u8> {
        let mut f = vec![0_u8; TD_AUTH_BINARY_LEN];
        f[0] = TD_MSG_CODE_AUTH;
        f[OFF_AUTH_STATUS] = u8::from(success);
        let put = |f: &mut Vec<u8>, off: usize, cap: usize, s: &str| {
            let bytes = s.as_bytes();
            let n = bytes.len().min(cap);
            f[off..off + n].copy_from_slice(&bytes[..n]);
        };
        put(&mut f, OFF_AUTH_MESSAGE, LEN_AUTH_MESSAGE, message);
        put(&mut f, OFF_AUTH_SEGMENT, LEN_AUTH_SEGMENT, segments);
        f[OFF_AUTH_MAX_SYMBOLS..OFF_AUTH_MAX_SYMBOLS + 4]
            .copy_from_slice(&max_symbols.to_le_bytes());
        put(
            &mut f,
            OFF_AUTH_SUBSCRIPTION,
            LEN_AUTH_SUBSCRIPTION,
            subscription,
        );
        put(&mut f, OFF_AUTH_VALIDITY, LEN_AUTH_VALIDITY, validity);
        f
    }

    // --- auth ---

    #[test]
    fn test_decode_auth_binary_documented_sample() {
        // v2.6 p.10 binary sample: maxsymbols 900, subscription Tick+1min.
        let f = build_auth_frame(
            true,
            "TrueData Real Time Data Service",
            "NSEEQ,NSEFO",
            900,
            "Tick+1min",
            "2026-12-31",
        );
        let a = decode_auth_binary(&f).expect("auth must decode");
        assert!(a.success);
        assert_eq!(a.message, "TrueData Real Time Data Service");
        assert_eq!(a.segments, "NSEEQ,NSEFO");
        assert_eq!(a.max_symbols, 900);
        assert_eq!(a.subscription, "Tick+1min");
        assert_eq!(a.validity, "2026-12-31");
    }

    #[test]
    fn test_decode_auth_binary_duplicate_login_refusal() {
        // The 1-session-per-key refusal our SSM lock exists to prevent.
        let f = build_auth_frame(false, "User Already Connected", "", 0, "", "");
        let a = decode_auth_binary(&f).expect("decode");
        assert!(!a.success);
        assert_eq!(a.message, "User Already Connected");
    }

    #[test]
    fn test_decode_auth_binary_max_symbols_is_cap() {
        // maxsymbols must be readable — it bounds the subscribe set
        // fail-closed rather than being silently truncated.
        let f = build_auth_frame(true, "ok", "NSEEQ", 250, "tick", "2026-01-01");
        assert_eq!(decode_auth_binary(&f).expect("decode").max_symbols, 250);
    }

    #[test]
    fn test_decode_auth_binary_rejects_wrong_length_code() {
        let mut short = vec![0_u8; 64];
        short[0] = TD_MSG_CODE_AUTH;
        assert_eq!(
            decode_auth_binary(&short),
            Err(TruedataDecodeError::UnexpectedLength {
                msg_code: TD_MSG_CODE_AUTH,
                len: 64
            })
        );
        let mut wrong = vec![0_u8; TD_AUTH_BINARY_LEN];
        wrong[0] = TD_MSG_CODE_TRADE;
        assert_eq!(
            decode_auth_binary(&wrong),
            Err(TruedataDecodeError::UnknownMsgCode(TD_MSG_CODE_TRADE))
        );
        assert_eq!(decode_auth_binary(&[]), Err(TruedataDecodeError::Empty));
    }

    #[test]
    fn test_decode_auth_binary_declines_json() {
        let json = br#"{"success":true,"message":"TrueData Real Time Data Service"}"#;
        assert_eq!(decode_auth_binary(json), Err(TruedataDecodeError::IsJson));
    }

    #[test]
    fn test_fixed_str_stops_at_nul_and_trims() {
        let f = build_auth_frame(true, "short", "", 1, "", "");
        let a = decode_auth_binary(&f).expect("decode");
        assert_eq!(
            a.message, "short",
            "must stop at the NUL, not include padding"
        );
        assert_eq!(a.segments, "", "an all-NUL field is an empty string");
    }

    #[test]
    fn test_decode_auth_binary_survives_non_utf8() {
        let mut f = build_auth_frame(true, "ok", "", 1, "", "");
        // Invalid UTF-8 in the message field.
        f[OFF_AUTH_MESSAGE] = 0xFF;
        f[OFF_AUTH_MESSAGE + 1] = 0xFE;
        let a = decode_auth_binary(&f).expect("must not panic");
        assert_eq!(
            a.message, "",
            "invalid UTF-8 degrades to empty, never panics"
        );
    }

    // --- heartbeat ---

    #[test]
    fn test_decode_heartbeat_binary_ms_timestamp() {
        // v2.6 p.11 sample value.
        let mut f = vec![0_u8; TD_HEARTBEAT_BINARY_LEN];
        f[0] = TD_MSG_CODE_HEARTBEAT;
        f[OFF_HB_STATUS] = 1;
        f[OFF_HB_TIMESTAMP_MS..OFF_HB_TIMESTAMP_MS + 8]
            .copy_from_slice(&1_730_828_522_088_i64.to_le_bytes());
        let hb = decode_heartbeat_binary(&f).expect("heartbeat must decode");
        assert!(hb.success);
        assert_eq!(hb.timestamp_ms, 1_730_828_522_088);
    }

    #[test]
    fn test_decode_heartbeat_binary_ms_not_seconds() {
        // The asymmetry that must never be collapsed into one parser:
        // a heartbeat ms value read as seconds would be ~55000 years out.
        let mut f = vec![0_u8; TD_HEARTBEAT_BINARY_LEN];
        f[0] = TD_MSG_CODE_HEARTBEAT;
        f[OFF_HB_TIMESTAMP_MS..OFF_HB_TIMESTAMP_MS + 8]
            .copy_from_slice(&1_730_828_522_088_i64.to_le_bytes());
        let hb = decode_heartbeat_binary(&f).expect("decode");
        assert!(
            hb.timestamp_ms > 1_000_000_000_000,
            "heartbeat is epoch MILLISECONDS (13 digits), not seconds"
        );
    }

    #[test]
    fn test_decode_heartbeat_binary_rejects_wrong_length() {
        let mut f = vec![0_u8; 9];
        f[0] = TD_MSG_CODE_HEARTBEAT;
        assert_eq!(
            decode_heartbeat_binary(&f),
            Err(TruedataDecodeError::UnexpectedLength {
                msg_code: TD_MSG_CODE_HEARTBEAT,
                len: 9
            })
        );
    }

    // --- market status ---

    #[test]
    fn test_decode_market_status_binary_session_message() {
        let mut f = vec![0_u8; TD_MARKET_STATUS_BINARY_LEN];
        f[0] = TD_MSG_CODE_MARKET_STATUS;
        f[OFF_MS_STATUS] = 1;
        let msg = b"NSE EQ Normal Market Start";
        f[OFF_MS_DATA..OFF_MS_DATA + msg.len()].copy_from_slice(msg);
        let ms = decode_market_status_binary(&f).expect("market status must decode");
        assert!(ms.success);
        assert_eq!(ms.data, "NSE EQ Normal Market Start");
    }

    #[test]
    fn test_decode_market_status_binary_all_documented() {
        // v2.6 p.11 lists these verbatim; all must decode cleanly.
        for msg in [
            "NSE EQ Pre Open Start",
            "NSE EQ Pre Open End",
            "NSE EQ Normal Market Start",
            "NSE FO Normal Market End",
            "NSE EQ Post Close Start",
            "NSE EQ Post Close End",
            "NSE FO Normal Market Start",
            "MCX Market Status: 5 Session No: 1",
            "MCX Market Status: 1 Session No: 2",
        ] {
            let mut f = vec![0_u8; TD_MARKET_STATUS_BINARY_LEN];
            f[0] = TD_MSG_CODE_MARKET_STATUS;
            f[OFF_MS_STATUS] = 1;
            let b = msg.as_bytes();
            f[OFF_MS_DATA..OFF_MS_DATA + b.len()].copy_from_slice(b);
            let ms = decode_market_status_binary(&f).expect("decode");
            assert_eq!(ms.data, msg);
        }
    }

    #[test]
    fn test_decode_market_status_binary_rejects_length() {
        let mut f = vec![0_u8; 61];
        f[0] = TD_MSG_CODE_MARKET_STATUS;
        assert_eq!(
            decode_market_status_binary(&f),
            Err(TruedataDecodeError::UnexpectedLength {
                msg_code: TD_MSG_CODE_MARKET_STATUS,
                len: 61
            })
        );
    }

    // --- cross-decoder no-panic sweep ---

    #[test]
    fn test_all_control_decoders_never_panic_on_arbitrary_input() {
        for code in 0_u16..=255 {
            for len in [0_usize, 1, 10, 62, 74, 90, 128, 200] {
                let mut f = vec![0x5A_u8; len];
                if len > 0 {
                    f[0] = code as u8;
                }
                let _ = decode_auth_binary(&f);
                let _ = decode_heartbeat_binary(&f);
                let _ = decode_market_status_binary(&f);
            }
        }
    }
}
