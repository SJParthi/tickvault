//! TrueData (feed #4) auxiliary BINARY frame decoders.
//!
//! [`super::truedata`] decodes the frames the v2.6 PDF tabulates: `A` (auth,
//! 128 B), `T` (trade, 90 B), `H` (heartbeat, 10 B), `M` (market status,
//! 62 B). The PDF stops there — but the socket carries **nine more binary
//! message codes**, and a frame whose code we cannot decode is a frame whose
//! content we silently lose.
//!
//! # Ground truth for this module
//!
//! The layouts here come from TrueData's OFFICIAL reference client
//! (`truedata` 7.0.1, `truedata/websocket/compression_map.py`, the `msg_map`
//! table) — the decoder TrueData themselves ship. Its entries for `A`, `T`,
//! `H` and `M` match the PDF **byte for byte**, which is what makes it
//! trustworthy for the codes the PDF omits.
//!
//! | Code | Meaning | Length | Status |
//! |---|---|---|---|
//! | `W` | trade without bid/ask | 74 | decoded in [`super::truedata`] |
//! | `B` | standalone L1 bid/ask | 25 | decoded here |
//! | `G` | option greeks | 57 | decoded here |
//! | `O` | 1-minute bar | 37 | decoded here |
//! | `F` | 5-minute bar | 37 | decoded here |
//! | `S` | `addsymbol` ack (**learns SymbolIDs**) | 10 + n×114 | decoded here |
//! | `L` | touchline snapshot | 10 + n×114 | decoded here |
//! | `R` | `removesymbol` ack | 10 + n×34 | decoded here |
//! | `D` | BSE 5-level depth | — | **REFUSED — see below** |
//!
//! # Why `D` (bidaskL2) is deliberately NOT decoded
//!
//! Two independent, unresolved contradictions in the vendor's own material:
//!
//! 1. **Field order disagrees between the two sources.** The SDK's binary map
//!    orders each level `price, qty, count`; the PDF's prose (p.17 iii)
//!    orders it `#no of bids1, #bidprice1, #bidqty1, #no of ask1, …`, i.e.
//!    `count, price, qty`.
//! 2. **The SDK's own level-2 block is self-inconsistent.** Levels 1, 3, 4
//!    and 5 each span 24 bytes (six 4-byte fields); level 2 spans only 20
//!    (`bid_2`@37 → `bid_3`@57), and inside it `bid_qty_2`@45 collides with
//!    `ask_2`@45 while `no_bid_2`@41 sits out of list order. Either the map
//!    is right and the frame is 137 bytes with an asymmetric level 2, or the
//!    block is a copy-paste corruption whose 4-byte error propagates through
//!    levels 3–5 and the totals, making the frame 141 bytes.
//!
//! A decoder built on either reading has a coin-flip chance of reporting
//! wrong depth prices from level 2 onward. Silently-wrong market depth is
//! strictly worse than absent depth, so this module CLASSIFIES `D`, counts
//! it, and returns [`TruedataAuxDecodeError::UnresolvedLayout`] — loud and
//! visible — rather than guessing. BSE 5-level depth is outside the
//! subscription scope today (we subscribe ticks), so nothing is lost by
//! refusing it; resolving the layout is a day-0 probe item.
//!
//! # Cost
//!
//! Every decoder here is fixed-offset and allocation-free. The two
//! record-list frames (`S`/`L`/`R`) expose their records through a borrowed
//! O(1)-per-`next` iterator, so a 500-symbol ack costs no heap either.

use super::read_helpers::{read_f32_le, read_f64_le, read_i32_le, read_i64_le};
use super::truedata::{
    TD_MSG_CODE_BAR_1MIN, TD_MSG_CODE_BAR_5MIN, TD_MSG_CODE_BIDASK, TD_MSG_CODE_BIDASK_L2,
    TD_MSG_CODE_GREEKS, TD_MSG_CODE_SYMBOLS_ADDED, TD_MSG_CODE_SYMBOLS_REMOVED,
    TD_MSG_CODE_TOUCHLINE, fixed_str,
};

// ---------------------------------------------------------------------------
// Frame lengths
// ---------------------------------------------------------------------------

/// Standalone L1 bid/ask frame (`B`) — 25 bytes.
pub const TD_BIDASK_BINARY_LEN: usize = 25;
/// Option-greeks frame (`G`) — 57 bytes.
pub const TD_GREEKS_BINARY_LEN: usize = 57;
/// Bar frame (`O` 1-min / `F` 5-min) — 37 bytes.
pub const TD_BAR_BINARY_LEN: usize = 37;

/// Fixed header shared by the three record-list frames (`S`, `L`, `R`).
pub const TD_RECORD_LIST_HEADER_LEN: usize = 10;
/// One `add_symbol` record inside an `S`/`L` frame — 114 bytes.
pub const TD_ADD_SYMBOL_RECORD_LEN: usize = 114;
/// One `remove_symbol` record inside an `R` frame — 34 bytes.
pub const TD_REMOVE_SYMBOL_RECORD_LEN: usize = 34;
/// Width of the fixed symbol-name field inside both record shapes.
pub const TD_SYMBOL_NAME_LEN: usize = 30;

// ---------------------------------------------------------------------------
// Offsets — every one copied from the SDK msg_map, verbatim
// ---------------------------------------------------------------------------

// "B": msg_code 0, symbol_id 1, timestamp 5, bid 9, bid_qty 13, ask 17, ask_qty 21
const OFF_BA_SYMBOL_ID: usize = 1;
const OFF_BA_TIMESTAMP: usize = 5;
const OFF_BA_BID: usize = 9;
const OFF_BA_BID_QTY: usize = 13;
const OFF_BA_ASK: usize = 17;
const OFF_BA_ASK_QTY: usize = 21;
// Full adjacency chain — a middle-offset typo must fail the build, not
// silently read the wrong bytes (see the same note in `truedata.rs`).
const _: () = assert!(OFF_BA_TIMESTAMP == OFF_BA_SYMBOL_ID + 4);
const _: () = assert!(OFF_BA_BID == OFF_BA_TIMESTAMP + 4);
const _: () = assert!(OFF_BA_BID_QTY == OFF_BA_BID + 4);
const _: () = assert!(OFF_BA_ASK == OFF_BA_BID_QTY + 4);
const _: () = assert!(OFF_BA_ASK_QTY == OFF_BA_ASK + 4);
const _: () = assert!(OFF_BA_ASK_QTY + 4 == TD_BIDASK_BINARY_LEN);

// "G": symbol_id 1, timestamp 5, iv 9, delta 17, gamma 25, theta 33, vega 41, rho 49
const OFF_GR_SYMBOL_ID: usize = 1;
const OFF_GR_TIMESTAMP: usize = 5;
const OFF_GR_IV: usize = 9;
const OFF_GR_DELTA: usize = 17;
const OFF_GR_GAMMA: usize = 25;
const OFF_GR_THETA: usize = 33;
const OFF_GR_VEGA: usize = 41;
const OFF_GR_RHO: usize = 49;
const _: () = assert!(OFF_GR_TIMESTAMP == OFF_GR_SYMBOL_ID + 4);
const _: () = assert!(OFF_GR_IV == OFF_GR_TIMESTAMP + 4);
const _: () = assert!(OFF_GR_DELTA == OFF_GR_IV + 8);
const _: () = assert!(OFF_GR_GAMMA == OFF_GR_DELTA + 8);
const _: () = assert!(OFF_GR_THETA == OFF_GR_GAMMA + 8);
const _: () = assert!(OFF_GR_VEGA == OFF_GR_THETA + 8);
const _: () = assert!(OFF_GR_RHO == OFF_GR_VEGA + 8);
const _: () = assert!(OFF_GR_RHO + 8 == TD_GREEKS_BINARY_LEN);

// "O"/"F": symbol_id 1, timestamp 5, open 9, high 13, low 17, close 21,
//          volume 25, oi 29 (double)
const OFF_BAR_SYMBOL_ID: usize = 1;
const OFF_BAR_TIMESTAMP: usize = 5;
const OFF_BAR_OPEN: usize = 9;
const OFF_BAR_HIGH: usize = 13;
const OFF_BAR_LOW: usize = 17;
const OFF_BAR_CLOSE: usize = 21;
const OFF_BAR_VOLUME: usize = 25;
const OFF_BAR_OI: usize = 29;
const _: () = assert!(OFF_BAR_TIMESTAMP == OFF_BAR_SYMBOL_ID + 4);
const _: () = assert!(OFF_BAR_OPEN == OFF_BAR_TIMESTAMP + 4);
const _: () = assert!(OFF_BAR_HIGH == OFF_BAR_OPEN + 4);
const _: () = assert!(OFF_BAR_LOW == OFF_BAR_HIGH + 4);
const _: () = assert!(OFF_BAR_CLOSE == OFF_BAR_LOW + 4);
const _: () = assert!(OFF_BAR_VOLUME == OFF_BAR_CLOSE + 4);
const _: () = assert!(OFF_BAR_OI == OFF_BAR_VOLUME + 4);
const _: () = assert!(OFF_BAR_OI + 8 == TD_BAR_BINARY_LEN);

// "S"/"L"/"R" header: msg_code 0, success 1, count 2, total 6, records from 10
const OFF_RL_SUCCESS: usize = 1;
const OFF_RL_COUNT: usize = 2;
const OFF_RL_TOTAL: usize = 6;
const _: () = assert!(OFF_RL_TOTAL + 4 == TD_RECORD_LIST_HEADER_LEN);

// "add_symbol" record, offsets RELATIVE to the record start.
const OFF_AS_SYMBOL_ID: usize = 30;
const OFF_AS_TIMESTAMP: usize = 34;
const OFF_AS_LTP: usize = 38;
const OFF_AS_VOLUME: usize = 42;
const OFF_AS_ATP: usize = 46;
const OFF_AS_TOT_VOLUME: usize = 50;
const OFF_AS_OPEN: usize = 58;
const OFF_AS_HIGH: usize = 62;
const OFF_AS_LOW: usize = 66;
const OFF_AS_PREV_CLOSE: usize = 70;
const OFF_AS_OI: usize = 74;
const OFF_AS_PREV_OI: usize = 82;
const OFF_AS_TURNOVER: usize = 90;
const OFF_AS_BID: usize = 98;
const OFF_AS_BID_QTY: usize = 102;
const OFF_AS_ASK: usize = 106;
const OFF_AS_ASK_QTY: usize = 110;
const _: () = assert!(OFF_AS_TIMESTAMP == OFF_AS_SYMBOL_ID + 4);
const _: () = assert!(OFF_AS_LTP == OFF_AS_TIMESTAMP + 4);
const _: () = assert!(OFF_AS_VOLUME == OFF_AS_LTP + 4);
const _: () = assert!(OFF_AS_ATP == OFF_AS_VOLUME + 4);
const _: () = assert!(OFF_AS_TOT_VOLUME == OFF_AS_ATP + 4);
const _: () = assert!(OFF_AS_OPEN == OFF_AS_TOT_VOLUME + 8);
const _: () = assert!(OFF_AS_HIGH == OFF_AS_OPEN + 4);
const _: () = assert!(OFF_AS_LOW == OFF_AS_HIGH + 4);
const _: () = assert!(OFF_AS_PREV_CLOSE == OFF_AS_LOW + 4);
const _: () = assert!(OFF_AS_OI == OFF_AS_PREV_CLOSE + 4);
const _: () = assert!(OFF_AS_PREV_OI == OFF_AS_OI + 8);
const _: () = assert!(OFF_AS_TURNOVER == OFF_AS_PREV_OI + 8);
const _: () = assert!(OFF_AS_BID == OFF_AS_TURNOVER + 8);
const _: () = assert!(OFF_AS_BID_QTY == OFF_AS_BID + 4);
const _: () = assert!(OFF_AS_ASK == OFF_AS_BID_QTY + 4);
const _: () = assert!(OFF_AS_ASK_QTY == OFF_AS_ASK + 4);
const _: () = assert!(OFF_AS_ASK_QTY + 4 == TD_ADD_SYMBOL_RECORD_LEN);
const _: () = assert!(OFF_AS_SYMBOL_ID == TD_SYMBOL_NAME_LEN);

// "remove_symbol" record: symbol 0 (30 B), symbol_id 30 (4 B)
const OFF_RS_SYMBOL_ID: usize = 30;
const _: () = assert!(OFF_RS_SYMBOL_ID + 4 == TD_REMOVE_SYMBOL_RECORD_LEN);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why an auxiliary TrueData frame could not be decoded. Total — no panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruedataAuxDecodeError {
    /// Frame was empty (no msg-code byte).
    Empty,
    /// Msg-code byte is not the one this decoder handles.
    WrongMsgCode { expected: u8, found: u8 },
    /// Frame length does not match the documented shape for its code.
    UnexpectedLength { msg_code: u8, len: usize },
    /// Record-list frame whose body is not a whole number of records, or
    /// whose declared record count disagrees with the body length.
    ///
    /// Carries the body length and the record width so a coded warning can
    /// name both (doc-drift signal, never a panic).
    RaggedRecordList { body_len: usize, record_len: usize },
    /// The `D` (bidaskL2) frame: the vendor's SDK and PDF give contradictory
    /// layouts and the SDK's own level-2 offsets are self-inconsistent, so we
    /// refuse to guess. See the module docs.
    UnresolvedLayout { msg_code: u8, len: usize },
}

/// Validates the code byte and total length shared by every fixed-size
/// auxiliary frame.
#[inline]
fn check_fixed(
    raw: &[u8],
    expected_code: u8,
    expected_len: usize,
) -> Result<(), TruedataAuxDecodeError> {
    let Some(&found) = raw.first() else {
        return Err(TruedataAuxDecodeError::Empty);
    };
    if found != expected_code {
        return Err(TruedataAuxDecodeError::WrongMsgCode {
            expected: expected_code,
            found,
        });
    }
    if raw.len() != expected_len {
        return Err(TruedataAuxDecodeError::UnexpectedLength {
            msg_code: expected_code,
            len: raw.len(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `B` — standalone L1 bid/ask
// ---------------------------------------------------------------------------

/// A standalone L1 bid/ask update (code `B`, 25 B).
///
/// TrueData sends this when the touch moves without a trade — so on a
/// bid/ask-enabled account the quote can update far more often than the
/// trade stream does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruedataBidAsk {
    /// Session-scoped routing key — never the persisted `security_id`.
    pub symbol_id: i32,
    /// Exchange timestamp, epoch **seconds** (whole-second, as for trades).
    pub timestamp_secs: i32,
    /// Best bid price.
    pub bid: f32,
    /// Best bid quantity.
    pub bid_qty: i32,
    /// Best ask price.
    pub ask: f32,
    /// Best ask quantity.
    pub ask_qty: i32,
}

/// Decodes a `B` bid/ask frame. O(1), zero-allocation.
///
/// # Errors
/// [`TruedataAuxDecodeError::Empty`], `WrongMsgCode` if byte 0 is not `B`,
/// or `UnexpectedLength` if the frame is not exactly 25 bytes.
pub fn decode_bidask_binary(raw: &[u8]) -> Result<TruedataBidAsk, TruedataAuxDecodeError> {
    check_fixed(raw, TD_MSG_CODE_BIDASK, TD_BIDASK_BINARY_LEN)?;
    Ok(TruedataBidAsk {
        symbol_id: read_i32_le(raw, OFF_BA_SYMBOL_ID),
        timestamp_secs: read_i32_le(raw, OFF_BA_TIMESTAMP),
        bid: read_f32_le(raw, OFF_BA_BID),
        bid_qty: read_i32_le(raw, OFF_BA_BID_QTY),
        ask: read_f32_le(raw, OFF_BA_ASK),
        ask_qty: read_i32_le(raw, OFF_BA_ASK_QTY),
    })
}

// ---------------------------------------------------------------------------
// `G` — option greeks
// ---------------------------------------------------------------------------

/// Option greeks (code `G`, 57 B). Requires backend enablement.
///
/// **Field-order warning:** the BINARY frame leads with `iv`, then
/// delta/gamma/theta/vega/rho. The JSON form documented on v2.6 p.17
/// lists `delta, gamma, theta, vega, rho, IV` — IV **last**. Reusing the
/// JSON order to read the binary frame swaps IV with delta.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruedataGreeks {
    /// Session-scoped routing key.
    pub symbol_id: i32,
    /// Exchange timestamp, epoch seconds.
    pub timestamp_secs: i32,
    /// Implied volatility — FIRST on the wire, last in the JSON form.
    pub iv: f64,
    /// Delta.
    pub delta: f64,
    /// Gamma.
    pub gamma: f64,
    /// Theta.
    pub theta: f64,
    /// Vega.
    pub vega: f64,
    /// Rho.
    pub rho: f64,
}

/// Decodes a `G` greeks frame. O(1), zero-allocation.
///
/// # Errors
/// [`TruedataAuxDecodeError::Empty`], `WrongMsgCode` if byte 0 is not `G`,
/// or `UnexpectedLength` if the frame is not exactly 57 bytes.
pub fn decode_greeks_binary(raw: &[u8]) -> Result<TruedataGreeks, TruedataAuxDecodeError> {
    check_fixed(raw, TD_MSG_CODE_GREEKS, TD_GREEKS_BINARY_LEN)?;
    Ok(TruedataGreeks {
        symbol_id: read_i32_le(raw, OFF_GR_SYMBOL_ID),
        timestamp_secs: read_i32_le(raw, OFF_GR_TIMESTAMP),
        iv: read_f64_le(raw, OFF_GR_IV),
        delta: read_f64_le(raw, OFF_GR_DELTA),
        gamma: read_f64_le(raw, OFF_GR_GAMMA),
        theta: read_f64_le(raw, OFF_GR_THETA),
        vega: read_f64_le(raw, OFF_GR_VEGA),
        rho: read_f64_le(raw, OFF_GR_RHO),
    })
}

// ---------------------------------------------------------------------------
// `O` / `F` — vendor bars
// ---------------------------------------------------------------------------

/// Which bar interval a [`TruedataBar`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruedataBarInterval {
    /// 1-minute bar (code `O`).
    OneMinute,
    /// 5-minute bar (code `F`).
    FiveMinute,
}

/// A vendor-computed bar (codes `O` / `F`, 37 B).
///
/// We **derive** our own timeframes from ticks (scope-lock Quote 2), so this
/// stream is never subscribed and never persisted as a candle. The decoder
/// exists so that if the stream is ever enabled backend-side, the frames are
/// understood and counted rather than dropped as an unknown code — and so a
/// vendor bar can be used as an independent cross-check of our own
/// aggregation during the trial.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruedataBar {
    /// Which interval this bar covers.
    pub interval: TruedataBarInterval,
    /// Session-scoped routing key.
    pub symbol_id: i32,
    /// Bar timestamp, epoch seconds. Whether it stamps the bar's OPEN or its
    /// CLOSE is UNDOCUMENTED — a day-0 probe item; never assume.
    pub timestamp_secs: i32,
    /// Bar open.
    pub open: f32,
    /// Bar high.
    pub high: f32,
    /// Bar low.
    pub low: f32,
    /// Bar close.
    pub close: f32,
    /// Bar volume.
    pub volume: i32,
    /// Open interest — a `double` here, unlike the trade frame's `long`.
    pub oi: f64,
}

/// Decodes an `O` (1-min) or `F` (5-min) bar frame. O(1), zero-allocation.
///
/// # Errors
/// [`TruedataAuxDecodeError::Empty`], `WrongMsgCode` if byte 0 is neither
/// `O` nor `F`, or `UnexpectedLength` if the frame is not exactly 37 bytes.
pub fn decode_bar_binary(raw: &[u8]) -> Result<TruedataBar, TruedataAuxDecodeError> {
    let Some(&code) = raw.first() else {
        return Err(TruedataAuxDecodeError::Empty);
    };
    let interval = match code {
        TD_MSG_CODE_BAR_1MIN => TruedataBarInterval::OneMinute,
        TD_MSG_CODE_BAR_5MIN => TruedataBarInterval::FiveMinute,
        found => {
            return Err(TruedataAuxDecodeError::WrongMsgCode {
                expected: TD_MSG_CODE_BAR_1MIN,
                found,
            });
        }
    };
    if raw.len() != TD_BAR_BINARY_LEN {
        return Err(TruedataAuxDecodeError::UnexpectedLength {
            msg_code: code,
            len: raw.len(),
        });
    }
    Ok(TruedataBar {
        interval,
        symbol_id: read_i32_le(raw, OFF_BAR_SYMBOL_ID),
        timestamp_secs: read_i32_le(raw, OFF_BAR_TIMESTAMP),
        open: read_f32_le(raw, OFF_BAR_OPEN),
        high: read_f32_le(raw, OFF_BAR_HIGH),
        low: read_f32_le(raw, OFF_BAR_LOW),
        close: read_f32_le(raw, OFF_BAR_CLOSE),
        volume: read_i32_le(raw, OFF_BAR_VOLUME),
        oi: read_f64_le(raw, OFF_BAR_OI),
    })
}

// ---------------------------------------------------------------------------
// `S` / `L` / `R` — record-list frames
// ---------------------------------------------------------------------------

/// Which record-list frame arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruedataRecordListKind {
    /// `addsymbol` acknowledgement (code `S`) — the SymbolID learning path.
    SymbolsAdded,
    /// Touchline snapshot (code `L`) — same record shape as `S`, pushed
    /// automatically after a subscribe.
    Touchline,
    /// `removesymbol` acknowledgement (code `R`) — narrow 34-byte records.
    SymbolsRemoved,
}

impl TruedataRecordListKind {
    /// Width of one record in this frame's body.
    #[inline]
    #[must_use]
    pub const fn record_len(self) -> usize {
        match self {
            Self::SymbolsAdded | Self::Touchline => TD_ADD_SYMBOL_RECORD_LEN,
            Self::SymbolsRemoved => TD_REMOVE_SYMBOL_RECORD_LEN,
        }
    }
}

/// The decoded header of an `S` / `L` / `R` frame, plus a borrowed handle on
/// its record body.
///
/// Holding the body as a borrowed slice is what keeps a 500-symbol
/// acknowledgement allocation-free: records are yielded lazily by
/// [`TruedataRecordList::add_symbol_records`] /
/// [`TruedataRecordList::remove_symbol_records`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruedataRecordList<'a> {
    /// Which frame this is.
    pub kind: TruedataRecordListKind,
    /// The `success` flag byte, as a bool.
    pub success: bool,
    /// Symbols added (or removed) by the request this acknowledges.
    pub changed_count: i32,
    /// Total symbols now subscribed on this session.
    pub total_symbols: i32,
    /// The record body, already proven to be a whole number of records.
    body: &'a [u8],
}

impl<'a> TruedataRecordList<'a> {
    /// Number of records actually present in the body.
    #[inline]
    #[must_use]
    pub const fn record_count(&self) -> usize {
        // Exact division — the constructor rejected a ragged body, and every
        // `record_len()` arm is a non-zero constant. `checked_div` states that
        // in a form the COMPILER verifies rather than a comment asserting it:
        // the `None` arm is unreachable by construction, and making it so is
        // what lets this whole module DENY unguarded arithmetic instead of
        // carrying a hand-written exception for one line.
        match self.body.len().checked_div(self.kind.record_len()) {
            Some(n) => n,
            None => 0,
        }
    }

    /// Iterates the `add_symbol` records of an `S` or `L` frame.
    ///
    /// Yields nothing for an `R` frame (whose records are the narrow shape) —
    /// use [`Self::remove_symbol_records`] there.
    #[must_use]
    pub fn add_symbol_records(&self) -> AddSymbolRecords<'a> {
        let body = match self.kind {
            TruedataRecordListKind::SymbolsAdded | TruedataRecordListKind::Touchline => self.body,
            TruedataRecordListKind::SymbolsRemoved => &[],
        };
        AddSymbolRecords { body, index: 0 }
    }

    /// Iterates the `remove_symbol` records of an `R` frame.
    ///
    /// Yields nothing for an `S` or `L` frame.
    #[must_use]
    pub fn remove_symbol_records(&self) -> RemoveSymbolRecords<'a> {
        let body = match self.kind {
            TruedataRecordListKind::SymbolsRemoved => self.body,
            TruedataRecordListKind::SymbolsAdded | TruedataRecordListKind::Touchline => &[],
        };
        RemoveSymbolRecords { body, index: 0 }
    }
}

/// One `add_symbol` record (114 B) from an `S` / `L` frame.
///
/// This is the **name → SymbolID binding**: the only place the session
/// learns which numeric id the server will stamp on this symbol's ticks.
/// It must be re-learned on every reconnect — SymbolID stability across
/// sessions is undocumented (scope-lock §10.6 #5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruedataAddSymbolRecord<'a> {
    /// The symbol NAME as we subscribed it (`"NIFTY 50"`, `"RELIANCE_BSE"`).
    pub symbol: &'a str,
    /// The session-scoped numeric id the server assigned to that name.
    pub symbol_id: i32,
    /// Snapshot timestamp, epoch seconds.
    pub timestamp_secs: i32,
    /// Last traded price at snapshot time.
    pub ltp: f32,
    /// Last traded quantity.
    pub last_trade_qty: i32,
    /// Average traded price for the day.
    pub atp: f32,
    /// Total traded quantity for the day.
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
    /// Best bid price.
    pub bid: f32,
    /// Best bid quantity.
    pub bid_qty: i32,
    /// Best ask price.
    pub ask: f32,
    /// Best ask quantity.
    pub ask_qty: i32,
}

/// Lazy iterator over the `add_symbol` records of an `S` / `L` frame.
#[derive(Debug, Clone, Copy)]
pub struct AddSymbolRecords<'a> {
    body: &'a [u8],
    index: usize,
}

impl<'a> Iterator for AddSymbolRecords<'a> {
    type Item = TruedataAddSymbolRecord<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.index.checked_mul(TD_ADD_SYMBOL_RECORD_LEN)?;
        let end = start.checked_add(TD_ADD_SYMBOL_RECORD_LEN)?;
        if end > self.body.len() {
            return None;
        }
        self.index = self.index.saturating_add(1);
        let r = &self.body[start..end];
        Some(TruedataAddSymbolRecord {
            symbol: fixed_str(r, 0, TD_SYMBOL_NAME_LEN),
            symbol_id: read_i32_le(r, OFF_AS_SYMBOL_ID),
            timestamp_secs: read_i32_le(r, OFF_AS_TIMESTAMP),
            ltp: read_f32_le(r, OFF_AS_LTP),
            last_trade_qty: read_i32_le(r, OFF_AS_VOLUME),
            atp: read_f32_le(r, OFF_AS_ATP),
            tot_volume: read_i64_le(r, OFF_AS_TOT_VOLUME),
            open: read_f32_le(r, OFF_AS_OPEN),
            high: read_f32_le(r, OFF_AS_HIGH),
            low: read_f32_le(r, OFF_AS_LOW),
            prev_close: read_f32_le(r, OFF_AS_PREV_CLOSE),
            oi: read_i64_le(r, OFF_AS_OI),
            prev_oi: read_i64_le(r, OFF_AS_PREV_OI),
            turnover: read_f64_le(r, OFF_AS_TURNOVER),
            bid: read_f32_le(r, OFF_AS_BID),
            bid_qty: read_i32_le(r, OFF_AS_BID_QTY),
            ask: read_f32_le(r, OFF_AS_ASK),
            ask_qty: read_i32_le(r, OFF_AS_ASK_QTY),
        })
    }
}

/// One `remove_symbol` record (34 B) from an `R` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruedataRemoveSymbolRecord<'a> {
    /// The symbol NAME that was unsubscribed.
    pub symbol: &'a str,
    /// The session-scoped id it had been bound to.
    pub symbol_id: i32,
}

/// Lazy iterator over the `remove_symbol` records of an `R` frame.
#[derive(Debug, Clone, Copy)]
pub struct RemoveSymbolRecords<'a> {
    body: &'a [u8],
    index: usize,
}

impl<'a> Iterator for RemoveSymbolRecords<'a> {
    type Item = TruedataRemoveSymbolRecord<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.index.checked_mul(TD_REMOVE_SYMBOL_RECORD_LEN)?;
        let end = start.checked_add(TD_REMOVE_SYMBOL_RECORD_LEN)?;
        if end > self.body.len() {
            return None;
        }
        self.index = self.index.saturating_add(1);
        let r = &self.body[start..end];
        Some(TruedataRemoveSymbolRecord {
            symbol: fixed_str(r, 0, TD_SYMBOL_NAME_LEN),
            symbol_id: read_i32_le(r, OFF_RS_SYMBOL_ID),
        })
    }
}

/// Decodes an `S` / `L` / `R` record-list frame header and validates its body.
///
/// The returned value borrows `raw`; records are yielded lazily, so a
/// 500-symbol acknowledgement costs zero allocations.
///
/// # Errors
/// - [`TruedataAuxDecodeError::Empty`] for an empty frame.
/// - `WrongMsgCode` if byte 0 is not `S`, `L` or `R`.
/// - `UnexpectedLength` if the frame is shorter than the 10-byte header.
/// - `RaggedRecordList` if the body is not a whole number of records.
pub fn decode_record_list(raw: &[u8]) -> Result<TruedataRecordList<'_>, TruedataAuxDecodeError> {
    let Some(&code) = raw.first() else {
        return Err(TruedataAuxDecodeError::Empty);
    };
    let kind = match code {
        TD_MSG_CODE_SYMBOLS_ADDED => TruedataRecordListKind::SymbolsAdded,
        TD_MSG_CODE_TOUCHLINE => TruedataRecordListKind::Touchline,
        TD_MSG_CODE_SYMBOLS_REMOVED => TruedataRecordListKind::SymbolsRemoved,
        found => {
            return Err(TruedataAuxDecodeError::WrongMsgCode {
                expected: TD_MSG_CODE_SYMBOLS_ADDED,
                found,
            });
        }
    };
    if raw.len() < TD_RECORD_LIST_HEADER_LEN {
        return Err(TruedataAuxDecodeError::UnexpectedLength {
            msg_code: code,
            len: raw.len(),
        });
    }
    let body = &raw[TD_RECORD_LIST_HEADER_LEN..];
    let record_len = kind.record_len();
    if !body.len().is_multiple_of(record_len) {
        return Err(TruedataAuxDecodeError::RaggedRecordList {
            body_len: body.len(),
            record_len,
        });
    }
    Ok(TruedataRecordList {
        kind,
        success: raw[OFF_RL_SUCCESS] != 0,
        changed_count: read_i32_le(raw, OFF_RL_COUNT),
        total_symbols: read_i32_le(raw, OFF_RL_TOTAL),
        body,
    })
}

// ---------------------------------------------------------------------------
// `D` — deliberately refused
// ---------------------------------------------------------------------------

/// Refuses a `D` (bidaskL2) frame with the reason recorded.
///
/// This is a decoder-shaped **refusal**, not a stub: it exists so the caller
/// has one honest place to route `D` frames — counted and logged, never
/// silently dropped and never decoded from a layout we cannot pin down. See
/// the module docs for the two contradictions that block it.
///
/// # Errors
/// Always. [`TruedataAuxDecodeError::Empty`] for an empty frame,
/// `WrongMsgCode` if byte 0 is not `D`, otherwise
/// [`TruedataAuxDecodeError::UnresolvedLayout`].
pub fn decode_bidask_l2_binary(raw: &[u8]) -> Result<(), TruedataAuxDecodeError> {
    let Some(&found) = raw.first() else {
        return Err(TruedataAuxDecodeError::Empty);
    };
    if found != TD_MSG_CODE_BIDASK_L2 {
        return Err(TruedataAuxDecodeError::WrongMsgCode {
            expected: TD_MSG_CODE_BIDASK_L2,
            found,
        });
    }
    Err(TruedataAuxDecodeError::UnresolvedLayout {
        msg_code: TD_MSG_CODE_BIDASK_L2,
        len: raw.len(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn put_i32(f: &mut [u8], off: usize, v: i32) {
        f[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_i64(f: &mut [u8], off: usize, v: i64) {
        f[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn put_f32(f: &mut [u8], off: usize, v: f32) {
        f[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_f64(f: &mut [u8], off: usize, v: f64) {
        f[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn put_name(f: &mut [u8], off: usize, name: &str) {
        let bytes = name.as_bytes();
        f[off..off + bytes.len()].copy_from_slice(bytes);
    }

    // --- B ---

    #[test]
    fn test_decode_bidask_binary_roundtrips_all_fields() {
        let mut f = vec![0_u8; TD_BIDASK_BINARY_LEN];
        f[0] = TD_MSG_CODE_BIDASK;
        put_i32(&mut f, OFF_BA_SYMBOL_ID, 950_000_606);
        put_i32(&mut f, OFF_BA_TIMESTAMP, 1_608_115_952);
        put_f32(&mut f, OFF_BA_BID, 3698.0);
        put_i32(&mut f, OFF_BA_BID_QTY, 34);
        put_f32(&mut f, OFF_BA_ASK, 3700.0);
        put_i32(&mut f, OFF_BA_ASK_QTY, 54);
        let q = decode_bidask_binary(&f).expect("must decode");
        assert_eq!(q.symbol_id, 950_000_606);
        assert_eq!(q.timestamp_secs, 1_608_115_952);
        assert!((q.bid - 3698.0).abs() < f32::EPSILON);
        assert_eq!(q.bid_qty, 34);
        assert!((q.ask - 3700.0).abs() < f32::EPSILON);
        assert_eq!(q.ask_qty, 54);
    }

    #[test]
    fn test_decode_bidask_binary_rejects_wrong_code_and_length() {
        assert_eq!(
            decode_bidask_binary(&[]),
            Err(TruedataAuxDecodeError::Empty)
        );
        let mut wrong = vec![0_u8; TD_BIDASK_BINARY_LEN];
        wrong[0] = b'T';
        assert_eq!(
            decode_bidask_binary(&wrong),
            Err(TruedataAuxDecodeError::WrongMsgCode {
                expected: TD_MSG_CODE_BIDASK,
                found: b'T'
            })
        );
        for len in [1_usize, 24, 26, 90] {
            let mut f = vec![0_u8; len];
            f[0] = TD_MSG_CODE_BIDASK;
            assert_eq!(
                decode_bidask_binary(&f),
                Err(TruedataAuxDecodeError::UnexpectedLength {
                    msg_code: TD_MSG_CODE_BIDASK,
                    len
                })
            );
        }
    }

    // --- G ---

    #[test]
    fn test_decode_greeks_binary_iv_is_first_not_last() {
        // The JSON form lists IV LAST; the binary frame puts it FIRST. This
        // test is the regression guard against copying the JSON order.
        let mut f = vec![0_u8; TD_GREEKS_BINARY_LEN];
        f[0] = TD_MSG_CODE_GREEKS;
        put_i32(&mut f, OFF_GR_SYMBOL_ID, 7);
        put_i32(&mut f, OFF_GR_TIMESTAMP, 1_700_000_000);
        put_f64(&mut f, OFF_GR_IV, 0.1234);
        put_f64(&mut f, OFF_GR_DELTA, 0.5);
        put_f64(&mut f, OFF_GR_GAMMA, 0.01);
        put_f64(&mut f, OFF_GR_THETA, -1.5);
        put_f64(&mut f, OFF_GR_VEGA, 2.5);
        put_f64(&mut f, OFF_GR_RHO, 0.75);
        let g = decode_greeks_binary(&f).expect("must decode");
        assert!((g.iv - 0.1234).abs() < 1e-12, "IV must come from offset 9");
        assert!((g.delta - 0.5).abs() < 1e-12);
        assert!((g.gamma - 0.01).abs() < 1e-12);
        assert!((g.theta + 1.5).abs() < 1e-12);
        assert!((g.vega - 2.5).abs() < 1e-12);
        assert!((g.rho - 0.75).abs() < 1e-12);
    }

    #[test]
    fn test_decode_greeks_binary_rejects_wrong_shape() {
        let mut f = vec![0_u8; TD_GREEKS_BINARY_LEN - 1];
        f[0] = TD_MSG_CODE_GREEKS;
        assert_eq!(
            decode_greeks_binary(&f),
            Err(TruedataAuxDecodeError::UnexpectedLength {
                msg_code: TD_MSG_CODE_GREEKS,
                len: TD_GREEKS_BINARY_LEN - 1
            })
        );
    }

    // --- O / F ---

    #[test]
    fn test_decode_bar_binary_both_intervals_share_one_layout() {
        for (code, want) in [
            (TD_MSG_CODE_BAR_1MIN, TruedataBarInterval::OneMinute),
            (TD_MSG_CODE_BAR_5MIN, TruedataBarInterval::FiveMinute),
        ] {
            let mut f = vec![0_u8; TD_BAR_BINARY_LEN];
            f[0] = code;
            put_i32(&mut f, OFF_BAR_SYMBOL_ID, 11);
            put_i32(&mut f, OFF_BAR_TIMESTAMP, 1_700_000_060);
            put_f32(&mut f, OFF_BAR_OPEN, 100.0);
            put_f32(&mut f, OFF_BAR_HIGH, 105.0);
            put_f32(&mut f, OFF_BAR_LOW, 99.0);
            put_f32(&mut f, OFF_BAR_CLOSE, 104.0);
            put_i32(&mut f, OFF_BAR_VOLUME, 5000);
            put_f64(&mut f, OFF_BAR_OI, 1234.5);
            let b = decode_bar_binary(&f).expect("must decode");
            assert_eq!(b.interval, want);
            assert_eq!(b.symbol_id, 11);
            assert_eq!(b.timestamp_secs, 1_700_000_060);
            assert!((b.open - 100.0).abs() < f32::EPSILON);
            assert!((b.high - 105.0).abs() < f32::EPSILON);
            assert!((b.low - 99.0).abs() < f32::EPSILON);
            assert!((b.close - 104.0).abs() < f32::EPSILON);
            assert_eq!(b.volume, 5000);
            assert!((b.oi - 1234.5).abs() < 1e-9);
        }
    }

    #[test]
    fn test_decode_bar_binary_rejects_non_bar_code() {
        assert_eq!(decode_bar_binary(&[]), Err(TruedataAuxDecodeError::Empty));
        let mut f = vec![0_u8; TD_BAR_BINARY_LEN];
        f[0] = TD_MSG_CODE_GREEKS;
        assert_eq!(
            decode_bar_binary(&f),
            Err(TruedataAuxDecodeError::WrongMsgCode {
                expected: TD_MSG_CODE_BAR_1MIN,
                found: TD_MSG_CODE_GREEKS
            })
        );
    }

    // --- S / L / R ---

    fn build_add_symbol_frame(code: u8, names: &[(&str, i32)]) -> Vec<u8> {
        let mut f = vec![0_u8; TD_RECORD_LIST_HEADER_LEN + names.len() * TD_ADD_SYMBOL_RECORD_LEN];
        f[0] = code;
        f[OFF_RL_SUCCESS] = 1;
        let count = i32::try_from(names.len()).unwrap_or(i32::MAX);
        put_i32(&mut f, OFF_RL_COUNT, count);
        put_i32(&mut f, OFF_RL_TOTAL, count);
        for (i, (name, id)) in names.iter().enumerate() {
            let base = TD_RECORD_LIST_HEADER_LEN + i * TD_ADD_SYMBOL_RECORD_LEN;
            put_name(&mut f, base, name);
            put_i32(&mut f, base + OFF_AS_SYMBOL_ID, *id);
            put_i32(&mut f, base + OFF_AS_TIMESTAMP, 1_700_000_000);
            put_f32(&mut f, base + OFF_AS_LTP, 42.5);
            put_i64(&mut f, base + OFF_AS_TOT_VOLUME, 999_999_999_999);
            put_f64(&mut f, base + OFF_AS_TURNOVER, 1234.5);
            put_i32(&mut f, base + OFF_AS_ASK_QTY, 7);
        }
        f
    }

    #[test]
    fn test_decode_record_list_learns_every_symbol_id_binding() {
        let f = build_add_symbol_frame(
            TD_MSG_CODE_SYMBOLS_ADDED,
            &[("NIFTY 50", 100), ("INDIA VIX", 200), ("RELIANCE_BSE", 300)],
        );
        let list = decode_record_list(&f).expect("must decode");
        assert_eq!(list.kind, TruedataRecordListKind::SymbolsAdded);
        assert!(list.success);
        assert_eq!(list.changed_count, 3);
        assert_eq!(list.total_symbols, 3);
        assert_eq!(list.record_count(), 3);

        let mut seen = 0_usize;
        for (i, r) in list.add_symbol_records().enumerate() {
            let (want_name, want_id) = match i {
                0 => ("NIFTY 50", 100),
                1 => ("INDIA VIX", 200),
                _ => ("RELIANCE_BSE", 300),
            };
            assert_eq!(r.symbol, want_name);
            assert_eq!(r.symbol_id, want_id);
            assert_eq!(r.timestamp_secs, 1_700_000_000);
            assert!((r.ltp - 42.5).abs() < f32::EPSILON);
            assert_eq!(r.tot_volume, 999_999_999_999);
            assert!((r.turnover - 1234.5).abs() < 1e-9);
            assert_eq!(r.ask_qty, 7);
            seen += 1;
        }
        assert_eq!(seen, 3, "every record must be yielded");
    }

    #[test]
    fn test_decode_record_list_touchline_uses_the_same_record_shape() {
        let f = build_add_symbol_frame(TD_MSG_CODE_TOUCHLINE, &[("SENSEX", 51)]);
        let list = decode_record_list(&f).expect("must decode");
        assert_eq!(list.kind, TruedataRecordListKind::Touchline);
        assert_eq!(list.record_count(), 1);
        let r = list.add_symbol_records().next().expect("one record");
        assert_eq!(r.symbol, "SENSEX");
        assert_eq!(r.symbol_id, 51);
    }

    #[test]
    fn test_decode_record_list_remove_frame_uses_narrow_records() {
        let mut f = vec![0_u8; TD_RECORD_LIST_HEADER_LEN + 2 * TD_REMOVE_SYMBOL_RECORD_LEN];
        f[0] = TD_MSG_CODE_SYMBOLS_REMOVED;
        f[OFF_RL_SUCCESS] = 1;
        put_i32(&mut f, OFF_RL_COUNT, 2);
        put_i32(&mut f, OFF_RL_TOTAL, 8);
        for (i, (name, id)) in [("MINDTREE", 5_i32), ("NIFTY-I", 6)].iter().enumerate() {
            let base = TD_RECORD_LIST_HEADER_LEN + i * TD_REMOVE_SYMBOL_RECORD_LEN;
            put_name(&mut f, base, name);
            put_i32(&mut f, base + OFF_RS_SYMBOL_ID, *id);
        }
        let list = decode_record_list(&f).expect("must decode");
        assert_eq!(list.kind, TruedataRecordListKind::SymbolsRemoved);
        assert_eq!(list.changed_count, 2);
        assert_eq!(list.total_symbols, 8);
        assert_eq!(list.record_count(), 2);
        let got: [Option<TruedataRemoveSymbolRecord<'_>>; 3] = {
            let mut it = list.remove_symbol_records();
            [it.next(), it.next(), it.next()]
        };
        assert_eq!(got[0].expect("first").symbol, "MINDTREE");
        assert_eq!(got[1].expect("second").symbol_id, 6);
        assert!(got[2].is_none(), "iterator must stop at the body end");
    }

    #[test]
    fn test_decode_record_list_iterators_do_not_cross_frame_kinds() {
        // An add-frame must yield no remove-records and vice versa: mixing
        // the two would read a 114-byte record at 34-byte stride.
        let add = build_add_symbol_frame(TD_MSG_CODE_SYMBOLS_ADDED, &[("NIFTY 50", 1)]);
        let list = decode_record_list(&add).expect("must decode");
        assert!(list.remove_symbol_records().next().is_none());

        let mut rem = vec![0_u8; TD_RECORD_LIST_HEADER_LEN + TD_REMOVE_SYMBOL_RECORD_LEN];
        rem[0] = TD_MSG_CODE_SYMBOLS_REMOVED;
        let list = decode_record_list(&rem).expect("must decode");
        assert!(list.add_symbol_records().next().is_none());
    }

    #[test]
    fn test_decode_record_list_empty_body_is_valid_zero_records() {
        let mut f = vec![0_u8; TD_RECORD_LIST_HEADER_LEN];
        f[0] = TD_MSG_CODE_SYMBOLS_ADDED;
        let list = decode_record_list(&f).expect("header-only frame is legal");
        assert_eq!(list.record_count(), 0);
        assert!(list.add_symbol_records().next().is_none());
    }

    #[test]
    fn test_decode_record_list_rejects_ragged_body() {
        let mut f = vec![0_u8; TD_RECORD_LIST_HEADER_LEN + TD_ADD_SYMBOL_RECORD_LEN + 7];
        f[0] = TD_MSG_CODE_SYMBOLS_ADDED;
        assert_eq!(
            decode_record_list(&f),
            Err(TruedataAuxDecodeError::RaggedRecordList {
                body_len: TD_ADD_SYMBOL_RECORD_LEN + 7,
                record_len: TD_ADD_SYMBOL_RECORD_LEN
            })
        );
    }

    #[test]
    fn test_decode_record_list_rejects_short_header_and_wrong_code() {
        assert_eq!(decode_record_list(&[]), Err(TruedataAuxDecodeError::Empty));
        let mut short = vec![0_u8; TD_RECORD_LIST_HEADER_LEN - 1];
        short[0] = TD_MSG_CODE_SYMBOLS_ADDED;
        assert_eq!(
            decode_record_list(&short),
            Err(TruedataAuxDecodeError::UnexpectedLength {
                msg_code: TD_MSG_CODE_SYMBOLS_ADDED,
                len: TD_RECORD_LIST_HEADER_LEN - 1
            })
        );
        let wrong = vec![b'T'; TD_RECORD_LIST_HEADER_LEN];
        assert_eq!(
            decode_record_list(&wrong),
            Err(TruedataAuxDecodeError::WrongMsgCode {
                expected: TD_MSG_CODE_SYMBOLS_ADDED,
                found: b'T'
            })
        );
    }

    #[test]
    fn test_add_symbol_record_name_stops_at_nul_and_survives_max_width() {
        // A 30-char name fills the field with no NUL terminator at all.
        let full = "A".repeat(TD_SYMBOL_NAME_LEN);
        let f = build_add_symbol_frame(TD_MSG_CODE_SYMBOLS_ADDED, &[(full.as_str(), 1)]);
        let list = decode_record_list(&f).expect("must decode");
        let r = list.add_symbol_records().next().expect("one record");
        assert_eq!(r.symbol.len(), TD_SYMBOL_NAME_LEN);
        assert!(r.symbol.chars().all(|c| c == 'A'));
    }

    // --- D ---

    #[test]
    fn test_decode_bidask_l2_binary_refuses_rather_than_guessing() {
        // Both candidate total lengths must be refused: the layout is
        // unresolved, so neither 137 nor 141 may be decoded.
        for len in [137_usize, 141] {
            let mut f = vec![0_u8; len];
            f[0] = TD_MSG_CODE_BIDASK_L2;
            assert_eq!(
                decode_bidask_l2_binary(&f),
                Err(TruedataAuxDecodeError::UnresolvedLayout {
                    msg_code: TD_MSG_CODE_BIDASK_L2,
                    len
                })
            );
        }
        assert_eq!(
            decode_bidask_l2_binary(&[]),
            Err(TruedataAuxDecodeError::Empty)
        );
        assert_eq!(
            decode_bidask_l2_binary(b"T"),
            Err(TruedataAuxDecodeError::WrongMsgCode {
                expected: TD_MSG_CODE_BIDASK_L2,
                found: b'T'
            })
        );
    }

    // --- cross-cutting ---

    #[test]
    fn test_record_count_trusts_the_body_not_the_declared_header_count() {
        // The header's count field is the SERVER's claim; the body is the
        // fact. If they disagree we must iterate what actually arrived —
        // trusting the claim would read past the body or skip records.
        let mut f = build_add_symbol_frame(TD_MSG_CODE_SYMBOLS_ADDED, &[("A", 1), ("B", 2)]);
        put_i32(&mut f, OFF_RL_COUNT, 99); // server claims 99, body has 2
        let list = decode_record_list(&f).expect("must decode");
        assert_eq!(list.changed_count, 99, "the claim is reported verbatim");
        assert_eq!(list.record_count(), 2, "the body is what is counted");
        assert_eq!(list.add_symbol_records().count(), 2);
    }

    #[test]
    fn test_add_symbol_records_yields_exactly_record_count_items() {
        for n in [0_usize, 1, 2, 5] {
            let names: Vec<(String, i32)> = (0..n)
                .map(|i| (format!("SYM{i}"), i32::try_from(i).unwrap_or(0)))
                .collect();
            let refs: Vec<(&str, i32)> = names.iter().map(|(s, i)| (s.as_str(), *i)).collect();
            let f = build_add_symbol_frame(TD_MSG_CODE_SYMBOLS_ADDED, &refs);
            let list = decode_record_list(&f).expect("must decode");
            assert_eq!(list.record_count(), n);
            assert_eq!(list.add_symbol_records().count(), n, "n = {n}");
        }
    }

    #[test]
    fn test_remove_symbol_records_yields_exactly_record_count_items() {
        for n in [0_usize, 1, 3] {
            let mut f = vec![0_u8; TD_RECORD_LIST_HEADER_LEN + n * TD_REMOVE_SYMBOL_RECORD_LEN];
            f[0] = TD_MSG_CODE_SYMBOLS_REMOVED;
            for i in 0..n {
                let base = TD_RECORD_LIST_HEADER_LEN + i * TD_REMOVE_SYMBOL_RECORD_LEN;
                put_i32(
                    &mut f,
                    base + OFF_RS_SYMBOL_ID,
                    i32::try_from(i).unwrap_or(0),
                );
            }
            let list = decode_record_list(&f).expect("must decode");
            assert_eq!(list.record_count(), n);
            assert_eq!(list.remove_symbol_records().count(), n, "n = {n}");
        }
    }

    #[test]
    fn test_record_len_matches_the_frame_kind() {
        assert_eq!(
            TruedataRecordListKind::SymbolsAdded.record_len(),
            TD_ADD_SYMBOL_RECORD_LEN
        );
        assert_eq!(
            TruedataRecordListKind::Touchline.record_len(),
            TD_ADD_SYMBOL_RECORD_LEN
        );
        assert_eq!(
            TruedataRecordListKind::SymbolsRemoved.record_len(),
            TD_REMOVE_SYMBOL_RECORD_LEN
        );
    }

    #[test]
    fn test_documented_frame_lengths_are_pinned() {
        // Pin the SDK-sourced lengths so a silent edit fails the build.
        assert_eq!(TD_BIDASK_BINARY_LEN, 25);
        assert_eq!(TD_GREEKS_BINARY_LEN, 57);
        assert_eq!(TD_BAR_BINARY_LEN, 37);
        assert_eq!(TD_RECORD_LIST_HEADER_LEN, 10);
        assert_eq!(TD_ADD_SYMBOL_RECORD_LEN, 114);
        assert_eq!(TD_REMOVE_SYMBOL_RECORD_LEN, 34);
        assert_eq!(TD_SYMBOL_NAME_LEN, 30);
    }
}
