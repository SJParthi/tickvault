//! Our own ILP encoder and sender for the tick writer's rows (plan ITEM DT,
//! owner decision 2026-10-30: "Yes, build our own").
//!
//! # Why
//!
//! Building one `ticks` row through `questdb::ingress::Buffer` cost about
//! 9,400 of the ~10,400 instructions a row takes (callgrind, 2026-10-30):
//! the library formats every price with ryu, escapes every column name on
//! every row, and copies each piece with a separate small `memcpy`. Its API
//! has no way to append bytes formatted elsewhere, and its `Sender` sends only
//! its own `Buffer`, so a faster row needs a faster buffer AND a sender that
//! takes raw bytes.
//!
//! # What this module is
//!
//! * [`IlpLine`]: the seven calls a row is built from (table, symbol, i64,
//!   f64, timestamp column, string column, designated timestamp; the string
//!   column, added 2026-10-31, is written only by the candle writer's delay
//!   text, which uses this trait too). Every row layout in the tick
//!   writer is written ONCE, generic over this trait, so the column order and
//!   separators of the library path and our path cannot drift apart: only the
//!   primitive encoders differ, and each primitive is tested against the
//!   library's own output.
//! * [`OwnIlpBuffer`]: the encoder. Output is byte-identical to
//!   `Buffer::new(ProtocolVersion::V1)` for the same calls:
//!   - names are written as given (every name passed is a `&'static`
//!     literal of this crate; a debug assertion and the identity tests check
//!     that none needs escaping, so the library's escape is a no-op on them);
//!   - symbol values are escaped exactly as the library does (`space , = \n
//!     \r \` each get a backslash);
//!   - integers are plain decimal plus `i` (the library uses `itoa`);
//!   - a price that is exactly a whole number of paise (`|value| < 1e13`) is
//!     written from that count of paise, which is the text ryu prints for it
//!     (see [`OwnIlpBuffer::column_f64`] for why); every other `f64` (NaN,
//!     infinities, `-0.0`, other values) goes through a reused library
//!     `Buffer`, so it is the library's own text by construction;
//!   - a timestamp column is microseconds (`nanos / 1000`, truncated) plus
//!     `t`, the designated timestamp nanoseconds plus a newline, a negative
//!     designated timestamp refused, as protocol version 1 does.
//! * [`TickRowBuffer`]: the tick writer's buffer, either the library's or
//!   ours, chosen once at construction by `[tick_storage] own_ilp_encoder`
//!   (default OFF).
//! * [`RawIlpSender`]: POSTs our buffer's bytes to QuestDB's `/write`
//!   endpoint the way the library's HTTP sender does with the tick writer's
//!   settings (`?precision=n`, `Content-Type: text/plain; charset=utf-8`,
//!   timeout 5 s plus one second per 100 KiB, no internal retry; the
//!   caller's one fast retry and rescue are unchanged).
//!
//! # Performance
//!
//! Every [`OwnIlpBuffer`] call is O(length of what it writes), no allocation
//! while the buffer has room (it is pre-sized like the library buffer). The
//! f64 fast path is a multiply, an add, a divide and up to 13 digit writes;
//! the fallback (any `f64` that is not a whole number of paise) costs one
//! ryu call into a reused scratch buffer, allocation-free after its first use.

use std::fmt;
use std::sync::mpsc;
use std::time::Duration;

use questdb::ingress::{Buffer, ProtocolVersion, TimestampNanos};

// ---------------------------------------------------------------------------
// The line-building calls
// ---------------------------------------------------------------------------

/// Why a row could not be written or a buffer could not be sent.
#[derive(Debug)]
pub enum TickIlpError {
    /// The library buffer or sender refused.
    Library(questdb::Error),
    /// Our encoder refused a call out of order or a value the protocol
    /// cannot carry (the same refusals the library makes).
    Encoder(&'static str),
    /// Our sender could not deliver the bytes.
    Send(RawSendError),
}

impl TickIlpError {
    /// Whether one more attempt may succeed with the same bytes: a transport
    /// failure (the library's `SocketError` class), never a refusal by the
    /// server or a malformed buffer. Mirrors
    /// `depth_persistence::flush_failure_is_retryable` for the library arm.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Library(err) => crate::depth_persistence::flush_failure_is_retryable(err),
            Self::Encoder(_) => false,
            Self::Send(err) => err.retryable,
        }
    }
}

impl fmt::Display for TickIlpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Library(err) => write!(f, "{err}"),
            Self::Encoder(why) => write!(f, "ILP encoder refused the call: {why}"),
            Self::Send(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for TickIlpError {}

impl From<questdb::Error> for TickIlpError {
    fn from(err: questdb::Error) -> Self {
        Self::Library(err)
    }
}

/// The calls one ILP row is built from. Implemented by the library `Buffer`
/// (delegating) and by [`OwnIlpBuffer`], so a row layout written once,
/// generic over this trait, produces the same line through either.
pub trait IlpLine {
    /// Starts a row for `table`.
    ///
    /// # Errors
    /// Out of order (a row is already open).
    fn table(&mut self, table: &'static str) -> Result<(), TickIlpError>;
    /// Writes one symbol. All symbols come before the first column.
    ///
    /// # Errors
    /// Out of order.
    fn symbol(&mut self, name: &'static str, value: &str) -> Result<(), TickIlpError>;
    /// Writes one integer column.
    ///
    /// # Errors
    /// Out of order.
    fn column_i64(&mut self, name: &'static str, value: i64) -> Result<(), TickIlpError>;
    /// Writes one double column.
    ///
    /// # Errors
    /// Out of order.
    fn column_f64(&mut self, name: &'static str, value: f64) -> Result<(), TickIlpError>;
    /// Writes one timestamp column given in nanoseconds.
    ///
    /// # Errors
    /// Out of order.
    fn column_ts_nanos(&mut self, name: &'static str, nanos: i64) -> Result<(), TickIlpError>;
    /// Writes one string column, quoted and escaped as the library does
    /// (`\n \r " \\` each get a backslash). Used by the candle writer's
    /// delay text; no tick layout writes one.
    ///
    /// # Errors
    /// Out of order.
    fn column_str(&mut self, name: &'static str, value: &str) -> Result<(), TickIlpError>;
    /// Ends the row with its designated timestamp in nanoseconds.
    ///
    /// # Errors
    /// Out of order, or a negative timestamp.
    fn at_nanos(&mut self, nanos: i64) -> Result<(), TickIlpError>;
}

/// A name for the library, without re-validation on every row: every name
/// passed is a literal of this crate. Debug builds run the library's own
/// validator. O(1).
#[inline]
fn library_column(name: &'static str) -> questdb::ingress::ColumnName<'static> {
    debug_assert!(
        questdb::ingress::ColumnName::new(name).is_ok(),
        "column name {name:?} is not a valid ILP column name"
    );
    questdb::ingress::ColumnName::new_unchecked(name)
}

impl IlpLine for Buffer {
    #[inline]
    fn table(&mut self, table: &'static str) -> Result<(), TickIlpError> {
        Buffer::table(self, questdb::ingress::TableName::new_unchecked(table))?;
        Ok(())
    }
    #[inline]
    fn symbol(&mut self, name: &'static str, value: &str) -> Result<(), TickIlpError> {
        Buffer::symbol(self, library_column(name), value)?;
        Ok(())
    }
    #[inline]
    fn column_i64(&mut self, name: &'static str, value: i64) -> Result<(), TickIlpError> {
        Buffer::column_i64(self, library_column(name), value)?;
        Ok(())
    }
    #[inline]
    fn column_f64(&mut self, name: &'static str, value: f64) -> Result<(), TickIlpError> {
        Buffer::column_f64(self, library_column(name), value)?;
        Ok(())
    }
    #[inline]
    fn column_ts_nanos(&mut self, name: &'static str, nanos: i64) -> Result<(), TickIlpError> {
        Buffer::column_ts(self, library_column(name), TimestampNanos::new(nanos))?;
        Ok(())
    }
    #[inline]
    fn column_str(&mut self, name: &'static str, value: &str) -> Result<(), TickIlpError> {
        Buffer::column_str(self, library_column(name), value)?;
        Ok(())
    }
    #[inline]
    fn at_nanos(&mut self, nanos: i64) -> Result<(), TickIlpError> {
        Buffer::at(self, TimestampNanos::new(nanos))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Our encoder
// ---------------------------------------------------------------------------

/// Where the current row is, the library's state machine in four states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineState {
    /// No row open: a table (or a marker) may follow.
    Idle,
    /// Table written: a symbol or a column may follow.
    Table,
    /// At least one symbol written: a symbol, a column or the end.
    Symbol,
    /// At least one column written: a column or the end.
    Column,
}

/// The library refuses a name longer than this (QuestDB's default).
const MAX_NAME_LEN: usize = 127;

/// Largest |value| written from its count of paise: below 1e13 a whole
/// number of paise has at most 15 significant digits, so its shortest
/// round-trip decimal is exactly those digits (see [`OwnIlpBuffer::column_f64`]).
const CENTS_FAST_PATH_MAX_ABS: f64 = 1e15;

/// Our ILP buffer: the library's output for the same calls, written straight
/// into one byte vector.
pub struct OwnIlpBuffer {
    bytes: Vec<u8>,
    state: LineState,
    rows: usize,
    /// `(len, state, rows)` at the marker.
    marker: Option<(usize, LineState, usize)>,
    /// Library buffer reused to print an `f64` our fast path does not cover,
    /// so that text is the library's own. Empty until first needed.
    scratch: Buffer,
}

impl fmt::Debug for OwnIlpBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnIlpBuffer")
            .field("len", &self.bytes.len())
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}

impl Default for OwnIlpBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnIlpBuffer {
    /// An empty buffer. Allocates nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            state: LineState::Idle,
            rows: 0,
            marker: None,
            scratch: Buffer::new(ProtocolVersion::V1),
        }
    }

    /// The lines written so far.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether nothing is written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Bytes the buffer holds before it must grow.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// Completed rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// Reserves room for at least `additional` more bytes.
    pub fn reserve(&mut self, additional: usize) {
        self.bytes.reserve(additional);
    }

    /// Empties the buffer and its marker, keeping the capacity.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.state = LineState::Idle;
        self.rows = 0;
        self.marker = None;
    }

    /// Marks a rewind point between rows.
    ///
    /// # Errors
    /// A row is open (the library refuses the same).
    pub fn set_marker(&mut self) -> Result<(), TickIlpError> {
        if self.state != LineState::Idle {
            return Err(TickIlpError::Encoder("marker set while a row is open"));
        }
        self.marker = Some((self.bytes.len(), self.state, self.rows));
        Ok(())
    }

    /// Undoes everything since the marker and clears it.
    ///
    /// # Errors
    /// No marker is set.
    pub fn rewind_to_marker(&mut self) -> Result<(), TickIlpError> {
        let Some((len, state, rows)) = self.marker.take() else {
            return Err(TickIlpError::Encoder("rewind with no marker set"));
        };
        self.bytes.truncate(len);
        self.state = state;
        self.rows = rows;
        Ok(())
    }

    /// Drops the marker. Idempotent.
    pub fn clear_marker(&mut self) {
        self.marker = None;
    }

    /// Whether the buffer may be sent: no row is half-written.
    #[must_use]
    pub fn can_flush(&self) -> bool {
        self.state == LineState::Idle
    }

    #[inline]
    fn push_name(&mut self, name: &'static str) {
        debug_assert!(
            !name.is_empty()
                && name.len() <= MAX_NAME_LEN
                && !name.bytes().any(must_escape_unquoted),
            "ILP name {name:?} would need escaping or validation"
        );
        self.bytes.extend_from_slice(name.as_bytes());
    }

    /// Writes `value` escaped exactly as the library's unquoted escape does.
    #[inline]
    fn push_escaped(&mut self, value: &str) {
        let raw = value.as_bytes();
        if !raw.iter().copied().any(must_escape_unquoted) {
            self.bytes.extend_from_slice(raw);
            return;
        }
        for &b in raw {
            if must_escape_unquoted(b) {
                self.bytes.push(b'\\');
            }
            self.bytes.push(b);
        }
    }

    #[inline]
    fn begin_column(&mut self, name: &'static str) -> Result<(), TickIlpError> {
        let sep = match self.state {
            LineState::Table | LineState::Symbol => b' ',
            LineState::Column => b',',
            LineState::Idle => return Err(TickIlpError::Encoder("column before table")),
        };
        self.bytes.push(sep);
        self.push_name(name);
        self.bytes.push(b'=');
        self.state = LineState::Column;
        Ok(())
    }

    /// Writes the text the library writes for `value`, no `i`.
    #[inline]
    fn push_i64(&mut self, value: i64) {
        if value < 0 {
            self.bytes.push(b'-');
        }
        push_u64_digits(&mut self.bytes, value.unsigned_abs());
    }

    /// Writes the library's text for `value`.
    ///
    /// # Why the paise path is exact
    ///
    /// Let `c` be an integer with `|c| < 1e15` and `v` the `f64` nearest to
    /// `c / 100` (what `round_to_2dp` returns, and what `c as f64 / 100.0`
    /// computes, IEEE division being correctly rounded). ryu prints the
    /// shortest decimal that rounds back to `v`. The decimal `c / 100` itself
    /// rounds back to `v` and has at most 15 significant digits, so any other
    /// decimal of at most that many digits differs from it by at least
    /// `|v| * 1e-15`, more than the half-ulp (`|v| * 1.2e-16`) it would need
    /// to round to `v`. So ryu's digits are exactly the digits of `c` with
    /// trailing zeros dropped. Its layout for `|v| < 1e16` is plain decimal:
    /// an integer gets `.0`, a fraction keeps its one or two digits, and a
    /// value below 1 starts `0.`. That is what [`write_cents`] writes. The
    /// check `c as f64 / 100.0 == v` (bit for bit) admits only such `v`;
    /// zero is admitted only as `+0.0` (`-0.0` prints `-0.0`). Pinned by the
    /// tests against the library over random paise, every price an `f32`
    /// below 2^17 can become, and random bit patterns.
    #[inline]
    fn push_f64(&mut self, value: f64) -> Result<(), TickIlpError> {
        if let Some(cents) = exact_cents(value) {
            write_cents(&mut self.bytes, cents);
            return Ok(());
        }
        self.push_f64_via_library(value)
    }

    /// Copies the library's own text for `value`: the scratch buffer writes
    /// `t v=<value>` and everything after the fixed prefix is appended.
    #[cold]
    fn push_f64_via_library(&mut self, value: f64) -> Result<(), TickIlpError> {
        self.scratch.clear();
        self.scratch
            .table(questdb::ingress::TableName::new_unchecked("t"))
            .and_then(|b| b.column_f64(questdb::ingress::ColumnName::new_unchecked("v"), value))?;
        let text = self.scratch.as_bytes();
        let Some(rest) = text.strip_prefix(SCRATCH_PREFIX) else {
            // Unreachable for these two literals; refused rather than guessed.
            return Err(TickIlpError::Encoder("library f64 text had no prefix"));
        };
        self.bytes.extend_from_slice(rest);
        self.scratch.clear();
        Ok(())
    }
}

/// What the scratch library buffer writes before the value.
const SCRATCH_PREFIX: &[u8] = b"t v=";

/// The library's unquoted-escape set.
#[inline]
const fn must_escape_unquoted(b: u8) -> bool {
    matches!(b, b' ' | b',' | b'=' | b'\n' | b'\r' | b'\\')
}

/// The library's quoted-escape set (string column values).
#[inline]
const fn must_escape_quoted(b: u8) -> bool {
    matches!(b, b'\n' | b'\r' | b'"' | b'\\')
}

/// The two-digit decimal text of 00 to 99, so a number is written two digits
/// per division.
const DIGIT_PAIRS: &[u8; 200] = b"0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";

/// Appends `n` in decimal to `out`, the text `itoa` (the library's integer
/// writer) gives.
///
/// The digits are written left-aligned into a 20-byte stack block, two per
/// division, and the whole block is appended (a fixed-size copy, no `memcpy`
/// call) and then cut back to the digit count. O(digits / 2), no allocation
/// while `out` has 20 bytes of room.
#[inline]
fn push_u64_digits(out: &mut Vec<u8>, mut n: u64) {
    // `ilog10` of a u64 is at most 19, so `len` is 1 to 20.
    let len = n.checked_ilog10().map_or(1, |l| l as usize + 1);
    let mut block = [b'0'; 20];
    let mut end = len;
    while n >= 100 {
        // `n % 100` is below 100: one pair, two bytes in the table.
        #[allow(clippy::cast_possible_truncation)]
        // APPROVED: a value below 100
        let pair = (n % 100) as usize * 2;
        n /= 100;
        end -= 2;
        block[end] = DIGIT_PAIRS[pair];
        block[end + 1] = DIGIT_PAIRS[pair + 1];
    }
    // `n` is below 100 here.
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: a value below 100
    let last = n as usize;
    if last >= 10 {
        block[0] = DIGIT_PAIRS[last * 2];
        block[1] = DIGIT_PAIRS[last * 2 + 1];
    } else {
        block[0] = DIGIT_PAIRS[last * 2 + 1];
    }
    let start = out.len();
    out.extend_from_slice(&block);
    out.truncate(start + len);
}

/// The whole number of paise `value` is exactly the nearest `f64` to, when
/// that number is below 1e15 in magnitude (and `+0.0` as 0). `None` for
/// anything else, including `-0.0`, NaN and the infinities. O(1).
#[inline]
#[must_use]
pub fn exact_cents(value: f64) -> Option<i64> {
    if value.to_bits() == 0 {
        return Some(0);
    }
    let scaled = value * 100.0;
    // Also false for NaN.
    if scaled.is_nan() || scaled.abs() >= CENTS_FAST_PATH_MAX_ABS {
        return None;
    }
    // |scaled| < 1e15, so the cast cannot saturate; a value not near an
    // integer simply fails the round trip below.
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: candidate count of paise, verified by the round trip
    let cents = (scaled + 0.5_f64.copysign(scaled)) as i64;
    if cents == 0 {
        // A nonzero value this small (or -0.0) is not a whole paise count.
        return None;
    }
    // |cents| < 1e15 < 2^53, exact in f64.
    #[allow(clippy::cast_precision_loss)]
    // APPROVED: exact, see above
    let back = cents as f64 / 100.0;
    (back.to_bits() == value.to_bits()).then_some(cents)
}

/// Writes ryu's text for the `f64` nearest to `cents / 100`.
#[inline]
fn write_cents(out: &mut Vec<u8>, cents: i64) {
    if cents < 0 {
        out.push(b'-');
    }
    let magnitude = cents.unsigned_abs();
    let whole = magnitude / 100;
    let frac = magnitude % 100;
    push_u64_digits(out, whole);
    out.push(b'.');
    // `frac` is below 100, so each narrowing is one exact digit.
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: decimal digits of a value below 100
    let (tens, ones) = ((frac / 10) as u8, (frac % 10) as u8);
    if ones == 0 {
        // Covers frac == 0 too: "x.0".
        out.push(b'0' + tens);
    } else {
        out.push(b'0' + tens);
        out.push(b'0' + ones);
    }
}

impl IlpLine for OwnIlpBuffer {
    #[inline]
    fn table(&mut self, table: &'static str) -> Result<(), TickIlpError> {
        if self.state != LineState::Idle {
            return Err(TickIlpError::Encoder("table while a row is open"));
        }
        self.push_name(table);
        self.state = LineState::Table;
        Ok(())
    }

    #[inline]
    fn symbol(&mut self, name: &'static str, value: &str) -> Result<(), TickIlpError> {
        if !matches!(self.state, LineState::Table | LineState::Symbol) {
            return Err(TickIlpError::Encoder(
                "symbol after a column or before table",
            ));
        }
        self.bytes.push(b',');
        self.push_name(name);
        self.bytes.push(b'=');
        self.push_escaped(value);
        self.state = LineState::Symbol;
        Ok(())
    }

    #[inline]
    fn column_i64(&mut self, name: &'static str, value: i64) -> Result<(), TickIlpError> {
        self.begin_column(name)?;
        self.push_i64(value);
        self.bytes.push(b'i');
        Ok(())
    }

    #[inline]
    fn column_f64(&mut self, name: &'static str, value: f64) -> Result<(), TickIlpError> {
        self.begin_column(name)?;
        self.push_f64(value)
    }

    #[inline]
    fn column_ts_nanos(&mut self, name: &'static str, nanos: i64) -> Result<(), TickIlpError> {
        self.begin_column(name)?;
        // Protocol version 1 sends microseconds, truncated toward zero.
        self.push_i64(nanos / 1000);
        self.bytes.push(b't');
        Ok(())
    }

    #[inline]
    fn column_str(&mut self, name: &'static str, value: &str) -> Result<(), TickIlpError> {
        self.begin_column(name)?;
        self.bytes.push(b'"');
        let raw = value.as_bytes();
        if raw.iter().copied().any(must_escape_quoted) {
            for &b in raw {
                if must_escape_quoted(b) {
                    self.bytes.push(b'\\');
                }
                self.bytes.push(b);
            }
        } else {
            self.bytes.extend_from_slice(raw);
        }
        self.bytes.push(b'"');
        Ok(())
    }

    #[inline]
    fn at_nanos(&mut self, nanos: i64) -> Result<(), TickIlpError> {
        if !matches!(self.state, LineState::Symbol | LineState::Column) {
            return Err(TickIlpError::Encoder(
                "designated timestamp before any field",
            ));
        }
        if nanos < 0 {
            return Err(TickIlpError::Encoder("negative designated timestamp"));
        }
        self.bytes.push(b' ');
        self.push_i64(nanos);
        self.bytes.push(b'\n');
        self.state = LineState::Idle;
        self.rows += 1;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The tick writer's buffer: library or ours
// ---------------------------------------------------------------------------

/// Which encoder a tick writer uses, chosen once at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TickIlpEncoder {
    /// The questdb-rs `Buffer` and `Sender` (default).
    #[default]
    Library,
    /// [`OwnIlpBuffer`] and [`RawIlpSender`] (`[tick_storage]
    /// own_ilp_encoder = true`).
    Own,
}

/// The tick writer's row buffer.
#[derive(Debug)]
pub enum TickRowBuffer {
    /// The library buffer, protocol version 1.
    Library(Buffer),
    /// Our encoder.
    Own(OwnIlpBuffer),
}

/// What a row buffer is: which encoder, and which protocol its bytes speak.
/// A recycled spare is installed only when its kind matches the writer's, so
/// the next batch always speaks what the sender expects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TickRowBufferKind {
    /// The library buffer of this protocol version.
    Library(ProtocolVersion),
    /// Our encoder (protocol version 1).
    Own,
}

impl TickRowBufferKind {
    /// An empty buffer of this kind. Allocates nothing.
    #[must_use]
    pub fn empty_buffer(self) -> TickRowBuffer {
        match self {
            Self::Library(protocol) => TickRowBuffer::Library(Buffer::new(protocol)),
            Self::Own => TickRowBuffer::Own(OwnIlpBuffer::new()),
        }
    }
}

impl TickRowBuffer {
    /// An empty buffer for `encoder` (protocol version 1). Allocates nothing.
    #[must_use]
    pub fn new(encoder: TickIlpEncoder) -> Self {
        match encoder {
            TickIlpEncoder::Library => TickRowBufferKind::Library(ProtocolVersion::V1),
            TickIlpEncoder::Own => TickRowBufferKind::Own,
        }
        .empty_buffer()
    }

    /// This buffer's kind.
    #[must_use]
    pub fn kind(&self) -> TickRowBufferKind {
        match self {
            Self::Library(b) => TickRowBufferKind::Library(b.protocol_version()),
            Self::Own(_) => TickRowBufferKind::Own,
        }
    }

    /// The lines written so far.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Library(b) => b.as_bytes(),
            Self::Own(b) => b.as_bytes(),
        }
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    /// Whether nothing is written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }

    /// Bytes held before the buffer must grow.
    #[must_use]
    pub fn capacity(&self) -> usize {
        match self {
            Self::Library(b) => b.capacity(),
            Self::Own(b) => b.capacity(),
        }
    }

    /// Rows completed so far.
    #[must_use]
    pub fn row_count(&self) -> usize {
        match self {
            Self::Library(b) => b.row_count(),
            Self::Own(b) => b.row_count(),
        }
    }

    /// Reserves room for `additional` more bytes.
    pub fn reserve(&mut self, additional: usize) {
        match self {
            Self::Library(b) => b.reserve(additional),
            Self::Own(b) => b.reserve(additional),
        }
    }

    /// Empties the buffer, keeping its capacity.
    pub fn clear(&mut self) {
        match self {
            Self::Library(b) => b.clear(),
            Self::Own(b) => b.clear(),
        }
    }

    /// Marks a rewind point between rows.
    ///
    /// # Errors
    /// A row is open.
    pub fn set_marker(&mut self) -> Result<(), TickIlpError> {
        match self {
            Self::Library(b) => b.set_marker().map_err(TickIlpError::Library),
            Self::Own(b) => b.set_marker(),
        }
    }

    /// Undoes everything since the marker.
    ///
    /// # Errors
    /// No marker is set.
    pub fn rewind_to_marker(&mut self) -> Result<(), TickIlpError> {
        match self {
            Self::Library(b) => b.rewind_to_marker().map_err(TickIlpError::Library),
            Self::Own(b) => b.rewind_to_marker(),
        }
    }

    /// Drops the marker.
    pub fn clear_marker(&mut self) {
        match self {
            Self::Library(b) => b.clear_marker(),
            Self::Own(b) => b.clear_marker(),
        }
    }
}

/// One predictable branch per call: the kind never changes after
/// construction.
impl IlpLine for TickRowBuffer {
    #[inline]
    fn table(&mut self, table: &'static str) -> Result<(), TickIlpError> {
        match self {
            Self::Library(b) => IlpLine::table(b, table),
            Self::Own(b) => IlpLine::table(b, table),
        }
    }
    #[inline]
    fn symbol(&mut self, name: &'static str, value: &str) -> Result<(), TickIlpError> {
        match self {
            Self::Library(b) => IlpLine::symbol(b, name, value),
            Self::Own(b) => IlpLine::symbol(b, name, value),
        }
    }
    #[inline]
    fn column_i64(&mut self, name: &'static str, value: i64) -> Result<(), TickIlpError> {
        match self {
            Self::Library(b) => IlpLine::column_i64(b, name, value),
            Self::Own(b) => IlpLine::column_i64(b, name, value),
        }
    }
    #[inline]
    fn column_f64(&mut self, name: &'static str, value: f64) -> Result<(), TickIlpError> {
        match self {
            Self::Library(b) => IlpLine::column_f64(b, name, value),
            Self::Own(b) => IlpLine::column_f64(b, name, value),
        }
    }
    #[inline]
    fn column_ts_nanos(&mut self, name: &'static str, nanos: i64) -> Result<(), TickIlpError> {
        match self {
            Self::Library(b) => IlpLine::column_ts_nanos(b, name, nanos),
            Self::Own(b) => IlpLine::column_ts_nanos(b, name, nanos),
        }
    }
    #[inline]
    fn column_str(&mut self, name: &'static str, value: &str) -> Result<(), TickIlpError> {
        match self {
            Self::Library(b) => IlpLine::column_str(b, name, value),
            Self::Own(b) => IlpLine::column_str(b, name, value),
        }
    }
    #[inline]
    fn at_nanos(&mut self, nanos: i64) -> Result<(), TickIlpError> {
        match self {
            Self::Library(b) => IlpLine::at_nanos(b, nanos),
            Self::Own(b) => IlpLine::at_nanos(b, nanos),
        }
    }
}

// ---------------------------------------------------------------------------
// The sender: library or ours
// ---------------------------------------------------------------------------

/// A send our sender could not complete.
#[derive(Debug)]
pub struct RawSendError {
    /// What failed, in the library's words where it has them.
    pub message: String,
    /// HTTP status when the server answered.
    pub status: Option<u16>,
    /// Transport failure (the library's `SocketError` class): one more
    /// attempt with the same bytes may land.
    pub retryable: bool,
}

impl fmt::Display for RawSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The library's maximum buffer size: a larger buffer is refused unsent.
pub const RAW_MAX_BUF_BYTES: usize = 100 * 1024 * 1024;

/// The tick writer's request timeout (matches `ticks_ilp_http_conf`).
pub const RAW_REQUEST_TIMEOUT: Duration = Duration::from_millis(5_000);

/// The library's default minimum throughput, which adds time to the timeout
/// for a large body: `len / 102400` seconds.
pub const RAW_REQUEST_MIN_THROUGHPUT_BYTES_PER_SEC: u64 = 102_400;

/// The time one POST of `len` bytes may take, the library's formula.
#[must_use]
pub fn raw_request_timeout(len: usize) -> Duration {
    #[allow(clippy::cast_precision_loss)]
    // APPROVED: a byte count well below 2^52
    let extra = len as f64 / RAW_REQUEST_MIN_THROUGHPUT_BYTES_PER_SEC as f64;
    RAW_REQUEST_TIMEOUT + Duration::from_secs_f64(extra)
}

/// One POST handed to the sender thread: the bytes, and where to put the
/// outcome and the bytes back.
type RawJob = (
    Vec<u8>,
    mpsc::SyncSender<(Vec<u8>, Result<(), RawSendError>)>,
);

/// POSTs raw ILP to QuestDB's `/write` endpoint from a thread of its own.
///
/// The thread owns a current-thread tokio runtime and a `reqwest` client,
/// so [`RawIlpSender::send`] works from any caller (a plain OS thread or a
/// tokio worker inside `off_worker`) without nesting runtimes. The bytes
/// travel to the thread by move and come back by move, so a failed send still
/// holds them for the rescue tier.
///
/// # Where it deliberately differs from the library
///
/// * No proxy: the library's HTTP client reads `HTTP_PROXY` from the
///   environment; ours never does, because QuestDB is always reached directly.
/// * No redirects: QuestDB never sends one, and following one would re-send
///   tick rows to another host.
/// * An error body is read up to 64 KiB (the library reads up to 10 MB).
///
/// # Honest limit
///
/// If the sender thread died while holding a batch, that batch's bytes are
/// gone and the rescue would spill an empty payload. Release builds abort on
/// any panic, so this is reachable only in debug and test builds.
#[derive(Debug)]
pub struct RawIlpSender {
    jobs: mpsc::SyncSender<RawJob>,
    url: String,
}

impl RawIlpSender {
    /// Starts the sender thread for `http://host:port/write`.
    ///
    /// # Errors
    /// The runtime, the client or the thread could not be built.
    pub fn new(host: &str, http_port: u16) -> Result<Self, String> {
        // APPROVED: the URL, once per writer
        let url = format!("http://{host}:{http_port}/write");
        let (jobs, rx) = mpsc::sync_channel::<RawJob>(1);
        // QuestDB is always reached directly: no proxy from the environment.
        let client = reqwest::Client::builder()
            .no_proxy()
            // QuestDB never redirects; a redirect would re-send tick rows to
            // another host (security review, 2026-10-30).
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| format!("ILP HTTP client: {err}"))?;
        let thread_url = url.clone(); // APPROVED: once per writer
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("ILP HTTP runtime: {err}"))?;
        std::thread::Builder::new()
            .name("tv-tick-ilp-http".to_owned())
            .spawn(move || {
                while let Ok((body, reply)) = rx.recv() {
                    let outcome = runtime.block_on(post_once(&client, &thread_url, body));
                    // The caller waits for this; gone means it shut down.
                    let _receiver_gone = reply.send(outcome);
                }
            })
            .map_err(|err| format!("ILP HTTP thread: {err}"))?;
        Ok(Self { jobs, url })
    }

    /// The `/write` URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Sends `buffer`'s bytes. On success the buffer is cleared (as the
    /// library's `flush` does); on failure it holds the same bytes.
    ///
    /// # Errors
    /// A half-written row, a buffer above [`RAW_MAX_BUF_BYTES`], a transport
    /// failure or a refusal by the server.
    pub fn send(&self, buffer: &mut OwnIlpBuffer) -> Result<(), RawSendError> {
        if !buffer.can_flush() {
            return Err(RawSendError {
                message: "Could not flush buffer: a row is half-written".to_owned(),
                status: None,
                retryable: false,
            });
        }
        if buffer.len() > RAW_MAX_BUF_BYTES {
            return Err(RawSendError {
                // APPROVED: refusal path only
                message: format!(
                    "Could not flush buffer: Buffer size of {} exceeds maximum configured \
                     allowed size of {RAW_MAX_BUF_BYTES} bytes.",
                    buffer.len()
                ),
                status: None,
                retryable: false,
            });
        }
        if buffer.is_empty() {
            return Ok(());
        }
        let body = std::mem::take(&mut buffer.bytes);
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if let Err(mpsc::SendError((body, _))) = self.jobs.send((body, reply_tx)) {
            buffer.bytes = body;
            return Err(gone(&self.url));
        }
        match reply_rx.recv() {
            Ok((body, Ok(()))) => {
                buffer.bytes = body;
                buffer.clear();
                Ok(())
            }
            Ok((body, Err(err))) => {
                buffer.bytes = body;
                Err(err)
            }
            // The thread died with the bytes: they are lost from this buffer,
            // so the caller's rescue sees an empty payload. Unreachable
            // unless the thread panicked.
            Err(_) => Err(gone(&self.url)),
        }
    }
}

fn gone(url: &str) -> RawSendError {
    RawSendError {
        // APPROVED: failure path only
        message: format!("Could not flush buffer: {url}: the ILP HTTP thread is gone"),
        status: None,
        retryable: false,
    }
}

/// One POST, the library's request with the tick writer's settings. Returns
/// the bytes whatever the outcome.
async fn post_once(
    client: &reqwest::Client,
    url: &str,
    body: Vec<u8>,
) -> (Vec<u8>, Result<(), RawSendError>) {
    let timeout = raw_request_timeout(body.len());
    let payload = bytes::Bytes::from(body);
    let sent = client
        .post(url)
        .query(&[("precision", "n")])
        .header(reqwest::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .timeout(timeout)
        .body(payload.clone())
        .send()
        .await;
    let outcome = match sent {
        Err(err) => Err(RawSendError {
            // APPROVED: failure path only
            message: format!(
                "Could not flush buffer: {url}: {}",
                crate::tick_spill_replay::describe_send_error(&err)
            ),
            status: None,
            retryable: true,
        }),
        Ok(response) => classify_response(response).await,
    };
    // The request is done and dropped, so this is the only handle: no copy.
    (Vec::from(payload), outcome)
}

/// Bytes of an error answer's body read into the message. The library reads
/// up to 10 MB; a QuestDB error body is a short JSON object, so the rest of an
/// oversized body is dropped rather than held and logged (security review,
/// 2026-10-30).
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Reads at most [`MAX_ERROR_BODY_BYTES`] of the body, invalid UTF-8 replaced
/// (the library reads it lossily too).
async fn read_error_body(mut response: reqwest::Response) -> Result<String, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
        body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
        if body.len() >= MAX_ERROR_BODY_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// The library's text for a JSON error body (`parse_json_error`): the
/// message, then `[id: …, code: …, line: …]` for whichever are present.
fn json_error_description(json: &serde_json::Value, message: &str) -> String {
    let mut description = message.to_owned();
    let error_id = json.get("errorId").and_then(serde_json::Value::as_str);
    let code = json.get("code").and_then(serde_json::Value::as_str);
    let line = json.get("line").and_then(serde_json::Value::as_i64);
    if error_id.is_none() && code.is_none() && line.is_none() {
        return description;
    }
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if let Some(id) = error_id {
        parts.push(format!("id: {id}")); // APPROVED: failure path only
    }
    if let Some(code) = code {
        parts.push(format!("code: {code}")); // APPROVED: failure path only
    }
    if let Some(line) = line {
        parts.push(format!("line: {line}")); // APPROVED: failure path only
    }
    description.push_str(" [");
    description.push_str(&parts.join(", "));
    description.push(']');
    description
}

/// The library's reading of an HTTP answer (`parse_http_error`): 404 is
/// "does not support ILP", 401/403 an authentication error, any other error
/// status the server's message (a JSON body's `message` plus its id, code and
/// line, as the library prints it). None is retried; a body that cannot be
/// read is the transport class, as in the library.
async fn classify_response(response: reqwest::Response) -> Result<(), RawSendError> {
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        // Drain the body so the connection can be reused.
        let _drained = response.bytes().await;
        return Ok(());
    }
    let code = status.as_u16();
    if code == 404 {
        return Err(RawSendError {
            message: "Could not flush buffer: HTTP endpoint does not support ILP.".to_owned(),
            status: Some(code),
            retryable: false,
        });
    }
    let is_json = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.eq_ignore_ascii_case("application/json"));
    let body = read_error_body(response).await;
    if [401, 403].contains(&code) {
        let detail = match &body {
            Ok(text) if !text.is_empty() => format!(": {text}"), // APPROVED: failure path only
            _ => String::new(),
        };
        return Err(RawSendError {
            // APPROVED: failure path only
            message: format!(
                "Could not flush buffer: HTTP endpoint authentication error{detail} [code: {code}]"
            ),
            status: Some(code),
            retryable: false,
        });
    }
    let body = match body {
        Ok(text) => text,
        Err(err) => {
            return Err(RawSendError {
                // APPROVED: failure path only
                message: format!("Could not flush buffer: {err}"),
                status: Some(code),
                retryable: true,
            });
        }
    };
    let json = if is_json {
        serde_json::from_str::<serde_json::Value>(&body).ok()
    } else {
        None
    };
    let message = match json.as_ref().and_then(|j| {
        j.get("message")
            .and_then(serde_json::Value::as_str)
            .map(|m| (j, m))
    }) {
        Some((json, message)) => json_error_description(json, message),
        None => body,
    };
    Err(RawSendError {
        // APPROVED: failure path only
        message: format!("Could not flush buffer: {message}"),
        status: Some(code),
        retryable: false,
    })
}

/// The tick writer's sender: the library's, or ours.
#[derive(Debug)]
pub enum TickIlpSender {
    /// The questdb-rs HTTP sender.
    Library(questdb::ingress::Sender),
    /// [`RawIlpSender`].
    Own(RawIlpSender),
}

impl TickIlpSender {
    /// Sends `buffer` and clears it on success; on failure the buffer keeps
    /// its bytes for the rescue tier.
    ///
    /// # Errors
    /// The send failed, or the buffer is of the other encoder's kind (a
    /// wiring bug: never mixed in production, refused rather than guessed).
    pub fn flush(&mut self, buffer: &mut TickRowBuffer) -> Result<(), TickIlpError> {
        match (self, buffer) {
            (Self::Library(sender), TickRowBuffer::Library(b)) => {
                sender.flush(b).map_err(TickIlpError::Library)
            }
            (Self::Own(sender), TickRowBuffer::Own(b)) => {
                sender.send(b).map_err(TickIlpError::Send)
            }
            _ => Err(TickIlpError::Encoder(
                "buffer and sender use different encoders",
            )),
        }
    }
}

#[cfg(test)]
#[path = "tick_ilp_tests.rs"]
mod tick_ilp_tests;
