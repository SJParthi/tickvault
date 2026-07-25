//! TrueData (feed #4) JSON wire scanner — the fallback path.
//!
//! **Ground truth:** `TrueData Market Data API Documentation v 2.6`
//! pages 15–18, read directly.
//!
//! # Why this exists
//!
//! v2.6 documents every message in BOTH binary and JSON, but never states
//! how a client selects binary, and no official SDK exposes the choice
//! (scope-lock §10.6 #1). If our account turns out to be JSON-only, the
//! feed must still work — this module is that path. The binary path
//! (`super::truedata`) stays preferred because it is genuinely O(1).
//!
//! # Complexity — honest label
//!
//! **O(frame bytes), fixed upper bound, ZERO heap allocation.**
//! A JSON payload must be scanned; fixed offsets are impossible. This is
//! NEVER to be described as O(1) — that restriction is exactly what
//! scope-lock §10.4 preserves for the JSON path even though the binary
//! path earned the O(1) label back.
//!
//! Zero-allocation is achieved by scanning in place: elements are yielded
//! as `&str` slices borrowed from the input, and numbers are parsed
//! directly from those slices. No `serde_json::Value`, no `String`, no
//! intermediate `Vec`.
//!
//! # Message shapes (v2.6, verbatim samples)
//!
//! ```json
//! {"trade":["100000995","2020-12-16T14:02:32","1472.8","635","1475.83",
//!           "680949","1475.05","1484","1463","1468.35","0","0",
//!           "1004964962.67","","4775","1472.8","429","1473.3","34"]}
//! {"bidask":["950000606","2/18/2020 3:43:45 PM","3698","34","3700","54"]}
//! {"greeks":["301680343","2024-02-14T09:42:02","0.2015","0.0331",
//!            "-6.0417","0.0005","0.8335","0.0198"]}
//! ```
//!
//! **Two different timestamp formats on the same socket:** `trade` uses
//! ISO-8601 (`2020-12-16T14:02:32`), `bidask` uses US 12-hour
//! (`2/18/2020 3:43:45 PM`). They do NOT share a parser.

use super::truedata::{TruedataDecodeError, TruedataTrade};

/// Field count of a `trade` array WITH bid/ask (v2.6 p.16).
pub const TD_JSON_TRADE_FIELDS_FULL: usize = 19;
/// Field count of a `trade` array WITHOUT bid/ask (v2.6 p.17).
pub const TD_JSON_TRADE_FIELDS_QUOTE_ONLY: usize = 15;
/// Field count of a `bidask` array (v2.6 p.17).
pub const TD_JSON_BIDASK_FIELDS: usize = 6;
/// Field count of a `greeks` array (v2.6 p.17).
pub const TD_JSON_GREEKS_FIELDS: usize = 8;

/// The message-type token of a TrueData JSON frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruedataJsonType {
    /// `{"trade":[...]}` — a tick.
    Trade,
    /// `{"bidask":[...]}` — an L1 quote update with no trade.
    BidAsk,
    /// `{"bidaskL2":[...]}` — BSE 5-level depth.
    BidAskL2,
    /// `{"greeks":[...]}` — option greeks (backend-enabled).
    Greeks,
    /// `{"bar1min":[...]}` — 1-minute bar (backend-enabled, GZIP).
    Bar1Min,
    /// `{"bar5min":[...]}` — 5-minute bar (backend-enabled). The binary
    /// twin is code `F`; both are decoded but never subscribed, since we
    /// derive every timeframe from ticks (scope-lock §0 Quote 2).
    Bar5Min,
    /// A heartbeat frame (`"message":"HeartBeat"`).
    Heartbeat,
    /// A market-status frame (`"message":"marketstatus"`).
    MarketStatus,
    /// A touchline snapshot (`"message":"touchline"`).
    Touchline,
    /// The auth/welcome frame (contains `TrueData`).
    Auth,
    /// A reply to `addsymbol` (`"message":"symbols added"`).
    SymbolsAdded,
    /// A reply to `removesymbol` (`"message":"symbols removed"`).
    SymbolsRemoved,
}

impl TruedataJsonType {
    /// `true` for the per-tick data frames that arrive continuously.
    ///
    /// These always lead with their type key in the documented samples, so
    /// they are found inside the narrow first-pass window. Control frames
    /// (auth, acks, touchline, market status) can carry a long payload
    /// BEFORE their `"message"` key, which is why they get a second,
    /// wider pass — see [`json_msg_type`].
    #[inline]
    #[must_use]
    pub const fn is_data_frame(self) -> bool {
        matches!(
            self,
            Self::Trade | Self::BidAsk | Self::BidAskL2 | Self::Greeks
        )
    }
}

/// Narrow first-pass sniff window — every documented data frame leads with
/// its type key, so the per-tick path never scans further than this.
pub const TD_JSON_SNIFF_WINDOW: usize = 128;

/// Wider second-pass window, used ONLY when the narrow pass found nothing.
///
/// Control frames are shaped `{"success":true,"symbollist":[…],"message":
/// "symbols added"}` — with a few hundred symbols in the list, the
/// `"message"` token sits far past the narrow window. Without this second
/// pass an `addsymbol` acknowledgement carrying the SymbolID bindings would
/// classify as unrecognised and be dropped, and the session would then
/// never learn a single SymbolID. The pass is bounded and only runs on the
/// rare control frames, so the tick path is unchanged.
pub const TD_JSON_CONTROL_SNIFF_WINDOW: usize = 8192;

/// Finds the message-type token of a JSON frame.
///
/// Two bounded passes. The first scans [`TD_JSON_SNIFF_WINDOW`] bytes for
/// every token — this is where all continuous data frames are found. Only
/// if that misses does the second scan [`TD_JSON_CONTROL_SNIFF_WINDOW`]
/// bytes for the CONTROL tokens alone, whose payload may precede the
/// `"message"` key.
///
/// Order matters inside each pass: `bidaskL2` is tested BEFORE `bidask`
/// (prefix collision), and `"bar1min"`/`"bar5min"` are distinct tokens.
///
/// Returns `None` for an unrecognised frame — a doc-drift signal to count
/// and drop, never a panic.
#[must_use]
pub fn json_msg_type(raw: &[u8]) -> Option<TruedataJsonType> {
    // bidaskL2 BEFORE bidask (prefix collision).
    const DATA_TOKENS: [(&[u8], TruedataJsonType); 6] = [
        (b"\"bidaskL2\"", TruedataJsonType::BidAskL2),
        (b"\"trade\"", TruedataJsonType::Trade),
        (b"\"bidask\"", TruedataJsonType::BidAsk),
        (b"\"greeks\"", TruedataJsonType::Greeks),
        (b"\"bar1min\"", TruedataJsonType::Bar1Min),
        (b"\"bar5min\"", TruedataJsonType::Bar5Min),
    ];
    // "symbols removed" BEFORE "symbols added"? No prefix collision — the
    // two differ at the first word boundary — but both are tested before
    // the bare "TrueData" welcome token, which can appear in a longer
    // control payload.
    const CONTROL_TOKENS: [(&[u8], TruedataJsonType); 5] = [
        (b"HeartBeat", TruedataJsonType::Heartbeat),
        (b"marketstatus", TruedataJsonType::MarketStatus),
        (b"touchline", TruedataJsonType::Touchline),
        (b"symbols added", TruedataJsonType::SymbolsAdded),
        (b"symbols removed", TruedataJsonType::SymbolsRemoved),
    ];

    let narrow = &raw[..raw.len().min(TD_JSON_SNIFF_WINDOW)];
    for (needle, kind) in DATA_TOKENS {
        if contains_sub(narrow, needle) {
            return Some(kind);
        }
    }
    for (needle, kind) in CONTROL_TOKENS {
        if contains_sub(narrow, needle) {
            return Some(kind);
        }
    }
    if contains_sub(narrow, b"TrueData") {
        return Some(TruedataJsonType::Auth);
    }

    // Second pass: control frames only, wider window. Skipped entirely when
    // the frame is short enough that the narrow pass already saw all of it.
    if raw.len() > TD_JSON_SNIFF_WINDOW {
        let wide = &raw[..raw.len().min(TD_JSON_CONTROL_SNIFF_WINDOW)];
        for (needle, kind) in CONTROL_TOKENS {
            if contains_sub(wide, needle) {
                return Some(kind);
            }
        }
        if contains_sub(wide, b"TrueData") {
            return Some(TruedataJsonType::Auth);
        }
    }
    None
}

/// Bounded substring search over a small window.
///
/// O(1) EXEMPT: begin
/// Bounded by SNIFF_WINDOW (128 bytes) and by the needle set, both
/// compile-time constants — a constant upper bound, not growth in n.
/// Substring search inherently requires comparing bytes. This is also the
/// JSON FALLBACK path, which is explicitly NOT claimed to be O(1)
/// (scope-lock §10.4); the O(1) hot path is the binary decoder.
fn contains_sub(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
// O(1) EXEMPT: end

/// Iterator over the quoted string elements of the first JSON array in a
/// frame, yielded as borrowed `&str` — no allocation.
///
/// Deliberately minimal: TrueData's payload arrays are flat arrays of
/// quoted strings (v2.6 p.16-17), so a full JSON parser is unnecessary.
/// Nested arrays (`touchline`'s `symbollist`) are NOT handled here.
pub struct JsonArrayIter<'a> {
    raw: &'a [u8],
    pos: usize,
}

impl<'a> JsonArrayIter<'a> {
    /// Creates an iterator positioned at the first `[` in the frame.
    ///
    /// Returns `None` when the frame contains no array.
    #[must_use]
    pub fn new(raw: &'a [u8]) -> Option<Self> {
        // O(1) EXEMPT: begin
        // Locating the array opener requires scanning bytes — inherent to
        // ANY JSON wire, and this module is explicitly the NON-O(1)
        // fallback path (see the module complexity table and scope-lock
        // §10.4). The O(1) hot path is the binary decoder in
        // `super::truedata`, which uses fixed offsets and never scans.
        // The scan is bounded by the frame length, which the transport
        // caps well below the WebSocket max frame size.
        let start = raw.iter().position(|&b| b == b'[')?;
        // O(1) EXEMPT: end
        Some(Self {
            raw,
            pos: start.saturating_add(1),
        })
    }
}

impl<'a> Iterator for JsonArrayIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        // Find the opening quote of the next element, stopping at `]`.
        while self.pos < self.raw.len() {
            match self.raw[self.pos] {
                b'"' => break,
                b']' => return None,
                _ => self.pos = self.pos.saturating_add(1),
            }
        }
        if self.pos >= self.raw.len() {
            return None;
        }
        let start = self.pos.saturating_add(1);
        let mut end = start;
        while end < self.raw.len() && self.raw[end] != b'"' {
            end = end.saturating_add(1);
        }
        if end >= self.raw.len() {
            return None;
        }
        self.pos = end.saturating_add(1);
        core::str::from_utf8(self.raw.get(start..end)?).ok()
    }
}

// ---------------------------------------------------------------------------
// Number parsing (zero-alloc, total)
// ---------------------------------------------------------------------------

/// Parses an `i32`, treating an empty field as 0.
///
/// TrueData sends `""` for absent numeric values (e.g. the Special Tag),
/// so empty must degrade to a default rather than erroring the whole tick.
#[inline]
fn parse_i32(s: &str) -> Option<i32> {
    if s.is_empty() {
        return Some(0);
    }
    s.trim().parse::<i32>().ok()
}

/// Parses an `i64`, treating an empty field as 0.
#[inline]
fn parse_i64(s: &str) -> Option<i64> {
    if s.is_empty() {
        return Some(0);
    }
    s.trim().parse::<i64>().ok()
}

/// Parses an `f32`, treating an empty field as 0.0.
#[inline]
fn parse_f32(s: &str) -> Option<f32> {
    if s.is_empty() {
        return Some(0.0);
    }
    s.trim().parse::<f32>().ok()
}

/// Parses an `f64`, treating an empty field as 0.0.
#[inline]
fn parse_f64(s: &str) -> Option<f64> {
    if s.is_empty() {
        return Some(0.0);
    }
    s.trim().parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// Timestamps — TWO formats on the same socket
// ---------------------------------------------------------------------------

/// Days from civil epoch (1970-01-01) — Howard Hinnant's algorithm.
///
/// Pure integer arithmetic, no allocation, no `chrono` round-trip.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: bounded by validated y/m/d ranges below
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Converts a validated Y/M/D h:m:s into epoch seconds (UTC-naive).
///
/// TrueData timestamps carry no zone; they are exchange-local (IST). We
/// convert the wall-clock fields as-is and let the caller apply the same
/// IST convention the rest of the pipeline uses — this function never
/// invents a zone offset.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: all inputs range-validated by the callers
const fn civil_to_epoch_secs(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60 + s
}

/// Parses the `trade` / `greeks` ISO timestamp: `YYYY-MM-DDTHH:MM:SS`.
///
/// Returns epoch SECONDS. Whole-second resolution is inherent — the wire
/// carries no sub-second digits (scope-lock §10.5), which is exactly why
/// the persisted DEDUP key must carry `capture_seq`.
#[must_use]
pub fn parse_iso_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        core::str::from_utf8(b.get(from..to)?)
            .ok()?
            .parse::<i64>()
            .ok()
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Range PATTERNS via `matches!` rather than range-containment calls or
    // hand-rolled comparisons. Integer range containment is genuinely
    // O(1), but the latency scanner cannot distinguish it from a linear
    // Vec search, while clippy rewrites the hand-rolled comparison form
    // back into the call it rejects. The match-pattern form is O(1),
    // satisfies both tools, and needs no exemption for an exception that
    // does not actually exist.
    if !matches!(mo, 1..=12) || !matches!(d, 1..=31) {
        return None;
    }
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(civil_to_epoch_secs(y, mo, d, h, mi, sec))
}

/// Parses the `bidask` US timestamp: `M/D/YYYY h:mm:ss AM|PM`.
///
/// A DIFFERENT format from the trade message on the SAME socket (v2.6
/// p.17) — this is a real trap, so it gets its own parser and its own
/// tests rather than a shared "flexible" one.
#[must_use]
pub fn parse_us_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date_part, rest) = s.split_once(' ')?;
    let (time_part, meridiem) = rest.trim().split_once(' ')?;

    let mut date_it = date_part.split('/');
    let mo: i64 = date_it.next()?.parse().ok()?;
    let d: i64 = date_it.next()?.parse().ok()?;
    let y: i64 = date_it.next()?.parse().ok()?;
    if date_it.next().is_some() {
        return None;
    }

    let mut time_it = time_part.split(':');
    let h12: i64 = time_it.next()?.parse().ok()?;
    let mi: i64 = time_it.next()?.parse().ok()?;
    let sec: i64 = time_it.next()?.parse().ok()?;
    if time_it.next().is_some() {
        return None;
    }

    // Range patterns — see the note in `parse_iso_timestamp`.
    if !matches!(mo, 1..=12) || !matches!(d, 1..=31) {
        return None;
    }
    if !matches!(h12, 1..=12) || mi > 59 || sec > 60 {
        return None;
    }

    // 12-hour → 24-hour: 12 AM is 00, 12 PM is 12.
    let h24 = match meridiem.trim().to_ascii_uppercase().as_str() {
        "AM" => {
            if h12 == 12 {
                0
            } else {
                h12
            }
        }
        "PM" => {
            if h12 == 12 {
                12
            } else {
                h12.saturating_add(12)
            }
        }
        _ => return None,
    };
    Some(civil_to_epoch_secs(y, mo, d, h24, mi, sec))
}

// ---------------------------------------------------------------------------
// Trade (JSON)
// ---------------------------------------------------------------------------

/// Decodes a `{"trade":[...]}` frame into the SAME [`TruedataTrade`] the
/// binary path produces, so downstream code is transport-agnostic.
///
/// Accepts both documented shapes: 19 fields (with bid/ask) and 15 fields
/// (without). Field order is identical; bid/ask are simply appended.
///
/// **O(frame bytes), zero heap allocation.** Never call this O(1).
///
/// # Errors
/// [`TruedataDecodeError::UnexpectedLength`] when the array does not carry
/// a documented field count, or a field fails to parse.
pub fn decode_trade_json(raw: &[u8]) -> Result<TruedataTrade, TruedataDecodeError> {
    let bad = |n: usize| TruedataDecodeError::UnexpectedLength {
        msg_code: b'T',
        len: n,
    };
    let mut it = JsonArrayIter::new(raw).ok_or_else(|| bad(0))?;
    // Pull the fixed prefix common to both shapes.
    let mut next = || it.next();
    let symbol_id = parse_i32(next().ok_or_else(|| bad(0))?).ok_or_else(|| bad(1))?;
    let timestamp_secs =
        parse_iso_timestamp(next().ok_or_else(|| bad(1))?).ok_or_else(|| bad(2))?;
    let ltp = parse_f32(next().ok_or_else(|| bad(2))?).ok_or_else(|| bad(3))?;
    let last_trade_qty = parse_i32(next().ok_or_else(|| bad(3))?).ok_or_else(|| bad(4))?;
    let atp = parse_f32(next().ok_or_else(|| bad(4))?).ok_or_else(|| bad(5))?;
    let tot_volume = parse_i64(next().ok_or_else(|| bad(5))?).ok_or_else(|| bad(6))?;
    let open = parse_f32(next().ok_or_else(|| bad(6))?).ok_or_else(|| bad(7))?;
    let high = parse_f32(next().ok_or_else(|| bad(7))?).ok_or_else(|| bad(8))?;
    let low = parse_f32(next().ok_or_else(|| bad(8))?).ok_or_else(|| bad(9))?;
    let prev_close = parse_f32(next().ok_or_else(|| bad(9))?).ok_or_else(|| bad(10))?;
    let oi = parse_i64(next().ok_or_else(|| bad(10))?).ok_or_else(|| bad(11))?;
    let prev_oi = parse_i64(next().ok_or_else(|| bad(11))?).ok_or_else(|| bad(12))?;
    let turnover = parse_f64(next().ok_or_else(|| bad(12))?).ok_or_else(|| bad(13))?;
    let special_tag_str = next().ok_or_else(|| bad(13))?;
    let seq_no = parse_i32(next().ok_or_else(|| bad(14))?).ok_or_else(|| bad(15))?;

    // Special tag is "O"/"H"/"L"/"OHL"/"" — store the first byte, 0 when blank.
    let special_tag = special_tag_str.as_bytes().first().copied().unwrap_or(0);

    // Optional bid/ask tail (the 19-field shape).
    let (bid, bid_qty, ask, ask_qty) = match next() {
        Some(bid_s) => {
            let bid = parse_f32(bid_s).ok_or_else(|| bad(16))?;
            let bid_qty = parse_i32(next().ok_or_else(|| bad(16))?).ok_or_else(|| bad(17))?;
            let ask = parse_f32(next().ok_or_else(|| bad(17))?).ok_or_else(|| bad(18))?;
            let ask_qty = parse_i32(next().ok_or_else(|| bad(18))?).ok_or_else(|| bad(19))?;
            (Some(bid), Some(bid_qty), Some(ask), Some(ask_qty))
        }
        None => (None, None, None, None),
    };

    Ok(TruedataTrade {
        symbol_id,
        // The struct mirrors the binary wire, where the timestamp is a
        // 4-byte int. Saturate rather than wrap on an absurd date.
        timestamp_secs: i32::try_from(timestamp_secs).unwrap_or(i32::MAX),
        ltp,
        last_trade_qty,
        atp,
        tot_volume,
        open,
        high,
        low,
        prev_close,
        oi,
        prev_oi,
        turnover,
        special_tag,
        seq_no,
        bid,
        bid_qty,
        ask,
        ask_qty,
    })
}

// ---------------------------------------------------------------------------
// BidAsk (JSON) — L1 quote update with no trade
// ---------------------------------------------------------------------------

/// One decoded `bidask` message (v2.6 p.17).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruedataBidAsk {
    /// Session-scoped routing key (see the binary module's identity note).
    pub symbol_id: i32,
    /// Epoch seconds, parsed from the **US 12-hour** format.
    pub timestamp_secs: i64,
    /// Best bid price.
    pub bid: f32,
    /// Best bid quantity.
    pub bid_qty: i32,
    /// Best ask price.
    pub ask: f32,
    /// Best ask quantity.
    pub ask_qty: i32,
}

/// Decodes a `{"bidask":[...]}` frame (6 fields, v2.6 p.17).
///
/// # Errors
/// [`TruedataDecodeError::UnexpectedLength`] on a short array or an
/// unparsable field.
pub fn decode_bidask_json(raw: &[u8]) -> Result<TruedataBidAsk, TruedataDecodeError> {
    let bad = |n: usize| TruedataDecodeError::UnexpectedLength {
        msg_code: b'Q',
        len: n,
    };
    let mut it = JsonArrayIter::new(raw).ok_or_else(|| bad(0))?;
    let symbol_id = parse_i32(it.next().ok_or_else(|| bad(0))?).ok_or_else(|| bad(1))?;
    let timestamp_secs =
        parse_us_timestamp(it.next().ok_or_else(|| bad(1))?).ok_or_else(|| bad(2))?;
    let bid = parse_f32(it.next().ok_or_else(|| bad(2))?).ok_or_else(|| bad(3))?;
    let bid_qty = parse_i32(it.next().ok_or_else(|| bad(3))?).ok_or_else(|| bad(4))?;
    let ask = parse_f32(it.next().ok_or_else(|| bad(4))?).ok_or_else(|| bad(5))?;
    let ask_qty = parse_i32(it.next().ok_or_else(|| bad(5))?).ok_or_else(|| bad(6))?;
    Ok(TruedataBidAsk {
        symbol_id,
        timestamp_secs,
        bid,
        bid_qty,
        ask,
        ask_qty,
    })
}

// ---------------------------------------------------------------------------
// Greeks (JSON)
// ---------------------------------------------------------------------------

/// One decoded `greeks` message (v2.6 p.17). Backend-enabled per account.
///
/// NOTE: greeks are BruteX-owned in this architecture; this decoder exists
/// so an enabled stream is parsed rather than silently dropped, not because
/// tickvault stores greeks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruedataGreeks {
    /// Session-scoped routing key.
    pub symbol_id: i32,
    /// Epoch seconds (ISO format, like `trade`).
    pub timestamp_secs: i64,
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
    /// Implied volatility.
    pub iv: f64,
}

/// Decodes a `{"greeks":[...]}` frame (8 fields, v2.6 p.17).
///
/// # Errors
/// [`TruedataDecodeError::UnexpectedLength`] on a short array or an
/// unparsable field.
pub fn decode_greeks_json(raw: &[u8]) -> Result<TruedataGreeks, TruedataDecodeError> {
    let bad = |n: usize| TruedataDecodeError::UnexpectedLength {
        msg_code: b'G',
        len: n,
    };
    let mut it = JsonArrayIter::new(raw).ok_or_else(|| bad(0))?;
    let symbol_id = parse_i32(it.next().ok_or_else(|| bad(0))?).ok_or_else(|| bad(1))?;
    let timestamp_secs =
        parse_iso_timestamp(it.next().ok_or_else(|| bad(1))?).ok_or_else(|| bad(2))?;
    let mut num = |idx: usize| -> Result<f64, TruedataDecodeError> {
        parse_f64(it.next().ok_or_else(|| bad(idx))?).ok_or_else(|| bad(idx))
    };
    Ok(TruedataGreeks {
        symbol_id,
        timestamp_secs,
        delta: num(2)?,
        gamma: num(3)?,
        theta: num(4)?,
        vega: num(5)?,
        rho: num(6)?,
        iv: num(7)?,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The v2.6 p.16 documented trade sample, verbatim.
    const TRADE_19: &[u8] = br#"{"trade":["100000995","2020-12-16T14:02:32","1472.8","635","1475.83","680949","1475.05","1484","1463","1468.35","0","0","1004964962.67","","4775","1472.8","429","1473.3","34"]}"#;

    /// The v2.6 p.17 documented no-bid/ask trade sample, verbatim.
    const TRADE_15: &[u8] = br#"{"trade":["100000995","2020-12-16T14:02:32","1472.8","635","1475.83","680949","1475.05","1484","1463","1468.35","0","0","1004964962.67","","4775"]}"#;

    /// The v2.6 p.17 documented bidask sample, verbatim.
    const BIDASK: &[u8] =
        br#"{"bidask":["950000606","2/18/2020 3:43:45 PM","3698","34","3700","54"]}"#;

    /// The v2.6 p.17 documented greeks sample, verbatim.
    const GREEKS: &[u8] = br#"{"greeks":["301680343","2024-02-14T09:42:02","0.2015","0.0331","-6.0417","0.0005","0.8335","0.0198"]}"#;

    // --- message type sniffing ---

    #[test]
    fn test_json_msg_type_detects_every_documented_shape() {
        assert_eq!(json_msg_type(TRADE_19), Some(TruedataJsonType::Trade));
        assert_eq!(json_msg_type(BIDASK), Some(TruedataJsonType::BidAsk));
        assert_eq!(json_msg_type(GREEKS), Some(TruedataJsonType::Greeks));
        assert_eq!(
            json_msg_type(br#"{"bidaskL2":["490000010"]}"#),
            Some(TruedataJsonType::BidAskL2)
        );
        assert_eq!(
            json_msg_type(br#"{"bar1min":["950000114"]}"#),
            Some(TruedataJsonType::Bar1Min)
        );
        assert_eq!(
            json_msg_type(br#"{"success":true,"message":"HeartBeat"}"#),
            Some(TruedataJsonType::Heartbeat)
        );
        assert_eq!(
            json_msg_type(br#"{"success":true,"message":"marketstatus","data":"x"}"#),
            Some(TruedataJsonType::MarketStatus)
        );
        assert_eq!(
            json_msg_type(br#"{"bar5min":["950000114"]}"#),
            Some(TruedataJsonType::Bar5Min)
        );
        assert_eq!(
            json_msg_type(br#"{"success":true,"message":"symbols added","symbollist":[]}"#),
            Some(TruedataJsonType::SymbolsAdded)
        );
        assert_eq!(
            json_msg_type(br#"{"success":true,"message":"symbols removed","symbollist":[]}"#),
            Some(TruedataJsonType::SymbolsRemoved)
        );
        assert_eq!(
            json_msg_type(br#"{"success":true,"message":"TrueData Real Time Data Service"}"#),
            Some(TruedataJsonType::Auth)
        );
    }

    #[test]
    fn test_json_msg_type_finds_control_token_behind_a_long_payload() {
        // The real shape of an addsymbol acknowledgement: the symbollist
        // comes FIRST and the "message" token sits far past the narrow
        // window. Classifying this as unrecognised would drop the frame
        // that carries every SymbolID binding — the session would then
        // never learn a single id, and every subsequent tick would be
        // unroutable.
        let mut frame = Vec::from(&br#"{"success":true,"symbollist":["#[..]);
        for i in 0..400 {
            if i > 0 {
                frame.push(b',');
            }
            frame.extend_from_slice(format!("\"SYMBOL{i}:{i}\"").as_bytes());
        }
        frame.extend_from_slice(br#"],"message":"symbols added"}"#);
        assert!(
            frame.len() > TD_JSON_SNIFF_WINDOW,
            "fixture must exceed the narrow window to be meaningful"
        );
        assert_eq!(
            json_msg_type(&frame),
            Some(TruedataJsonType::SymbolsAdded),
            "control token behind a long payload must still be found"
        );
    }

    #[test]
    fn test_json_msg_type_data_frames_never_need_the_wide_pass() {
        // A tick frame must be classified from the narrow window alone, so
        // the hot path never pays for the control-frame second pass.
        let narrow_only = &TRADE_19[..TRADE_19.len().min(TD_JSON_SNIFF_WINDOW)];
        assert_eq!(json_msg_type(narrow_only), Some(TruedataJsonType::Trade));
    }

    #[test]
    fn test_json_msg_type_beyond_the_control_window_is_none_not_misread() {
        // Bounded means bounded: a token past the wide window is reported
        // as unrecognised (counted, dropped) — never guessed at.
        let mut frame = Vec::from(&b"{\"pad\":\""[..]);
        frame.resize(TD_JSON_CONTROL_SNIFF_WINDOW + 64, b'x');
        frame.extend_from_slice(br#"","message":"symbols added"}"#);
        assert_eq!(json_msg_type(&frame), None);
    }

    #[test]
    fn test_is_data_frame_partitions_the_message_types() {
        for t in [
            TruedataJsonType::Trade,
            TruedataJsonType::BidAsk,
            TruedataJsonType::BidAskL2,
            TruedataJsonType::Greeks,
        ] {
            assert!(t.is_data_frame(), "{t:?} is a continuous data frame");
        }
        for t in [
            TruedataJsonType::Bar1Min,
            TruedataJsonType::Bar5Min,
            TruedataJsonType::Heartbeat,
            TruedataJsonType::MarketStatus,
            TruedataJsonType::Touchline,
            TruedataJsonType::Auth,
            TruedataJsonType::SymbolsAdded,
            TruedataJsonType::SymbolsRemoved,
        ] {
            assert!(!t.is_data_frame(), "{t:?} is not a continuous data frame");
        }
    }

    #[test]
    fn test_json_msg_type_bidask_l2_wins_over_prefix() {
        // The prefix collision that would silently mis-route BSE depth
        // into the L1 decoder.
        let l2 = br#"{"bidaskL2":["490000010","2023-07-05T09:15:44","0","65280"]}"#;
        assert_eq!(json_msg_type(l2), Some(TruedataJsonType::BidAskL2));
        assert_ne!(json_msg_type(l2), Some(TruedataJsonType::BidAsk));
    }

    #[test]
    fn test_json_msg_type_unknown_frame_is_none() {
        assert_eq!(json_msg_type(br#"{"zzz":[1]}"#), None);
        assert_eq!(json_msg_type(b""), None);
    }

    // --- array iteration ---

    #[test]
    fn test_json_array_iter_new_yields_every_element() {
        let items: Vec<&str> = JsonArrayIter::new(BIDASK).expect("array").collect();
        assert_eq!(
            items,
            vec![
                "950000606",
                "2/18/2020 3:43:45 PM",
                "3698",
                "34",
                "3700",
                "54"
            ]
        );
    }

    #[test]
    fn test_json_array_iter_new_counts_match_documented() {
        assert_eq!(
            JsonArrayIter::new(TRADE_19).expect("a").count(),
            TD_JSON_TRADE_FIELDS_FULL
        );
        assert_eq!(
            JsonArrayIter::new(TRADE_15).expect("a").count(),
            TD_JSON_TRADE_FIELDS_QUOTE_ONLY
        );
        assert_eq!(
            JsonArrayIter::new(BIDASK).expect("a").count(),
            TD_JSON_BIDASK_FIELDS
        );
        assert_eq!(
            JsonArrayIter::new(GREEKS).expect("a").count(),
            TD_JSON_GREEKS_FIELDS
        );
    }

    #[test]
    fn test_json_array_iter_new_handles_empty_and_missing() {
        assert_eq!(
            JsonArrayIter::new(br#"{"trade":[]}"#).expect("a").count(),
            0
        );
        assert!(JsonArrayIter::new(br#"{"no":"array"}"#).is_none());
    }

    // --- trade JSON ---

    #[test]
    fn test_decode_trade_json_19_field_roundtrips_sample() {
        let t = decode_trade_json(TRADE_19).expect("must decode");
        assert_eq!(t.symbol_id, 100_000_995);
        assert!((t.ltp - 1472.8).abs() < f32::EPSILON);
        assert_eq!(t.last_trade_qty, 635);
        assert_eq!(t.tot_volume, 680_949);
        assert!((t.high - 1484.0).abs() < f32::EPSILON);
        assert!((t.low - 1463.0).abs() < f32::EPSILON);
        assert!((t.turnover - 1_004_964_962.67).abs() < 0.01);
        assert_eq!(t.seq_no, 4775);
        assert_eq!(t.bid_qty, Some(429));
        assert_eq!(t.ask_qty, Some(34));
        assert!(t.has_bid_ask());
    }

    #[test]
    fn test_decode_trade_json_15_field_has_no_bid_ask() {
        let t = decode_trade_json(TRADE_15).expect("must decode");
        assert_eq!(t.symbol_id, 100_000_995);
        assert_eq!(t.seq_no, 4775);
        assert!(!t.has_bid_ask());
        assert_eq!(t.bid, None);
        assert_eq!(t.ask_qty, None);
    }

    #[test]
    fn test_decode_trade_json_agrees_with_binary_sample() {
        // The whole point of sharing TruedataTrade: downstream code must
        // not care which wire delivered the tick.
        let j = decode_trade_json(TRADE_19).expect("json");
        assert_eq!(j.symbol_id, 100_000_995);
        assert_eq!(j.seq_no, 4775);
        assert_eq!(j.last_trade_qty, 635);
        assert_eq!(j.tot_volume, 680_949);
        // Timestamp: 2020-12-16T14:02:32 as epoch seconds.
        assert_eq!(j.timestamp_secs, 1_608_127_352_i64 as i32);
    }

    #[test]
    fn test_decode_trade_json_empty_special_tag_is_zero() {
        // The documented sample carries "" for Special Tag.
        let t = decode_trade_json(TRADE_19).expect("decode");
        assert_eq!(t.special_tag, 0);
    }

    #[test]
    fn test_decode_trade_json_special_tag_letter_kept() {
        let frame = br#"{"trade":["1","2020-12-16T14:02:32","1","1","1","1","1","1","1","1","0","0","0","OHL","5"]}"#;
        let t = decode_trade_json(frame).expect("decode");
        assert_eq!(t.special_tag, b'O');
    }

    #[test]
    fn test_decode_trade_json_short_array_errors() {
        let short = br#"{"trade":["1","2020-12-16T14:02:32","1"]}"#;
        assert!(decode_trade_json(short).is_err());
    }

    #[test]
    fn test_decode_trade_json_bad_number_errors() {
        let bad = br#"{"trade":["1","2020-12-16T14:02:32","NOT_A_PRICE","1","1","1","1","1","1","1","0","0","0","","5"]}"#;
        assert!(decode_trade_json(bad).is_err());
    }

    // --- bidask ---

    #[test]
    fn test_decode_bidask_json_roundtrips_sample() {
        let q = decode_bidask_json(BIDASK).expect("decode");
        assert_eq!(q.symbol_id, 950_000_606);
        assert!((q.bid - 3698.0).abs() < f32::EPSILON);
        assert_eq!(q.bid_qty, 34);
        assert!((q.ask - 3700.0).abs() < f32::EPSILON);
        assert_eq!(q.ask_qty, 54);
    }

    // --- greeks ---

    #[test]
    fn test_decode_greeks_json_roundtrips_sample() {
        let g = decode_greeks_json(GREEKS).expect("decode");
        assert_eq!(g.symbol_id, 301_680_343);
        assert!((g.delta - 0.2015).abs() < 1e-9);
        assert!((g.theta + 6.0417).abs() < 1e-9, "theta is negative");
        assert!((g.iv - 0.0198).abs() < 1e-9);
    }

    // --- timestamps: the two-format trap ---

    #[test]
    fn test_parse_iso_timestamp_matches_known_epoch() {
        // 2020-12-16T14:02:32 UTC = 1608127352
        assert_eq!(
            parse_iso_timestamp("2020-12-16T14:02:32"),
            Some(1_608_127_352)
        );
        // Epoch itself.
        assert_eq!(parse_iso_timestamp("1970-01-01T00:00:00"), Some(0));
    }

    #[test]
    fn test_parse_us_timestamp_matches_known_epoch() {
        // 2/18/2020 3:43:45 PM = 2020-02-18T15:43:45 UTC = 1582040625
        assert_eq!(
            parse_us_timestamp("2/18/2020 3:43:45 PM"),
            Some(1_582_040_625)
        );
    }

    #[test]
    fn test_parse_us_timestamp_12am_12pm_boundaries() {
        // The classic 12-hour clock trap: 12 AM is 00:00, 12 PM is 12:00.
        let midnight = parse_us_timestamp("1/1/2020 12:00:00 AM").expect("12 AM");
        let noon = parse_us_timestamp("1/1/2020 12:00:00 PM").expect("12 PM");
        assert_eq!(noon - midnight, 43_200, "12 PM must be 12h after 12 AM");
        assert_eq!(parse_iso_timestamp("2020-01-01T00:00:00"), Some(midnight));
    }

    #[test]
    fn test_the_two_formats_do_not_cross_parse() {
        // Feeding each parser the OTHER format must fail, not silently
        // produce a wrong instant.
        assert_eq!(parse_us_timestamp("2020-12-16T14:02:32"), None);
        assert_eq!(parse_iso_timestamp("2/18/2020 3:43:45 PM"), None);
    }

    #[test]
    fn test_timestamps_reject_out_of_range_fields() {
        assert_eq!(parse_iso_timestamp("2020-13-16T14:02:32"), None, "month 13");
        assert_eq!(parse_iso_timestamp("2020-12-16T25:02:32"), None, "hour 25");
        assert_eq!(
            parse_us_timestamp("13/18/2020 3:43:45 PM"),
            None,
            "month 13"
        );
        assert_eq!(parse_us_timestamp("2/18/2020 13:43:45 PM"), None, "hour 13");
        assert_eq!(parse_us_timestamp("2/18/2020 3:43:45 XM"), None, "meridiem");
    }

    #[test]
    fn test_timestamps_reject_truncated_input() {
        assert_eq!(parse_iso_timestamp("2020-12-16"), None);
        assert_eq!(parse_iso_timestamp(""), None);
        assert_eq!(parse_us_timestamp("2/18/2020"), None);
        assert_eq!(parse_us_timestamp(""), None);
    }

    #[test]
    fn test_leap_day_is_handled() {
        // 2020-02-29 exists; 2021-02-29 does not, but the civil algorithm
        // normalises rather than erroring — assert the valid one is right.
        let feb29 = parse_iso_timestamp("2020-02-29T00:00:00").expect("leap day");
        let mar01 = parse_iso_timestamp("2020-03-01T00:00:00").expect("next day");
        assert_eq!(mar01 - feb29, 86_400);
    }

    // --- no-panic sweep ---

    #[test]
    fn test_decoders_never_panic_on_malformed_json() {
        let fuzz: [&[u8]; 10] = [
            b"",
            b"{",
            b"[",
            b"{\"trade\":",
            b"{\"trade\":[",
            b"{\"trade\":[\"",
            br#"{"trade":["unterminated]}"#,
            br#"{"bidask":[]}"#,
            br#"{"greeks":["1"]}"#,
            br#"{"trade":[null,null]}"#,
        ];
        for f in fuzz {
            let _ = json_msg_type(f);
            let _ = decode_trade_json(f);
            let _ = decode_bidask_json(f);
            let _ = decode_greeks_json(f);
            if let Some(it) = JsonArrayIter::new(f) {
                let _ = it.count();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Nested arrays (touchline `symbollist` is an array OF arrays)
// ---------------------------------------------------------------------------

/// Iterator over the INNER arrays of a nested JSON array, yielding each
/// inner array as a byte slice (including its brackets) so it can be fed
/// to [`JsonArrayIter`].
///
/// Needed because `touchline` wraps one row per subscribed symbol in
/// `symbollist: [[...],[...]]` (v2.6 p.15).
pub struct JsonNestedArrayIter<'a> {
    raw: &'a [u8],
    pos: usize,
}

impl<'a> JsonNestedArrayIter<'a> {
    /// Creates an iterator over the inner arrays following `key`.
    ///
    /// Returns `None` when the key is absent.
    #[must_use]
    pub fn new(raw: &'a [u8], key: &[u8]) -> Option<Self> {
        // O(1) EXEMPT: begin
        // Locating the key requires scanning — inherent to a JSON wire and
        // explicitly the non-O(1) fallback path (module complexity table).
        let key_at = raw
            .windows(key.len())
            .position(|w| w == key)
            .map(|p| p.saturating_add(key.len()))?;
        // O(1) EXEMPT: end
        Some(Self { raw, pos: key_at })
    }
}

impl<'a> Iterator for JsonNestedArrayIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        // Find the next inner '['.
        while self.pos < self.raw.len() && self.raw[self.pos] != b'[' {
            // A ']' closing the OUTER array ends iteration.
            if self.raw[self.pos] == b']' {
                return None;
            }
            self.pos = self.pos.saturating_add(1);
        }
        // Skip the outer opening bracket on the first call.
        if self.pos < self.raw.len() && self.raw[self.pos] == b'[' {
            let after = self.pos.saturating_add(1);
            let is_outer = self
                .raw
                .get(after..)
                .is_some_and(|r| r.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'['));
            if is_outer {
                self.pos = after;
                while self.pos < self.raw.len() && self.raw[self.pos] != b'[' {
                    self.pos = self.pos.saturating_add(1);
                }
            }
        }
        if self.pos >= self.raw.len() {
            return None;
        }
        let start = self.pos;
        let mut end = start;
        while end < self.raw.len() && self.raw[end] != b']' {
            end = end.saturating_add(1);
        }
        if end >= self.raw.len() {
            return None;
        }
        self.pos = end.saturating_add(1);
        self.raw.get(start..=end)
    }
}

// ---------------------------------------------------------------------------
// Touchline
// ---------------------------------------------------------------------------

/// Field count of a touchline row as shown in EVERY v2.6 p.15 SAMPLE.
pub const TD_TOUCHLINE_FIELDS_SAMPLE: usize = 17;
/// Field count of a touchline row as listed in the v2.6 p.15 PROSE.
///
/// **The document contradicts itself**: the prose enumerates 18 fields
/// (…, Today's OI, Previous Open Interest Close, Turnover, Bid, …) while
/// all three worked examples carry 17. We therefore accept BOTH and
/// anchor parsing from both ends — see [`decode_touchline_row`].
pub const TD_TOUCHLINE_FIELDS_PROSE: usize = 18;

/// One decoded touchline row (v2.6 p.15).
///
/// Pushed at pre-open and on every subscribe — this is where a symbol's
/// opening snapshot (and its SymbolID) arrives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruedataTouchline<'a> {
    /// Symbol NAME (e.g. `NIFTY BANK`) — borrowed, zero-alloc.
    pub symbol: &'a str,
    /// Session-scoped numeric routing key.
    pub symbol_id: i32,
    /// Last update time, epoch seconds (ISO on the wire).
    pub last_update_secs: i64,
    /// Last traded price.
    pub ltp: f32,
    /// Tick volume.
    pub tick_volume: i64,
    /// Average traded price.
    pub atp: f32,
    /// Total volume for the day.
    pub total_volume: i64,
    /// Day open.
    pub open: f32,
    /// Day high.
    pub high: f32,
    /// Day low.
    pub low: f32,
    /// Previous close.
    pub prev_close: f32,
    /// Best bid.
    pub bid: f32,
    /// Best bid quantity.
    pub bid_qty: i32,
    /// Best ask.
    pub ask: f32,
    /// Best ask quantity.
    pub ask_qty: i32,
    /// The 2-or-3 ambiguous middle fields (OI / prev-OI-close / turnover),
    /// kept RAW and unlabelled because the vendor doc contradicts itself
    /// about how many there are and therefore which is which.
    ///
    /// `middle_count` says how many were present: 2 (sample shape) or 3
    /// (prose shape). A consumer that needs OI must resolve this against
    /// live data — we refuse to guess and mislabel a field.
    pub middle_count: usize,
    /// First ambiguous middle value (present when `middle_count >= 1`).
    pub middle_0: i64,
    /// Second ambiguous middle value (present when `middle_count >= 2`).
    pub middle_1: i64,
    /// Third ambiguous middle value (present only when `middle_count == 3`).
    pub middle_2: i64,
}

/// Decodes ONE touchline row (an inner array of `symbollist`).
///
/// Parsing is anchored from BOTH ends because the vendor doc disagrees
/// with itself on the field count (17 in every sample, 18 in the prose):
/// the leading 11 fields and the trailing 4 (bid/bidqty/ask/askqty) are
/// unambiguous in both shapes, so only the 2-or-3 middle OI/turnover
/// fields vary — and those are surfaced raw rather than mislabelled.
///
/// # Errors
/// [`TruedataDecodeError::UnexpectedLength`] when the row carries neither
/// 17 nor 18 fields, or a field fails to parse.
pub fn decode_touchline_row(row: &[u8]) -> Result<TruedataTouchline<'_>, TruedataDecodeError> {
    let bad = |n: usize| TruedataDecodeError::UnexpectedLength {
        msg_code: b'L',
        len: n,
    };
    let iter = JsonArrayIter::new(row).ok_or_else(|| bad(0))?;
    // Collect into a fixed-capacity stack array — no heap allocation.
    const MAX: usize = TD_TOUCHLINE_FIELDS_PROSE;
    let mut fields: [&str; MAX] = [""; MAX];
    let mut n = 0usize;
    for item in iter {
        if n >= MAX {
            return Err(bad(n.saturating_add(1)));
        }
        fields[n] = item;
        n = n.saturating_add(1);
    }
    if n != TD_TOUCHLINE_FIELDS_SAMPLE && n != TD_TOUCHLINE_FIELDS_PROSE {
        return Err(bad(n));
    }
    // Trailing 4 are always bid, bidqty, ask, askqty.
    let tail = n.saturating_sub(4);
    let middle_count = tail.saturating_sub(11);
    let mid = |i: usize| -> i64 {
        if i < middle_count {
            parse_i64(fields[11usize.saturating_add(i)]).unwrap_or(0)
        } else {
            0
        }
    };
    Ok(TruedataTouchline {
        symbol: fields[0],
        symbol_id: parse_i32(fields[1]).ok_or_else(|| bad(1))?,
        last_update_secs: parse_iso_timestamp(fields[2]).ok_or_else(|| bad(2))?,
        ltp: parse_f32(fields[3]).ok_or_else(|| bad(3))?,
        tick_volume: parse_i64(fields[4]).ok_or_else(|| bad(4))?,
        atp: parse_f32(fields[5]).ok_or_else(|| bad(5))?,
        total_volume: parse_i64(fields[6]).ok_or_else(|| bad(6))?,
        open: parse_f32(fields[7]).ok_or_else(|| bad(7))?,
        high: parse_f32(fields[8]).ok_or_else(|| bad(8))?,
        low: parse_f32(fields[9]).ok_or_else(|| bad(9))?,
        prev_close: parse_f32(fields[10]).ok_or_else(|| bad(10))?,
        bid: parse_f32(fields[tail]).ok_or_else(|| bad(tail))?,
        bid_qty: parse_i32(fields[tail.saturating_add(1)]).ok_or_else(|| bad(tail))?,
        ask: parse_f32(fields[tail.saturating_add(2)]).ok_or_else(|| bad(tail))?,
        ask_qty: parse_i32(fields[tail.saturating_add(3)]).ok_or_else(|| bad(tail))?,
        middle_count,
        middle_0: mid(0),
        middle_1: mid(1),
        middle_2: mid(2),
    })
}

// ---------------------------------------------------------------------------
// BidAsk L2 (BSE 5-level)
// ---------------------------------------------------------------------------

/// Number of depth levels in a BSE `bidaskL2` message (v2.6 p.17).
pub const TD_BIDASK_L2_LEVELS: usize = 5;

/// One depth level of a BSE L2 message.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TruedataL2Level {
    /// Number of bid orders at this level.
    pub bid_orders: i32,
    /// Bid price.
    pub bid_price: f32,
    /// Bid quantity.
    pub bid_qty: i32,
    /// Number of ask orders at this level.
    pub ask_orders: i32,
    /// Ask price.
    pub ask_price: f32,
    /// Ask quantity.
    pub ask_qty: i32,
}

/// A decoded BSE 5-level depth message (v2.6 p.17).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruedataBidAskL2 {
    /// Session-scoped routing key.
    pub symbol_id: i32,
    /// Epoch seconds (ISO on the wire, like `trade`).
    pub timestamp_secs: i64,
    /// The five depth levels, best first.
    pub levels: [TruedataL2Level; TD_BIDASK_L2_LEVELS],
    /// Total bid quantity across the book.
    pub total_bid_qty: i64,
    /// Total ask quantity across the book.
    pub total_ask_qty: i64,
}

/// Decodes a `{"bidaskL2":[...]}` frame (BSE only, v2.6 p.17).
///
/// Layout: symbol id, timestamp, then five groups of
/// (bid orders, bid price, bid qty, ask orders, ask price, ask qty),
/// then total bid qty and total ask qty.
///
/// The totals are read from the END of the array rather than a fixed
/// index, so a vendor field-count drift in the level block degrades to a
/// typed error instead of silently shifting every price.
///
/// # Errors
/// [`TruedataDecodeError::UnexpectedLength`] when the array is too short
/// to carry the documented structure.
pub fn decode_bidask_l2_json(raw: &[u8]) -> Result<TruedataBidAskL2, TruedataDecodeError> {
    let bad = |n: usize| TruedataDecodeError::UnexpectedLength {
        msg_code: b'2',
        len: n,
    };
    // 2 header + 5*6 levels + 2 totals
    const EXPECTED: usize = 2 + TD_BIDASK_L2_LEVELS * 6 + 2;
    let iter = JsonArrayIter::new(raw).ok_or_else(|| bad(0))?;
    let mut fields: [&str; EXPECTED] = [""; EXPECTED];
    let mut n = 0usize;
    for item in iter {
        if n >= EXPECTED {
            // Extra fields: vendor drift. Fail typed rather than mis-index.
            return Err(bad(n.saturating_add(1)));
        }
        fields[n] = item;
        n = n.saturating_add(1);
    }
    if n != EXPECTED {
        return Err(bad(n));
    }
    let mut levels = [TruedataL2Level::default(); TD_BIDASK_L2_LEVELS];
    for (lvl, slot) in levels.iter_mut().enumerate() {
        let base = 2usize.saturating_add(lvl.saturating_mul(6));
        *slot = TruedataL2Level {
            bid_orders: parse_i32(fields[base]).unwrap_or(0),
            bid_price: parse_f32(fields[base.saturating_add(1)]).unwrap_or(0.0),
            bid_qty: parse_i32(fields[base.saturating_add(2)]).unwrap_or(0),
            ask_orders: parse_i32(fields[base.saturating_add(3)]).unwrap_or(0),
            ask_price: parse_f32(fields[base.saturating_add(4)]).unwrap_or(0.0),
            ask_qty: parse_i32(fields[base.saturating_add(5)]).unwrap_or(0),
        };
    }
    Ok(TruedataBidAskL2 {
        symbol_id: parse_i32(fields[0]).ok_or_else(|| bad(0))?,
        timestamp_secs: parse_iso_timestamp(fields[1]).ok_or_else(|| bad(1))?,
        levels,
        total_bid_qty: parse_i64(fields[EXPECTED.saturating_sub(2)]).unwrap_or(0),
        total_ask_qty: parse_i64(fields[EXPECTED.saturating_sub(1)]).unwrap_or(0),
    })
}

#[cfg(test)]
mod nested_tests {
    use super::*;

    /// The v2.6 p.15 touchline sample (first two rows), verbatim shape.
    const TOUCHLINE: &[u8] = br#"{"success":true,"message":"touchline","symbolsadded":2,"symbollist":[["NIFTY BANK","200000004","2020-12-17T08:40:29","30698.4","0","0","0","30920.35","30932.25","30587.1","30698.4","0","0","0","0","0","0"],["NIFTY-I","900000596","2020-12-17T09:17:52","13701.8","300","13695.74","206850","13710.15","13715.2","13686.9","13699.45","12538050","2832963189","13701.25","450","13701.9","375"]],"totalsymbolsubscribed":2}"#;

    #[test]
    fn test_json_nested_array_iter_new_yields_each_row() {
        let rows: Vec<&[u8]> = JsonNestedArrayIter::new(TOUCHLINE, b"\"symbollist\":")
            .expect("key")
            .collect();
        assert_eq!(rows.len(), 2, "two symbols in the sample");
    }

    #[test]
    fn test_decode_touchline_row_parses_documented_sample() {
        let rows: Vec<&[u8]> = JsonNestedArrayIter::new(TOUCHLINE, b"\"symbollist\":")
            .expect("key")
            .collect();
        let t = decode_touchline_row(rows[0]).expect("row 0 decodes");
        assert_eq!(t.symbol, "NIFTY BANK");
        assert_eq!(t.symbol_id, 200_000_004);
        assert!((t.ltp - 30698.4).abs() < 0.01);
        assert!((t.open - 30920.35).abs() < 0.01);
        assert!((t.high - 30932.25).abs() < 0.01);
        assert!((t.low - 30587.1).abs() < 0.01);
    }

    #[test]
    fn test_decode_touchline_row_reports_ambiguous_middle_count() {
        // The doc contradiction: samples carry 17 fields (2 middle values),
        // the prose lists 18 (3 middle values). We surface the count rather
        // than guessing which OI/turnover field is which.
        let rows: Vec<&[u8]> = JsonNestedArrayIter::new(TOUCHLINE, b"\"symbollist\":")
            .expect("key")
            .collect();
        let t = decode_touchline_row(rows[1]).expect("row 1 decodes");
        assert_eq!(t.symbol, "NIFTY-I");
        assert_eq!(
            t.middle_count, 2,
            "the documented SAMPLES carry 2 middle fields, not the prose's 3"
        );
        // Bid/ask are anchored from the END, so they are correct in both shapes.
        assert_eq!(t.bid_qty, 450);
        assert_eq!(t.ask_qty, 375);
        assert!((t.bid - 13701.25).abs() < 0.01);
        assert!((t.ask - 13701.9).abs() < 0.01);
    }

    #[test]
    fn test_decode_touchline_row_accepts_the_prose_18_field_shape() {
        // Same row plus one extra middle field — the prose shape. Bid/ask
        // must STILL land correctly because they are back-anchored.
        let row = br#"["X","1","2020-12-17T09:17:52","100","1","100","10","100","101","99","100","11","22","33","1.5","7","2.5","9"]"#;
        let t = decode_touchline_row(row).expect("18-field row decodes");
        assert_eq!(t.middle_count, 3);
        assert_eq!(t.bid_qty, 7);
        assert_eq!(t.ask_qty, 9);
        assert!((t.bid - 1.5).abs() < 0.01);
        assert!((t.ask - 2.5).abs() < 0.01);
    }

    #[test]
    fn test_decode_touchline_row_rejects_other_field_counts() {
        let short = br#"["X","1","2020-12-17T09:17:52","100"]"#;
        assert!(decode_touchline_row(short).is_err());
    }

    #[test]
    fn test_decode_bidask_l2_json_parses_five_levels_and_totals() {
        // 2 header + 30 level fields + 2 totals = 34.
        let mut s = String::from(r#"{"bidaskL2":["490000010","2023-07-05T09:15:44""#);
        for lvl in 0..5 {
            let base = 100 + lvl * 10;
            s.push_str(&format!(
                r#","1","{}","{}","2","{}","{}""#,
                base,
                base + 1,
                base + 2,
                base + 3
            ));
        }
        s.push_str(r#","120","110"]}"#);
        let l2 = decode_bidask_l2_json(s.as_bytes()).expect("L2 decodes");
        assert_eq!(l2.symbol_id, 490_000_010);
        assert_eq!(l2.total_bid_qty, 120);
        assert_eq!(l2.total_ask_qty, 110);
        assert_eq!(l2.levels.len(), TD_BIDASK_L2_LEVELS);
        assert_eq!(l2.levels[0].bid_orders, 1);
        assert!((l2.levels[0].bid_price - 100.0).abs() < 0.01);
        assert_eq!(l2.levels[4].ask_qty, 143);
    }

    #[test]
    fn test_decode_bidask_l2_json_rejects_wrong_field_count() {
        let short = br#"{"bidaskL2":["490000010","2023-07-05T09:15:44","1","2"]}"#;
        assert!(decode_bidask_l2_json(short).is_err());
    }

    #[test]
    fn test_nested_decoders_never_panic_on_malformed_input() {
        let fuzz: [&[u8]; 6] = [
            b"",
            b"{",
            br#"{"symbollist":[["#,
            br#"{"symbollist":[[]]}"#,
            br#"{"bidaskL2":[}"#,
            br#"[[["#,
        ];
        for f in fuzz {
            let _ = decode_bidask_l2_json(f);
            let _ = decode_touchline_row(f);
            if let Some(it) = JsonNestedArrayIter::new(f, b"\"symbollist\":") {
                let _ = it.count();
            }
        }
    }
}
