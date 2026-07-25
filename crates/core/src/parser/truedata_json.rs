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
/// Bounded by TD_JSON_SNIFF_WINDOW (128 B) on the per-tick pass and by
/// TD_JSON_CONTROL_SNIFF_WINDOW (8192 B) on the rare control pass, plus a
/// fixed needle set — all compile-time constants, so a constant upper
/// bound, not growth in n. Data frames exit in the narrow pass, so the
/// wide bound is never paid per tick.
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
    malformed: bool,
    finished: bool,
}

impl<'a> JsonArrayIter<'a> {
    /// Creates an iterator positioned at the array that is the VALUE of a
    /// top-level key.
    ///
    /// Anchoring is string-aware: a `[` that appears INSIDE a quoted string
    /// is skipped. Without that, a frame like
    /// `{"message":"bad [req]","trade":[…]}` would anchor on the bracket
    /// inside the message text and iterate garbage that happens to parse.
    ///
    /// Returns `None` when the frame contains no array outside a string.
    #[must_use]
    pub fn new(raw: &'a [u8]) -> Option<Self> {
        // O(1) EXEMPT: begin
        // Locating the array opener requires scanning bytes — inherent to
        // ANY JSON wire, and this module is explicitly the NON-O(1)
        // fallback path (see the module complexity table and scope-lock
        // §11.3). The O(1) field extraction is the binary decoder in
        // `super::truedata`, which uses fixed offsets and never scans.
        // The scan is bounded by the frame length, which the transport
        // caps below the WebSocket max frame size.
        let mut i = 0_usize;
        let mut in_string = false;
        let mut start = None;
        while i < raw.len() {
            let b = raw[i];
            if in_string {
                if b == b'\\' {
                    // Skip the escaped byte so an escaped quote cannot end
                    // the string early.
                    i = i.saturating_add(2);
                    continue;
                }
                if b == b'"' {
                    in_string = false;
                }
            } else if b == b'"' {
                in_string = true;
            } else if b == b'[' {
                start = Some(i);
                break;
            }
            i = i.saturating_add(1);
        }
        // O(1) EXEMPT: end
        let start = start?;
        Some(Self {
            raw,
            pos: start.saturating_add(1),
            malformed: false,
            finished: false,
        })
    }

    /// `true` when iteration stopped because the array was BROKEN rather
    /// than because it ended cleanly at `]`.
    ///
    /// This distinction is load-bearing. `decode_trade_json` detects the
    /// optional bid/ask tail by `next()` returning `None` — so without it,
    /// an unterminated or non-UTF-8 field 16 makes a 19-field trade decode
    /// silently as the 15-field quote-only shape, with `bid`/`ask` reported
    /// as absent rather than as an error. Silent degradation of a tick is
    /// worse than a typed refusal: the ledger would count it as decoded and
    /// nothing anywhere would say the frame was damaged.
    #[inline]
    #[must_use]
    pub const fn is_malformed(&self) -> bool {
        self.malformed
    }

    /// `true` once the closing `]` has been consumed.
    ///
    /// Lets a decoder assert that a frame carried EXACTLY the documented
    /// field count: after reading its last expected field, a well-formed
    /// array must be finished. Extra trailing fields mean the vendor
    /// changed the shape, and continuing to trust the earlier positions
    /// would be a guess.
    #[inline]
    #[must_use]
    pub const fn is_finished(&self) -> bool {
        self.finished
    }
}

impl<'a> Iterator for JsonArrayIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        // Skip separators and whitespace to the start of the next element.
        while self.pos < self.raw.len() {
            match self.raw[self.pos] {
                b',' | b' ' | b'\t' | b'\n' | b'\r' => {
                    self.pos = self.pos.saturating_add(1);
                }
                b']' => {
                    // The ONLY clean end of the array.
                    self.pos = self.pos.saturating_add(1);
                    self.finished = true;
                    return None;
                }
                _ => break,
            }
        }
        if self.pos >= self.raw.len() {
            // Ran off the end without ever seeing `]` — unterminated, not
            // finished.
            self.malformed = true;
            return None;
        }

        if self.raw[self.pos] == b'"' {
            return self.next_quoted();
        }
        // An UNQUOTED token: a bare number, `null`, `true`, `false`. The
        // documented samples quote every value, but a vendor change to bare
        // numbers must not make elements INVISIBLE — skipping them would
        // shift every later field left and silently mis-assign prices to
        // quantities. So they are yielded verbatim and the caller's own
        // parse decides whether the content is acceptable.
        self.next_unquoted()
    }
}

impl<'a> JsonArrayIter<'a> {
    /// Reads a `"…"` element, honouring backslash escapes.
    ///
    /// The yielded slice is the RAW inner text, escapes NOT expanded —
    /// unescaping would require an allocation, and no numeric or symbol
    /// field TrueData sends contains one. What matters here is that an
    /// escaped quote does not terminate the element early: that would make
    /// the real closing quote read as an opening quote and desynchronise
    /// every remaining field in the array.
    fn next_quoted(&mut self) -> Option<&'a str> {
        let start = self.pos.saturating_add(1);
        let mut end = start;
        while end < self.raw.len() {
            match self.raw[end] {
                b'\\' => end = end.saturating_add(2),
                b'"' => break,
                _ => end = end.saturating_add(1),
            }
        }
        if end >= self.raw.len() {
            // Opening quote with no closing quote (or a trailing lone
            // backslash that ran the cursor past the end).
            self.malformed = true;
            return None;
        }
        self.pos = end.saturating_add(1);
        match self
            .raw
            .get(start..end)
            .and_then(|b| core::str::from_utf8(b).ok())
        {
            Some(field) => Some(field),
            None => {
                self.malformed = true;
                None
            }
        }
    }

    /// Reads a bare token up to the next `,` or `]`.
    fn next_unquoted(&mut self) -> Option<&'a str> {
        let start = self.pos;
        let mut end = start;
        while end < self.raw.len() && !matches!(self.raw[end], b',' | b']') {
            end = end.saturating_add(1);
        }
        if end >= self.raw.len() {
            self.malformed = true;
            return None;
        }
        self.pos = end;
        match self
            .raw
            .get(start..end)
            .and_then(|b| core::str::from_utf8(b).ok())
        {
            Some(field) => Some(field.trim()),
            None => {
                self.malformed = true;
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Number parsing (zero-alloc, total)
// ---------------------------------------------------------------------------

/// Parses an `i32`. An empty field is an ERROR.
///
/// Empty-tolerance used to apply to every numeric field, which meant a
/// blank LTP silently became `0.0` and a blank symbol id routed to symbol
/// zero. Only the two fields TrueData is actually known to blank get the
/// tolerant variant below; everything else must fail loudly.
#[inline]
fn parse_i32(s: &str) -> Option<i32> {
    s.trim().parse::<i32>().ok()
}

/// Parses an `i64`. An empty field is an ERROR — see [`parse_i32`].
#[inline]
fn parse_i64(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}

/// Parses an `i64` for the OI slots, where an empty field means "absent".
///
/// Open interest is genuinely absent for cash-segment instruments, and the
/// vendor blanks it rather than sending `0`. Rejecting the whole tick would
/// mean rejecting every equity print, so these two slots — and ONLY these
/// two — treat empty as zero.
#[inline]
fn parse_i64_or_absent(s: &str) -> Option<i64> {
    if s.trim().is_empty() {
        return Some(0);
    }
    s.trim().parse::<i64>().ok()
}

/// Parses a FINITE `f32`. Empty, `NaN`, `inf` and out-of-range are errors.
///
/// `"NaN"` and `"inf"` parse successfully in Rust, and `1e40` overflows to
/// `inf` — all three would enter price fields and poison every downstream
/// min/max comparison silently. A price that is not a finite number is not
/// a price.
#[inline]
fn parse_f32(s: &str) -> Option<f32> {
    let v = s.trim().parse::<f32>().ok()?;
    v.is_finite().then_some(v)
}

/// Parses a FINITE `f64` — see [`parse_f32`].
#[inline]
fn parse_f64(s: &str) -> Option<f64> {
    let v = s.trim().parse::<f64>().ok()?;
    v.is_finite().then_some(v)
}

// ---------------------------------------------------------------------------
// Timestamps — TWO formats on the same socket
// ---------------------------------------------------------------------------

/// Lowest year either timestamp parser will accept.
///
/// The epoch itself — anything earlier is a corrupt or hostile field, not
/// a market timestamp.
pub const TD_MIN_YEAR: i64 = 1970;

/// Highest year either timestamp parser will accept.
///
/// Deliberately generous but FINITE. The bound exists for overflow safety,
/// not plausibility: `days_from_civil` computes `y - 1` and `era * 146_097`,
/// and the release profile sets `overflow-checks = true`, so an unbounded
/// year from the wire is a PANIC — `i64::MIN` underflows the first
/// expression and any year past ~2.5e16 overflows the second. The ISO
/// parser reads a fixed 4-byte field and could not exceed 9999, but the US
/// parser splits on `/` and parses an arbitrary-length token, so the bound
/// is enforced in both rather than relying on one parser's field width.
pub const TD_MAX_YEAR: i64 = 2200;

/// Days in a given month, leap-year aware.
///
/// `days_from_civil` NORMALISES an out-of-range day — Feb 30 silently
/// becomes Mar 2 — so a plain `1..=31` check lets a corrupt date through as
/// a plausible-looking timestamp. This is the check that makes an invalid
/// date an error instead of a different valid date.
#[inline]
#[must_use]
pub const fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            // Gregorian leap rule. `y` is caller-validated in range, so the
            // remainders cannot overflow.
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// `true` when a year is inside the range both parsers accept.
#[inline]
#[must_use]
pub const fn is_supported_year(y: i64) -> bool {
    // Range pattern rather than a containment call — see the note in
    // `parse_iso_timestamp`.
    matches!(y, TD_MIN_YEAR..=TD_MAX_YEAR)
}

/// Days from civil epoch (1970-01-01) — Howard Hinnant's algorithm.
///
/// Pure integer arithmetic, no allocation, no `chrono` round-trip.
///
/// # Panics in release without the caller's range check
/// Callers MUST reject years outside [`TD_MIN_YEAR`]..=[`TD_MAX_YEAR`]
/// first. See [`TD_MAX_YEAR`] for exactly which expressions overflow.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: callers enforce is_supported_year + m/d ranges
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
///
/// # Panics in release without the caller's range check
/// See [`days_from_civil`].
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
    if !is_supported_year(y) || !matches!(mo, 1..=12) {
        return None;
    }
    if d < 1 || d > days_in_month(y, mo) {
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
    //
    // The YEAR check is not cosmetic here: unlike the ISO parser this one
    // splits on `/` and parses an arbitrary-length token, so without the
    // bound a crafted field reaches `days_from_civil` and PANICS in release
    // (overflow-checks = true) — a wire-reachable crash of the feed task.
    if !is_supported_year(y) || !matches!(mo, 1..=12) {
        return None;
    }
    if d < 1 || d > days_in_month(y, mo) {
        return None;
    }
    if !matches!(h12, 1..=12) || mi > 59 || sec > 60 {
        return None;
    }

    // 12-hour → 24-hour: 12 AM is 00, 12 PM is 12.
    //
    // `eq_ignore_ascii_case` rather than `to_ascii_uppercase()`: the latter
    // ALLOCATES A STRING, and `bidask` is a continuous per-tick data frame,
    // so that would be heap traffic on the hot path (principle 1).
    let meridiem = meridiem.trim();
    let h24 = if meridiem.eq_ignore_ascii_case("AM") {
        if h12 == 12 { 0 } else { h12 }
    } else if meridiem.eq_ignore_ascii_case("PM") {
        if h12 == 12 {
            12
        } else {
            h12.saturating_add(12)
        }
    } else {
        return None;
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
    // OI is legitimately blank on cash-segment instruments — the only two
    // slots that tolerate an empty field.
    let oi = parse_i64_or_absent(next().ok_or_else(|| bad(10))?).ok_or_else(|| bad(11))?;
    let prev_oi = parse_i64_or_absent(next().ok_or_else(|| bad(11))?).ok_or_else(|| bad(12))?;
    let turnover = parse_f64(next().ok_or_else(|| bad(12))?).ok_or_else(|| bad(13))?;
    let special_tag_str = next().ok_or_else(|| bad(13))?;
    let seq_no = parse_i32(next().ok_or_else(|| bad(14))?).ok_or_else(|| bad(15))?;

    // Special tag is "O"/"H"/"L"/"OHL"/"" — store the first byte, 0 when blank.
    let special_tag = special_tag_str.as_bytes().first().copied().unwrap_or(0);

    // Optional bid/ask tail (the 19-field shape).
    //
    // `None` here is ambiguous on its face — it means either "the array
    // ended after 15 fields" (legitimate quote-only shape) or "field 16 is
    // broken". Only the first is acceptable; the iterator's `is_malformed`
    // flag separates them, so a damaged frame errors instead of silently
    // decoding as quote-only.
    let (bid, bid_qty, ask, ask_qty) = match next() {
        Some(bid_s) => {
            let bid = parse_f32(bid_s).ok_or_else(|| bad(16))?;
            let bid_qty = parse_i32(next().ok_or_else(|| bad(16))?).ok_or_else(|| bad(17))?;
            let ask = parse_f32(next().ok_or_else(|| bad(17))?).ok_or_else(|| bad(18))?;
            let ask_qty = parse_i32(next().ok_or_else(|| bad(18))?).ok_or_else(|| bad(19))?;
            (Some(bid), Some(bid_qty), Some(ask), Some(ask_qty))
        }
        None => {
            if it.is_malformed() {
                return Err(bad(16));
            }
            (None, None, None, None)
        }
    };

    // The array must be EXHAUSTED here. A vendor-added 20th field would
    // otherwise be dropped in silence while we keep trusting positions 1-19
    // that may have shifted — the field-count constants existed but nothing
    // read them.
    if !it.is_finished() {
        if it.next().is_some() {
            return Err(bad(TD_JSON_TRADE_FIELDS_FULL.saturating_add(1)));
        }
        if it.is_malformed() {
            return Err(bad(TD_JSON_TRADE_FIELDS_FULL));
        }
    }

    Ok(TruedataTrade {
        symbol_id,
        // The struct mirrors the binary wire, where the timestamp is a
        // 4-byte int. `unwrap_or(i32::MAX)` was wrong in one direction: a
        // PRE-1970 date underflows and would have saturated UPWARDS to
        // 2038 — a sign flip presented as a valid recent timestamp. Clamp
        // to the nearest representable instant instead, so an out-of-range
        // date stays out of range.
        timestamp_secs: i32::try_from(
            timestamp_secs.clamp(i64::from(i32::MIN), i64::from(i32::MAX)),
        )
        .unwrap_or(i32::MAX),
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
    // FIELD ORDER: the SDK's JSON dataclass, NOT the v2.6 p.17 prose.
    //
    // The prose lists "delta, gamma, theta, vega, rho, IV" — IV last. The
    // vendor's own client reads iv, delta, theta, gamma, vega, rho, and the
    // PDF's OWN sample settles it: [0.2015, 0.0331, -6.0417, 0.0005,
    // 0.8335, 0.0198] under the prose order gives IV = 1.98% with
    // vega = 0.0005, which is not a real option; under the SDK order it
    // gives IV = 20.15%, gamma = 0.0005, vega = 0.83 — coherent. The prose
    // contradicts its own example, so the SDK wins.
    //
    // UNRESOLVED (day-0 probe): the SDK is internally inconsistent about
    // gamma vs theta — its BINARY map puts gamma@25 before theta@33, its
    // JSON order puts theta before gamma. The two are read from different
    // positions here for exactly that reason. Greeks are backend-enabled
    // and never subscribed, so nothing depends on it today.
    Ok(TruedataGreeks {
        symbol_id,
        timestamp_secs,
        iv: num(2)?,
        delta: num(3)?,
        theta: num(4)?,
        gamma: num(5)?,
        vega: num(6)?,
        rho: num(7)?,
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
        // Field order is the SDK's, NOT the v2.6 p.17 prose. The sample
        // values are what prove it: read in prose order they describe an
        // option with 1.98% implied vol and a vega of 0.0005, which does
        // not exist; read in SDK order they describe a normal one.
        let g = decode_greeks_json(GREEKS).expect("decode");
        assert_eq!(g.symbol_id, 301_680_343);
        assert!((g.iv - 0.2015).abs() < 1e-9, "IV is FIRST, not last");
        assert!((g.delta - 0.0331).abs() < 1e-9);
        assert!((g.theta + 6.0417).abs() < 1e-9, "theta is negative");
        assert!((g.gamma - 0.0005).abs() < 1e-9);
        assert!((g.vega - 0.8335).abs() < 1e-9);
        assert!((g.rho - 0.0198).abs() < 1e-9);
    }

    #[test]
    fn test_decode_greeks_json_prose_order_would_be_implausible() {
        // Guards the decision above rather than just its output: if someone
        // "fixes" the order back to the PDF prose, these two implausible
        // values are what they would be asserting.
        let g = decode_greeks_json(GREEKS).expect("decode");
        assert!(
            g.iv > 0.05,
            "an implied vol under 5% means the prose order crept back in"
        );
        assert!(
            g.vega > g.gamma,
            "vega above gamma is the coherence check that settles the order"
        );
    }

    // --- timestamps: the two-format trap ---

    #[test]
    fn test_is_supported_year_bounds_the_overflow_window() {
        // The bound is not about plausibility — it is what stops
        // `days_from_civil` from overflowing and PANICKING in release.
        assert!(is_supported_year(TD_MIN_YEAR));
        assert!(is_supported_year(TD_MAX_YEAR));
        assert!(is_supported_year(2026));
        assert!(!is_supported_year(TD_MIN_YEAR - 1));
        assert!(!is_supported_year(TD_MAX_YEAR + 1));
        assert!(!is_supported_year(i64::MIN));
        assert!(!is_supported_year(i64::MAX));
    }

    #[test]
    fn test_days_in_month_handles_the_leap_rule() {
        assert_eq!(days_in_month(2024, 2), 29, "divisible by 4");
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(1900, 2), 28, "century, not divisible by 400");
        assert_eq!(days_in_month(2000, 2), 29, "divisible by 400");
        assert_eq!(days_in_month(2026, 1), 31);
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 12), 31);
        assert_eq!(days_in_month(2026, 0), 0, "out of range yields no days");
        assert_eq!(days_in_month(2026, 13), 0);
    }

    #[test]
    fn test_timestamp_parsers_survive_extreme_years_without_panicking() {
        // These are the exact inputs that used to reach `days_from_civil`
        // and panic under overflow-checks. They must return None.
        assert_eq!(
            parse_us_timestamp("1/1/-9223372036854775808 1:00:00 AM"),
            None
        );
        assert_eq!(parse_us_timestamp("1/1/99999999999999999 1:00:00 AM"), None);
        assert_eq!(parse_us_timestamp("1/1/1969 1:00:00 AM"), None);
        assert_eq!(parse_iso_timestamp("0001-01-01T00:00:00"), None);
        assert_eq!(parse_iso_timestamp("9999-01-01T00:00:00"), None);
    }

    #[test]
    fn test_timestamp_parsers_reject_impossible_calendar_days() {
        // `days_from_civil` NORMALISES an out-of-range day, so Feb 30 would
        // silently become Mar 2 — a different valid timestamp, not an error.
        assert_eq!(parse_iso_timestamp("2021-02-30T10:00:00"), None);
        assert_eq!(
            parse_iso_timestamp("2023-02-29T10:00:00"),
            None,
            "not a leap year"
        );
        assert!(
            parse_iso_timestamp("2024-02-29T10:00:00").is_some(),
            "leap year"
        );
        assert_eq!(parse_iso_timestamp("2021-04-31T10:00:00"), None);
        assert_eq!(parse_us_timestamp("2/30/2021 1:00:00 AM"), None);
    }

    #[test]
    fn test_parse_i64_or_absent_tolerates_blank_oi_only() {
        // OI is genuinely blank on cash-segment instruments; every other
        // numeric field must fail loudly rather than become zero.
        assert_eq!(parse_i64_or_absent(""), Some(0));
        assert_eq!(parse_i64_or_absent("  "), Some(0));
        assert_eq!(parse_i64_or_absent("123"), Some(123));
        assert_eq!(parse_i64_or_absent("abc"), None);
    }

    #[test]
    fn test_blank_price_is_an_error_not_a_zero() {
        // A blank LTP used to decode as 0.0 — a real price of zero, fed to
        // every downstream consumer with no complaint.
        assert!(parse_f32("").is_none());
        assert!(parse_i32("").is_none());
        assert!(parse_i64("").is_none());
    }

    #[test]
    fn test_non_finite_numbers_are_rejected() {
        // "NaN" and "inf" parse successfully in Rust, and 1e40 overflows an
        // f32 to inf. None of the three is a price.
        assert!(parse_f32("NaN").is_none());
        assert!(parse_f32("inf").is_none());
        assert!(parse_f32("-inf").is_none());
        assert!(parse_f32("1e40").is_none(), "overflows f32 to infinity");
        assert!(parse_f64("NaN").is_none());
        assert!(parse_f64("inf").is_none());
    }

    #[test]
    fn test_json_array_iter_is_malformed_separates_broken_from_ended() {
        // Clean end.
        let mut it = JsonArrayIter::new(br#"["a","b"]"#).expect("iter");
        assert_eq!(it.next(), Some("a"));
        assert_eq!(it.next(), Some("b"));
        assert_eq!(it.next(), None);
        assert!(!it.is_malformed(), "a clean `]` is not malformed");
        assert!(it.is_finished());

        // Unterminated array.
        let mut it = JsonArrayIter::new(br#"["a","b""#).expect("iter");
        assert_eq!(it.next(), Some("a"));
        assert_eq!(it.next(), Some("b"));
        assert_eq!(it.next(), None);
        assert!(it.is_malformed(), "no closing `]` is malformed");
        assert!(!it.is_finished());

        // Unterminated element.
        let mut it = JsonArrayIter::new(br#"["a","bbb"#).expect("iter");
        assert_eq!(it.next(), Some("a"));
        assert_eq!(it.next(), None);
        assert!(it.is_malformed());
    }

    #[test]
    fn test_json_array_iter_is_finished_only_after_the_closing_bracket() {
        let mut it = JsonArrayIter::new(br#"["a"]"#).expect("iter");
        assert!(!it.is_finished(), "not finished before iterating");
        assert_eq!(it.next(), Some("a"));
        assert!(
            !it.is_finished(),
            "not finished while an element was just read"
        );
        assert_eq!(it.next(), None);
        assert!(it.is_finished());
    }

    #[test]
    fn test_json_array_iter_handles_escaped_quotes_without_desyncing() {
        // Zero escape handling used to end the element at the escaped
        // quote; the real closing quote then read as an OPENING quote and
        // every later field shifted by one.
        let mut it = JsonArrayIter::new(br#"["A\"B","1234"]"#).expect("iter");
        assert_eq!(it.next(), Some(r#"A\"B"#));
        assert_eq!(
            it.next(),
            Some("1234"),
            "the field after an escaped quote must survive"
        );
        assert_eq!(it.next(), None);
        assert!(!it.is_malformed());
    }

    #[test]
    fn test_json_array_iter_yields_unquoted_tokens_rather_than_skipping_them() {
        // Bare numbers used to be INVISIBLE: skipped silently, shifting
        // every later field left and mis-assigning prices to quantities.
        let mut it = JsonArrayIter::new(br#"["100",1472.8,"635",null,true]"#).expect("iter");
        assert_eq!(it.next(), Some("100"));
        assert_eq!(it.next(), Some("1472.8"), "a bare number is an ELEMENT");
        assert_eq!(it.next(), Some("635"));
        assert_eq!(it.next(), Some("null"));
        assert_eq!(it.next(), Some("true"));
        assert_eq!(it.next(), None);
    }

    #[test]
    fn test_json_array_iter_skips_a_bracket_inside_a_string() {
        // Anchoring on the first `[` anywhere would start iterating inside
        // the message text.
        let it =
            JsonArrayIter::new(br#"{"message":"bad [req]","trade":["42","7"]}"#).expect("iter");
        let got: [Option<&str>; 3] = {
            let mut it = it;
            [it.next(), it.next(), it.next()]
        };
        assert_eq!(got[0], Some("42"), "must anchor on the real array");
        assert_eq!(got[1], Some("7"));
        assert_eq!(got[2], None);
    }

    #[test]
    fn test_decode_trade_json_rejects_extra_trailing_fields() {
        // A vendor-added 20th field used to be dropped in silence while we
        // kept trusting positions 1-19 that may have shifted.
        let mut frame = Vec::from(&TRADE_19[..TRADE_19.len() - 2]);
        frame.extend_from_slice(br#","999"]}"#);
        assert!(
            decode_trade_json(&frame).is_err(),
            "a 20-field trade must error, not decode as 19"
        );
    }

    #[test]
    fn test_json_nested_array_iter_rejects_an_empty_key() {
        // `windows(0)` panics, and the release profile aborts on panic.
        assert!(JsonNestedArrayIter::new(b"[[\"a\"]]", b"").is_none());
    }

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
        // `slice::windows(0)` PANICS, and the release profile aborts on
        // panic. A public constructor accepting an arbitrary key must not
        // carry that edge.
        if key.is_empty() {
            return None;
        }
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

/// Refuses a `{"bidaskL2":[...]}` frame (BSE 5-level depth).
///
/// This is a decoder-shaped REFUSAL, matching
/// [`super::truedata_aux::decode_bidask_l2_binary`]. Both paths refuse the
/// same wire for the same reason, so no route can ever emit depth we are
/// not sure of.
///
/// # Why it is refused
///
/// The layout is contested three ways, and this decoder previously guessed
/// one of the three:
///
/// 1. **Field count.** It expected 34 elements (2 header + 5×6 + 2). The
///    v2.6 p.17 sample carries **35** — there is a `seq_no` after the
///    timestamp that the prose never mentions. So the decoder would have
///    rejected the vendor's own documented frame, while the unit test
///    passed because its fixture was built to the wrong shape.
/// 2. **Field order within a level.** The prose says
///    `count, price, qty`; the SDK's binary map says `price, qty, count`.
/// 3. **Grouping.** The prose reads as five bid/ask groups; decoding the
///    PDF's actual sample only yields a coherent book (bids descending
///    65280 → 65120.75 → 65050 → 65010, asks ascending
///    65540.75 → 65980 → 65989) under an INTERLEAVED reading.
///
/// # The resolved candidate, for the day-0 probe
///
/// `[symbol_id, timestamp, seq_no]` then five levels of
/// `(bid_price, bid_qty, bid_orders, ask_price, ask_qty, ask_orders)`,
/// then `total_bid_qty, total_ask_qty` — 35 elements; the binary twin is a
/// uniform 24 bytes per level starting at offset 13, totals at 133/137,
/// **141 bytes**. One captured frame confirms or refutes this. Until one
/// does, wrong depth is strictly worse than absent depth, and BSE 5-level
/// depth is outside the subscription scope anyway.
///
/// # Errors
/// Always [`TruedataDecodeError::UnexpectedLength`] with `msg_code = b'2'`,
/// carrying the observed element count so a captured frame can be compared
/// against the candidate above.
pub fn decode_bidask_l2_json(raw: &[u8]) -> Result<TruedataBidAskL2, TruedataDecodeError> {
    let n = JsonArrayIter::new(raw).map_or(0, Iterator::count);
    Err(TruedataDecodeError::UnexpectedLength {
        msg_code: b'2',
        len: n,
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
    fn test_decode_bidask_l2_json_refuses_rather_than_guessing() {
        // The v2.6 p.17 sample, VERBATIM — 35 elements, including the
        // seq_no the prose never mentions. The previous decoder expected
        // 34 and would have rejected this real frame while its own
        // hand-built 34-field fixture passed.
        let sample = br#"{"bidaskL2":["490000010","2023-07-05T09:15:44","0","65280","10","1","65540.75","10","1","65120.75","10","1","65980","50","1","65050","50","1","65989","50","1","65010","50","1","0","0","0","0","0","0","0","0","0","120","110"]}"#;
        let got = decode_bidask_l2_json(sample);
        assert!(
            matches!(
                got,
                Err(TruedataDecodeError::UnexpectedLength { msg_code: b'2', .. })
            ),
            "L2 must refuse until the layout is probed, got {got:?}"
        );
    }

    #[test]
    fn test_decode_bidask_l2_json_refusal_reports_the_element_count() {
        // The count is the whole point of the refusal: it is what a
        // captured frame gets compared against on day 0.
        let short = br#"{"bidaskL2":["490000010","2023-07-05T09:15:44","1","2"]}"#;
        assert_eq!(
            decode_bidask_l2_json(short),
            Err(TruedataDecodeError::UnexpectedLength {
                msg_code: b'2',
                len: 4
            })
        );
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
