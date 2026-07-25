//! TrueData (feed #4) frame router — the single accounted dispatch point.
//!
//! [`super::truedata`] and [`super::truedata_aux`] each decode one family of
//! frames. This module is the ONE place that looks at an inbound frame and
//! decides which of them handles it — and, critically, the one place that
//! **accounts for every frame**.
//!
//! # Why accounting, not just dispatch
//!
//! The operator's mandate is "not even miss even a single tick". A decoder
//! that returns `Err` on an unrecognised frame satisfies its own contract
//! while the caller quietly drops the frame — and nothing anywhere records
//! that it happened. That is the failure mode this module exists to make
//! impossible: [`FrameLedger`] counts every routing outcome, and
//! [`FrameLedger::is_conserved`] proves arithmetically that
//! `received == decoded + refused`. A frame cannot leave this module
//! unaccounted for.
//!
//! Note what the ledger does and does not claim. It proves that every frame
//! we RECEIVED was classified and counted — never that every exchange trade
//! reached us. Upstream conflation is measured separately by the vendor's
//! per-symbol `Sequence No` (`super::truedata::check_seq`) and, at the top,
//! by the day-0 cadence probe.
//!
//! # ⚠ Input must already be decompressed
//!
//! In binary mode the wire is LZ4-block compressed (scope-lock §11.1): the
//! caller decompresses into its reusable scratch buffer and passes the
//! DECOMPRESSED bytes here. Passing raw socket bytes routes on an LZ4
//! header byte. The RAW frame is WAL'd at receipt, before this call.
//!
//! # Cost
//!
//! Routing is O(1) — one byte of dispatch plus the chosen decoder. No
//! allocation: decoded frames borrow the input where they carry strings.

use super::truedata::{
    TruedataAuth, TruedataDecodeError, TruedataFrameKind, TruedataHeartbeat, TruedataMarketStatus,
    TruedataTrade, classify_frame, decode_auth_binary, decode_heartbeat_binary,
    decode_market_status_binary, decode_trade_binary,
};
use super::truedata_aux::{
    TruedataAuxDecodeError, TruedataBar, TruedataBidAsk, TruedataGreeks, TruedataRecordList,
    decode_bar_binary, decode_bidask_binary, decode_bidask_l2_binary, decode_greeks_binary,
    decode_record_list,
};

// ---------------------------------------------------------------------------
// Routed frame
// ---------------------------------------------------------------------------

/// A successfully routed TrueData frame.
///
/// Borrows the input buffer for the variants that carry strings, so routing
/// a 500-symbol acknowledgement still allocates nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TruedataFrame<'a> {
    /// Authentication response — carries `maxsymbols`, segments, validity.
    Auth(TruedataAuth<'a>),
    /// A tick, with or without the L1 bid/ask block (codes `T` / `W`).
    Trade(TruedataTrade),
    /// A standalone L1 quote update (code `B`).
    BidAsk(TruedataBidAsk),
    /// Option greeks (code `G`).
    Greeks(TruedataGreeks),
    /// A vendor-computed bar (codes `O` / `F`) — never persisted as one of
    /// our candles; we derive timeframes from ticks.
    Bar(TruedataBar),
    /// `addsymbol` / touchline / `removesymbol` (codes `S` / `L` / `R`) —
    /// the SymbolID learning path.
    RecordList(TruedataRecordList<'a>),
    /// Server heartbeat (code `H`) — feeds the stall watchdog.
    Heartbeat(TruedataHeartbeat),
    /// Market-status push (code `M`).
    MarketStatus(TruedataMarketStatus<'a>),
    /// The frame is JSON; the caller routes it to the JSON decoder.
    ///
    /// Routed, not refused: JSON is a first-class transport mode
    /// (scope-lock §11.1), not an error.
    Json,
}

/// Why a frame could not be routed to a decoder.
///
/// Every variant is counted by [`FrameLedger`]; none is silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRefusal {
    /// Zero-length frame — nothing to dispatch on.
    Empty,
    /// The msg-code byte matches no documented code.
    ///
    /// A doc-drift signal (a new vendor message type), never fatal — the
    /// annexure "no panic on unknown" precedent.
    UnknownMsgCode(u8),
    /// The code is known but the frame's shape is wrong for it.
    Malformed { msg_code: u8, len: usize },
    /// The code is known and the frame well-formed, but its field layout is
    /// not resolvable from the vendor's own material — today only `D`
    /// (BSE 5-level depth). Refused rather than guessed: silently-wrong
    /// depth is strictly worse than absent depth (scope-lock §11.6).
    UnresolvedLayout { msg_code: u8, len: usize },
}

impl RouteRefusal {
    /// The msg code this refusal concerns, when the frame had one.
    #[inline]
    #[must_use]
    pub const fn msg_code(self) -> Option<u8> {
        match self {
            Self::Empty => None,
            Self::UnknownMsgCode(code) => Some(code),
            Self::Malformed { msg_code, .. } | Self::UnresolvedLayout { msg_code, .. } => {
                Some(msg_code)
            }
        }
    }

    /// A short, stable label for logs and counter dimensions.
    #[inline]
    #[must_use]
    pub const fn reason_label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::UnknownMsgCode(_) => "unknown_code",
            Self::Malformed { .. } => "malformed",
            Self::UnresolvedLayout { .. } => "unresolved_layout",
        }
    }
}

/// Translates a core-decoder error into a routing refusal.
#[inline]
const fn refuse_core(err: TruedataDecodeError, fallback_code: u8, len: usize) -> RouteRefusal {
    match err {
        TruedataDecodeError::Empty => RouteRefusal::Empty,
        TruedataDecodeError::UnknownMsgCode(code) => RouteRefusal::UnknownMsgCode(code),
        TruedataDecodeError::UnexpectedLength { msg_code, len } => {
            RouteRefusal::Malformed { msg_code, len }
        }
        // A JSON frame reaching a binary decoder means classification and
        // dispatch disagreed — a routing bug, reported as malformed rather
        // than swallowed.
        TruedataDecodeError::IsJson => RouteRefusal::Malformed {
            msg_code: fallback_code,
            len,
        },
    }
}

/// Translates an auxiliary-decoder error into a routing refusal.
#[inline]
const fn refuse_aux(err: TruedataAuxDecodeError, code: u8, len: usize) -> RouteRefusal {
    match err {
        TruedataAuxDecodeError::Empty => RouteRefusal::Empty,
        TruedataAuxDecodeError::WrongMsgCode { found, .. } => RouteRefusal::UnknownMsgCode(found),
        TruedataAuxDecodeError::UnexpectedLength { msg_code, len } => {
            RouteRefusal::Malformed { msg_code, len }
        }
        // Carry the REAL code. Reporting 0 here rendered as a NUL byte and
        // was indistinguishable from a genuine code-0 frame, discarding the
        // one datum a doc-drift investigation needs: WHICH record-list
        // frame changed shape.
        TruedataAuxDecodeError::RaggedRecordList { .. } => RouteRefusal::Malformed {
            msg_code: code,
            len,
        },
        TruedataAuxDecodeError::UnresolvedLayout { msg_code, len } => {
            RouteRefusal::UnresolvedLayout { msg_code, len }
        }
    }
}

/// Routes one DECOMPRESSED frame to its decoder.
///
/// O(1) dispatch, zero allocation. Total — every input either routes or
/// yields a typed [`RouteRefusal`]; there is no panic and no silent drop.
///
/// Prefer [`FrameLedger::route`], which does this AND records the outcome.
///
/// # Errors
/// A [`RouteRefusal`] describing why the frame could not be decoded.
pub fn route_frame(raw: &[u8]) -> Result<TruedataFrame<'_>, RouteRefusal> {
    let len = raw.len();
    // The code byte, for error reporting. 0 only when the frame is empty,
    // which `classify_frame` rejects on the next line.
    let code = raw.first().copied().unwrap_or(0);
    let kind = classify_frame(raw).map_err(|e| refuse_core(e, code, len))?;
    match kind {
        TruedataFrameKind::Json => Ok(TruedataFrame::Json),
        TruedataFrameKind::AuthBinary => decode_auth_binary(raw)
            .map(TruedataFrame::Auth)
            .map_err(|e| refuse_core(e, code, len)),
        TruedataFrameKind::TradeBinary | TruedataFrameKind::TradeNoBidAskBinary => {
            decode_trade_binary(raw)
                .map(TruedataFrame::Trade)
                .map_err(|e| refuse_core(e, code, len))
        }
        TruedataFrameKind::BidAskBinary => decode_bidask_binary(raw)
            .map(TruedataFrame::BidAsk)
            .map_err(|e| refuse_aux(e, code, len)),
        TruedataFrameKind::GreeksBinary => decode_greeks_binary(raw)
            .map(TruedataFrame::Greeks)
            .map_err(|e| refuse_aux(e, code, len)),
        TruedataFrameKind::BarBinary => decode_bar_binary(raw)
            .map(TruedataFrame::Bar)
            .map_err(|e| refuse_aux(e, code, len)),
        TruedataFrameKind::SymbolAck | TruedataFrameKind::TouchlineBinary => {
            decode_record_list(raw)
                .map(TruedataFrame::RecordList)
                .map_err(|e| refuse_aux(e, code, len))
        }
        TruedataFrameKind::HeartbeatBinary => decode_heartbeat_binary(raw)
            .map(TruedataFrame::Heartbeat)
            .map_err(|e| refuse_core(e, code, len)),
        TruedataFrameKind::MarketStatusBinary => decode_market_status_binary(raw)
            .map(TruedataFrame::MarketStatus)
            .map_err(|e| refuse_core(e, code, len)),
        // Deliberately refused, not decoded — see the module docs.
        // Deliberately refused, not decoded — see the module docs. The
        // success arm maps to UnresolvedLayout rather than to `Empty`: it
        // is unreachable today, but the moment `D` IS resolved the natural
        // edit is to make the decoder return `Ok`, and an `Empty` mapping
        // would then file every depth frame under "zero-length frame"
        // while still discarding it. Conservation would hold and nothing
        // would alarm.
        TruedataFrameKind::BidAskL2Binary => Err(decode_bidask_l2_binary(raw).map_or_else(
            |e| refuse_aux(e, code, len),
            |()| RouteRefusal::UnresolvedLayout {
                msg_code: code,
                len,
            },
        )),
    }
}

// ---------------------------------------------------------------------------
// Accounting
// ---------------------------------------------------------------------------

/// Per-outcome frame counters — the conservation proof.
///
/// Every counter is a saturating `u64`, so a long session can never wrap a
/// count into a smaller number and fake conservation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameLedger {
    /// Frames handed to [`Self::route`].
    pub received: u64,
    /// Ticks decoded (codes `T` / `W`).
    pub trades: u64,
    /// Standalone L1 quotes (code `B`).
    pub bidasks: u64,
    /// Greeks frames (code `G`).
    pub greeks: u64,
    /// Vendor bars (codes `O` / `F`).
    pub bars: u64,
    /// Record-list frames (codes `S` / `L` / `R`).
    pub record_lists: u64,
    /// Auth responses (code `A`).
    pub auths: u64,
    /// Heartbeats (code `H`).
    pub heartbeats: u64,
    /// Market-status pushes (code `M`).
    pub market_statuses: u64,
    /// Frames handed off to the JSON decoder.
    pub json: u64,
    /// Empty frames.
    pub refused_empty: u64,
    /// Frames whose msg code is undocumented — the doc-drift signal.
    pub refused_unknown_code: u64,
    /// Frames whose shape did not match their code.
    pub refused_malformed: u64,
    /// Frames refused because their layout is unresolvable (`D`).
    pub refused_unresolved_layout: u64,
}

impl FrameLedger {
    /// A fresh ledger with every counter at zero.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            received: 0,
            trades: 0,
            bidasks: 0,
            greeks: 0,
            bars: 0,
            record_lists: 0,
            auths: 0,
            heartbeats: 0,
            market_statuses: 0,
            json: 0,
            refused_empty: 0,
            refused_unknown_code: 0,
            refused_malformed: 0,
            refused_unresolved_layout: 0,
        }
    }

    /// Routes one frame AND records its outcome.
    ///
    /// This is the entry point the driver should use: it is impossible to
    /// route through it without the frame being counted.
    ///
    /// # Errors
    /// A [`RouteRefusal`] — already counted before it is returned.
    pub fn route<'a>(&mut self, raw: &'a [u8]) -> Result<TruedataFrame<'a>, RouteRefusal> {
        self.received = self.received.saturating_add(1);
        let routed = route_frame(raw);
        match &routed {
            Ok(frame) => match frame {
                TruedataFrame::Trade(_) => self.trades = self.trades.saturating_add(1),
                TruedataFrame::BidAsk(_) => self.bidasks = self.bidasks.saturating_add(1),
                TruedataFrame::Greeks(_) => self.greeks = self.greeks.saturating_add(1),
                TruedataFrame::Bar(_) => self.bars = self.bars.saturating_add(1),
                TruedataFrame::RecordList(_) => {
                    self.record_lists = self.record_lists.saturating_add(1);
                }
                TruedataFrame::Auth(_) => self.auths = self.auths.saturating_add(1),
                TruedataFrame::Heartbeat(_) => self.heartbeats = self.heartbeats.saturating_add(1),
                TruedataFrame::MarketStatus(_) => {
                    self.market_statuses = self.market_statuses.saturating_add(1);
                }
                TruedataFrame::Json => self.json = self.json.saturating_add(1),
            },
            Err(refusal) => match refusal {
                RouteRefusal::Empty => self.refused_empty = self.refused_empty.saturating_add(1),
                RouteRefusal::UnknownMsgCode(_) => {
                    self.refused_unknown_code = self.refused_unknown_code.saturating_add(1);
                }
                RouteRefusal::Malformed { .. } => {
                    self.refused_malformed = self.refused_malformed.saturating_add(1);
                }
                RouteRefusal::UnresolvedLayout { .. } => {
                    self.refused_unresolved_layout =
                        self.refused_unresolved_layout.saturating_add(1);
                }
            },
        }
        routed
    }

    /// Total frames that reached a decoder.
    #[inline]
    #[must_use]
    pub const fn decoded(&self) -> u64 {
        self.trades
            .saturating_add(self.bidasks)
            .saturating_add(self.greeks)
            .saturating_add(self.bars)
            .saturating_add(self.record_lists)
            .saturating_add(self.auths)
            .saturating_add(self.heartbeats)
            .saturating_add(self.market_statuses)
            .saturating_add(self.json)
    }

    /// Total frames refused, for any reason.
    #[inline]
    #[must_use]
    pub const fn refused(&self) -> u64 {
        self.refused_empty
            .saturating_add(self.refused_unknown_code)
            .saturating_add(self.refused_malformed)
            .saturating_add(self.refused_unresolved_layout)
    }

    /// The conservation proof: `received == decoded + refused`.
    ///
    /// A `false` here means a frame entered [`Self::route`] and left without
    /// being classified — the exact silent-drop class this module exists to
    /// prevent. The caller should treat it as a coded error, never a warning.
    #[inline]
    #[must_use]
    pub const fn is_conserved(&self) -> bool {
        self.received == self.decoded().saturating_add(self.refused())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::truedata::{
        TD_AUTH_BINARY_LEN, TD_HEARTBEAT_BINARY_LEN, TD_MARKET_STATUS_BINARY_LEN, TD_MSG_CODE_AUTH,
        TD_MSG_CODE_BAR_1MIN, TD_MSG_CODE_BIDASK, TD_MSG_CODE_BIDASK_L2, TD_MSG_CODE_GREEKS,
        TD_MSG_CODE_HEARTBEAT, TD_MSG_CODE_MARKET_STATUS, TD_MSG_CODE_SYMBOLS_ADDED,
        TD_MSG_CODE_SYMBOLS_REMOVED, TD_MSG_CODE_TOUCHLINE, TD_MSG_CODE_TRADE,
        TD_MSG_CODE_TRADE_NO_BIDASK, TD_TRADE_BINARY_LEN_FULL, TD_TRADE_BINARY_LEN_QUOTE_ONLY,
    };
    use super::super::truedata_aux::{
        TD_BAR_BINARY_LEN, TD_BIDASK_BINARY_LEN, TD_GREEKS_BINARY_LEN, TD_RECORD_LIST_HEADER_LEN,
    };
    use super::*;

    /// A well-formed frame of the given code, zero-filled beyond the code.
    fn frame(code: u8, len: usize) -> Vec<u8> {
        let mut f = vec![0_u8; len];
        f[0] = code;
        f
    }

    /// Every documented code paired with a well-formed length for it.
    fn all_valid_frames() -> Vec<(u8, Vec<u8>)> {
        vec![
            (
                TD_MSG_CODE_AUTH,
                frame(TD_MSG_CODE_AUTH, TD_AUTH_BINARY_LEN),
            ),
            (
                TD_MSG_CODE_TRADE,
                frame(TD_MSG_CODE_TRADE, TD_TRADE_BINARY_LEN_FULL),
            ),
            (
                TD_MSG_CODE_TRADE_NO_BIDASK,
                frame(TD_MSG_CODE_TRADE_NO_BIDASK, TD_TRADE_BINARY_LEN_QUOTE_ONLY),
            ),
            (
                TD_MSG_CODE_BIDASK,
                frame(TD_MSG_CODE_BIDASK, TD_BIDASK_BINARY_LEN),
            ),
            (
                TD_MSG_CODE_GREEKS,
                frame(TD_MSG_CODE_GREEKS, TD_GREEKS_BINARY_LEN),
            ),
            (
                TD_MSG_CODE_BAR_1MIN,
                frame(TD_MSG_CODE_BAR_1MIN, TD_BAR_BINARY_LEN),
            ),
            (
                TD_MSG_CODE_SYMBOLS_ADDED,
                frame(TD_MSG_CODE_SYMBOLS_ADDED, TD_RECORD_LIST_HEADER_LEN),
            ),
            (
                TD_MSG_CODE_TOUCHLINE,
                frame(TD_MSG_CODE_TOUCHLINE, TD_RECORD_LIST_HEADER_LEN),
            ),
            (
                TD_MSG_CODE_SYMBOLS_REMOVED,
                frame(TD_MSG_CODE_SYMBOLS_REMOVED, TD_RECORD_LIST_HEADER_LEN),
            ),
            (
                TD_MSG_CODE_HEARTBEAT,
                frame(TD_MSG_CODE_HEARTBEAT, TD_HEARTBEAT_BINARY_LEN),
            ),
            (
                TD_MSG_CODE_MARKET_STATUS,
                frame(TD_MSG_CODE_MARKET_STATUS, TD_MARKET_STATUS_BINARY_LEN),
            ),
        ]
    }

    #[test]
    fn test_route_frame_decodes_every_documented_code() {
        for (code, f) in all_valid_frames() {
            let got = route_frame(&f);
            assert!(
                got.is_ok(),
                "code {:?} must route, got {got:?}",
                char::from(code)
            );
        }
    }

    #[test]
    fn test_route_frame_sends_each_code_to_the_right_decoder() {
        let f = frame(TD_MSG_CODE_TRADE, TD_TRADE_BINARY_LEN_FULL);
        assert!(matches!(route_frame(&f), Ok(TruedataFrame::Trade(_))));
        let f = frame(TD_MSG_CODE_TRADE_NO_BIDASK, TD_TRADE_BINARY_LEN_QUOTE_ONLY);
        assert!(matches!(route_frame(&f), Ok(TruedataFrame::Trade(_))));
        let f = frame(TD_MSG_CODE_BIDASK, TD_BIDASK_BINARY_LEN);
        assert!(matches!(route_frame(&f), Ok(TruedataFrame::BidAsk(_))));
        let f = frame(TD_MSG_CODE_GREEKS, TD_GREEKS_BINARY_LEN);
        assert!(matches!(route_frame(&f), Ok(TruedataFrame::Greeks(_))));
        let f = frame(TD_MSG_CODE_BAR_1MIN, TD_BAR_BINARY_LEN);
        assert!(matches!(route_frame(&f), Ok(TruedataFrame::Bar(_))));
        let f = frame(TD_MSG_CODE_SYMBOLS_ADDED, TD_RECORD_LIST_HEADER_LEN);
        assert!(matches!(route_frame(&f), Ok(TruedataFrame::RecordList(_))));
        let f = frame(TD_MSG_CODE_HEARTBEAT, TD_HEARTBEAT_BINARY_LEN);
        assert!(matches!(route_frame(&f), Ok(TruedataFrame::Heartbeat(_))));
        let f = frame(TD_MSG_CODE_MARKET_STATUS, TD_MARKET_STATUS_BINARY_LEN);
        assert!(matches!(
            route_frame(&f),
            Ok(TruedataFrame::MarketStatus(_))
        ));
        let f = frame(TD_MSG_CODE_AUTH, TD_AUTH_BINARY_LEN);
        assert!(matches!(route_frame(&f), Ok(TruedataFrame::Auth(_))));
    }

    #[test]
    fn test_route_frame_routes_json_as_success_not_refusal() {
        // JSON is a first-class transport mode, not an error.
        let json = br#"{"trade":["100000995"]}"#;
        assert_eq!(route_frame(json), Ok(TruedataFrame::Json));
    }

    #[test]
    fn test_route_frame_refuses_bidask_l2_as_unresolved_layout() {
        let f = frame(TD_MSG_CODE_BIDASK_L2, 137);
        assert_eq!(
            route_frame(&f),
            Err(RouteRefusal::UnresolvedLayout {
                msg_code: TD_MSG_CODE_BIDASK_L2,
                len: 137
            })
        );
    }

    #[test]
    fn test_route_frame_refuses_empty_and_unknown_codes() {
        assert_eq!(route_frame(&[]), Err(RouteRefusal::Empty));
        assert_eq!(route_frame(b"Z"), Err(RouteRefusal::UnknownMsgCode(b'Z')));
    }

    #[test]
    fn test_route_frame_refuses_every_documented_code_at_a_wrong_length() {
        // A known code with a bad shape must be MALFORMED — never decoded
        // with garbage, and never mistaken for an unknown code.
        for (code, valid) in all_valid_frames() {
            let mut bad = valid.clone();
            bad.push(0); // one byte too long for every fixed-size shape
            let got = route_frame(&bad);
            let is_record_list = matches!(
                code,
                TD_MSG_CODE_SYMBOLS_ADDED | TD_MSG_CODE_TOUCHLINE | TD_MSG_CODE_SYMBOLS_REMOVED
            );
            if is_record_list {
                // Record lists are variable length: one extra byte makes the
                // body ragged, which is also a malformed verdict.
                assert!(
                    matches!(got, Err(RouteRefusal::Malformed { .. })),
                    "ragged record list must be malformed, got {got:?}"
                );
            } else {
                assert!(
                    matches!(got, Err(RouteRefusal::Malformed { .. })),
                    "code {:?} at a wrong length must be malformed, got {got:?}",
                    char::from(code)
                );
            }
        }
    }

    #[test]
    fn test_route_frame_never_panics_on_arbitrary_bytes() {
        // Total-function proof over every possible first byte at several
        // lengths, including lengths that are valid for OTHER codes.
        for code in 0_u8..=255 {
            for len in [0_usize, 1, 10, 25, 37, 57, 74, 90, 114, 128, 137, 500] {
                let mut f = vec![0_u8; len];
                if len > 0 {
                    f[0] = code;
                }
                // Must return, never panic. Verdict is irrelevant here.
                let _ = route_frame(&f);
            }
        }
    }

    // --- ledger ---

    #[test]
    fn test_frame_ledger_new_starts_at_zero_and_is_conserved() {
        let ledger = FrameLedger::new();
        assert_eq!(ledger.received, 0);
        assert_eq!(ledger.decoded(), 0);
        assert_eq!(ledger.refused(), 0);
        assert!(ledger.is_conserved());
    }

    #[test]
    fn test_frame_ledger_route_conserves_across_a_mixed_stream() {
        let mut ledger = FrameLedger::new();
        // Every valid code, plus one of each refusal class.
        for (_, f) in all_valid_frames() {
            let _ = ledger.route(&f);
        }
        let _ = ledger.route(br#"{"trade":[]}"#);
        let _ = ledger.route(&[]);
        let _ = ledger.route(b"Z");
        let _ = ledger.route(&frame(TD_MSG_CODE_TRADE, 3));
        let _ = ledger.route(&frame(TD_MSG_CODE_BIDASK_L2, 141));

        assert_eq!(ledger.received, 16);
        assert_eq!(ledger.trades, 2, "T and W both count as trades");
        assert_eq!(ledger.record_lists, 3, "S, L and R");
        assert_eq!(ledger.json, 1);
        assert_eq!(ledger.refused_empty, 1);
        assert_eq!(ledger.refused_unknown_code, 1);
        assert_eq!(ledger.refused_malformed, 1);
        assert_eq!(ledger.refused_unresolved_layout, 1);
        assert_eq!(ledger.refused(), 4);
        assert!(
            ledger.is_conserved(),
            "received {} != decoded {} + refused {}",
            ledger.received,
            ledger.decoded(),
            ledger.refused()
        );
    }

    #[test]
    fn test_frame_ledger_is_conserved_over_every_byte_value() {
        // The strongest form of the guarantee: whatever arrives, the ledger
        // balances. Nothing can enter route() and vanish.
        let mut ledger = FrameLedger::new();
        let mut expected = 0_u64;
        for code in 0_u8..=255 {
            for len in [0_usize, 1, 25, 74, 90, 137] {
                let mut f = vec![0_u8; len];
                if len > 0 {
                    f[0] = code;
                }
                let _ = ledger.route(&f);
                expected = expected.saturating_add(1);
            }
        }
        assert_eq!(ledger.received, expected);
        assert!(ledger.is_conserved());
    }

    #[test]
    fn test_frame_ledger_decoded_and_refused_partition_the_stream() {
        let mut ledger = FrameLedger::new();
        let f = frame(TD_MSG_CODE_TRADE, TD_TRADE_BINARY_LEN_FULL);
        assert!(ledger.route(&f).is_ok());
        assert_eq!(ledger.decoded(), 1);
        assert_eq!(ledger.refused(), 0);
        assert!(ledger.route(b"Z").is_err());
        assert_eq!(ledger.decoded(), 1);
        assert_eq!(ledger.refused(), 1);
        assert!(ledger.is_conserved());
    }

    // --- refusal metadata ---

    #[test]
    fn test_route_refusal_msg_code_reports_the_offending_byte() {
        assert_eq!(RouteRefusal::Empty.msg_code(), None);
        assert_eq!(RouteRefusal::UnknownMsgCode(b'Z').msg_code(), Some(b'Z'));
        assert_eq!(
            RouteRefusal::Malformed {
                msg_code: b'T',
                len: 3
            }
            .msg_code(),
            Some(b'T')
        );
        assert_eq!(
            RouteRefusal::UnresolvedLayout {
                msg_code: b'D',
                len: 137
            }
            .msg_code(),
            Some(b'D')
        );
    }

    #[test]
    fn test_route_refusal_reason_label_is_stable_and_distinct() {
        let labels = [
            RouteRefusal::Empty.reason_label(),
            RouteRefusal::UnknownMsgCode(0).reason_label(),
            RouteRefusal::Malformed {
                msg_code: 0,
                len: 0,
            }
            .reason_label(),
            RouteRefusal::UnresolvedLayout {
                msg_code: 0,
                len: 0,
            }
            .reason_label(),
        ];
        assert_eq!(
            labels,
            ["empty", "unknown_code", "malformed", "unresolved_layout"]
        );
        // Labels become counter dimensions, so they must stay distinct.
        for (i, a) in labels.iter().enumerate() {
            for b in labels.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }
}
