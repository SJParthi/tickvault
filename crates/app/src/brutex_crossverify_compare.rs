//! BruteX↔TickVault daily cross-verify — PURE comparison core (Unit 4).
//!
//! NO I/O lives here: no tokio, no S3/HTTP, no QuestDB. The boot/runner shell
//! (`brutex_crossverify_boot.rs`, Unit 5) fetches the BruteX CSVs, reads the
//! live `candles_1m` rows (`feed='groww'`), and feeds this module three pure
//! functions:
//!
//! 1. [`parse_brutex_csv`] — hostile-input-safe CSV → paise-integer minute
//!    rows (fail-soft per row, typed error only for a missing/invalid header).
//! 2. [`GrowwSymbolMap`] — BruteX symbol → `(security_id, exchange_segment)`
//!    resolution built from `instrument_lifecycle` rows (EQUITY / INDEX /
//!    FUTIDX; index names canonicalized via the shared
//!    `canonicalize_index_symbol`).
//! 3. [`compare_day`] — minute-by-minute paise-exact (inclusive-tolerance)
//!    OHLC comparison producing per-cell findings keyed to the persistence
//!    enums in `tickvault_storage::brutex_crossverify_persistence`.
//!
//! Prices compare as integer paise (`(rupees * 100).round()`), matching the
//! BRUTEX-XVERIFY-01 contract (`.claude/rules/project/
//! brutex-crossverify-error-codes.md`). Timestamps are IST minute-bucket
//! nanos — the same representation `cross_verify_1m_boot.rs` uses (IST
//! wall-clock encoded as epoch seconds × 1e9, NO UTC offset arithmetic).

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDateTime;
use tickvault_core::instrument::index_extractor::canonicalize_index_symbol;
use tickvault_storage::brutex_crossverify_persistence::{
    BRUTEX_CROSSVERIFY_MISSING_SENTINEL, BrutexCrossverifyCellKind, BrutexCrossverifyDailyOutcome,
    BrutexCrossverifyField,
};

// ---------------------------------------------------------------------------
// Constants (session window + parse bounds)
// ---------------------------------------------------------------------------

/// Nanoseconds per second (i64 domain used by the IST minute-bucket keys).
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Seconds per minute.
const SECS_PER_MINUTE: i64 = 60;

/// Session open, seconds-of-day IST (09:15:00).
const SESSION_OPEN_SECS_OF_DAY: i64 = 9 * 3600 + 15 * 60;

/// Full-classification window end, seconds-of-day IST (15:28:00) — the
/// 15:28/15:29 minutes are the close-seal tail (`tail_unsealed` when the
/// live side has not sealed them yet).
const FULL_CLASSIFY_END_SECS_OF_DAY: i64 = 15 * 3600 + 28 * 60;

/// Session close, seconds-of-day IST (15:30:00, exclusive).
const SESSION_CLOSE_SECS_OF_DAY: i64 = 15 * 3600 + 30 * 60;

/// Maximum accepted price in paise (values above are junk / corrupt rows).
const MAX_PRICE_PAISE: i64 = 1_000_000_000_000;

/// Maximum accepted BruteX symbol length after trim (defensive bound).
const MAX_SYMBOL_CHARS: usize = 64;

/// How many bad-row sample strings are retained (each truncated to
/// [`BAD_ROW_SAMPLE_MAX_CHARS`]).
const MAX_BAD_ROW_SAMPLES: usize = 5;

/// Per-sample truncation bound for retained bad-row text.
const BAD_ROW_SAMPLE_MAX_CHARS: usize = 80;

/// Alignment-hint probe: max symbols re-scored with shifted minutes.
const ALIGNMENT_PROBE_MAX_SYMBOLS: usize = 20;

/// Alignment-hint trigger: (diverged + missing) must exceed this fraction of
/// the comparison universe before the ±60s probe runs.
const ALIGNMENT_TRIGGER_FRACTION: f64 = 0.5;

/// Alignment-hint verdict: a shifted match-rate must beat baseline by more
/// than this many percentage points to produce a hint.
const ALIGNMENT_IMPROVEMENT_POINTS: f64 = 30.0;

// ---------------------------------------------------------------------------
// Part 1 — hostile CSV parsing
// ---------------------------------------------------------------------------

/// Typed header-level failure for [`parse_brutex_csv`]. Row-level problems
/// are FAIL-SOFT (counted + sampled inside [`ParsedCsv`]); only a body that
/// cannot yield ANY rows (bad encoding / missing header columns) errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrutexCsvError {
    /// The object body was empty (or whitespace-only).
    Empty,
    /// The object body was not valid UTF-8.
    NotUtf8,
    /// The header line was present but a required column was missing.
    /// Carries the missing column name.
    MissingHeaderColumn(String),
}

impl std::fmt::Display for BrutexCsvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty csv body"),
            Self::NotUtf8 => write!(f, "csv body is not valid utf-8"),
            Self::MissingHeaderColumn(col) => {
                write!(f, "csv header missing required column '{col}'")
            }
        }
    }
}

impl std::error::Error for BrutexCsvError {}

/// One accepted BruteX 1-minute row (prices already integer paise, timestamp
/// already floored to the IST minute bucket).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrutexRow {
    /// Trimmed, uppercased BruteX symbol (≤ [`MAX_SYMBOL_CHARS`] chars).
    pub symbol: String,
    /// IST minute-bucket nanos (IST wall-clock epoch seconds × 1e9, floored
    /// to the minute — the `candles_1m` `ts` representation).
    pub minute_nanos: i64,
    /// Open price in integer paise.
    pub open_paise: i64,
    /// High price in integer paise.
    pub high_paise: i64,
    /// Low price in integer paise.
    pub low_paise: i64,
    /// Close price in integer paise.
    pub close_paise: i64,
    /// Minute volume (≥ 0).
    pub volume: i64,
    /// Open interest (≥ 0; 0 for non-derivatives).
    pub oi: i64,
}

/// Fail-soft parse output: accepted rows + honest counters for everything
/// that was refused (audit Rule 11 — a partial parse is never silent).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedCsv {
    /// Accepted rows, duplicate `(symbol, minute)` collapsed LAST-wins.
    pub rows: Vec<BrutexRow>,
    /// Total non-empty data lines seen (excludes the header + blank lines).
    pub total_data_lines: usize,
    /// Data lines refused (bad field count / junk numerics / bad timestamp /
    /// bad symbol).
    pub bad_rows: usize,
    /// First ≤5 refused lines, each truncated to ≤80 chars (char-safe).
    pub bad_row_samples: Vec<String>,
    /// Duplicate `(symbol, minute)` keys collapsed (last occurrence wins).
    pub duplicate_minutes: usize,
    /// True when parsing stopped at `max_rows` accepted rows.
    pub truncated: bool,
}

/// Truncate `line` to at most `max_chars` characters (UTF-8 char-boundary
/// safe — never slices mid-codepoint).
fn truncate_chars(line: &str, max_chars: usize) -> String {
    line.chars().take(max_chars).collect()
}

/// True when every char of the trimmed uppercase symbol is in the allowed
/// charset: ASCII alphanumeric, space, `.`, `-`, `_` — ALIGNED (FIX ROUND 2)
/// with the API's `is_valid_symbol_param` (`[A-Za-z0-9 ._-]`, the §37.3
/// contract). `&` and `/` were removed: rows admitted here but rejected by
/// the API produced dead drill-down links (400). Strictest charset wins —
/// M&M-style NSE symbols become counted bad rows until a future contract
/// amendment widens BOTH sides in lockstep.
fn symbol_charset_ok(symbol: &str) -> bool {
    symbol
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_'))
}

/// Parse a BruteX price field to integer paise. Rejects non-finite, ≤ 0,
/// and > [`MAX_PRICE_PAISE`] values.
fn parse_price_paise(field: &str) -> Option<i64> {
    let value: f64 = field.trim().parse().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let paise = (value * 100.0).round();
    // Guard the f64→i64 cast: beyond the cap the row is junk anyway.
    if paise > MAX_PRICE_PAISE as f64 {
        return None;
    }
    Some(paise as i64)
}

/// Parse a non-negative integer field (volume / oi). Junk → `None`.
fn parse_non_negative_i64(field: &str) -> Option<i64> {
    let value: i64 = field.trim().parse().ok()?;
    if value < 0 { None } else { Some(value) }
}

/// Parse a BruteX timestamp ("YYYY-MM-DD HH:MM:SS" or the `T`-separated
/// form) as IST wall-clock and return the IST minute-bucket nanos.
fn parse_ist_minute_nanos(field: &str) -> Option<i64> {
    let trimmed = field.trim();
    let naive = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    // The wall-clock IS IST; encode it as epoch seconds directly (the
    // repo-wide "IST epoch" convention — never add/subtract 19800 here).
    let secs = naive.and_utc().timestamp();
    if secs <= 0 {
        return None;
    }
    let minute_secs = secs - secs.rem_euclid(SECS_PER_MINUTE);
    minute_secs.checked_mul(NANOS_PER_SEC)
}

/// Column indexes resolved from the (case-insensitive, order-agnostic)
/// header line.
struct HeaderIndexes {
    symbol: usize,
    timestamp_ist: usize,
    open: usize,
    high: usize,
    low: usize,
    close: usize,
    volume: usize,
    oi: usize,
}

/// Resolve the required column indexes from the header line.
fn resolve_header(header_line: &str) -> Result<HeaderIndexes, BrutexCsvError> {
    let cols: Vec<String> = header_line
        .split(',')
        .map(|c| c.trim().to_ascii_lowercase())
        .collect();
    let find = |name: &str| -> Result<usize, BrutexCsvError> {
        cols.iter()
            .position(|c| c == name)
            .ok_or_else(|| BrutexCsvError::MissingHeaderColumn(name.to_owned()))
    };
    Ok(HeaderIndexes {
        symbol: find("symbol")?,
        timestamp_ist: find("timestamp_ist")?,
        open: find("open")?,
        high: find("high")?,
        low: find("low")?,
        close: find("close")?,
        volume: find("volume")?,
        oi: find("oi")?,
    })
}

/// Parse one BruteX CSV object body (BOM-stripped, CRLF/LF-normalized) into
/// paise-integer minute rows.
///
/// Header `symbol,timestamp_ist,open,high,low,close,volume,oi` is required
/// (case-insensitive, trimmed, order-agnostic) — otherwise a typed
/// [`BrutexCsvError`]. Every DATA row failure is fail-soft: counted in
/// `bad_rows` with the first ≤5 offending lines sampled (truncated to 80
/// chars). Duplicate `(symbol, minute)` keys collapse LAST-wins. Parsing
/// stops once `max_rows` rows are ACCEPTED (`truncated = true`).
// TEST-EXEMPT: covered by the `csv_*` suite below (csv_happy_path_three_rows_with_volume_and_oi, csv_missing_header_is_typed_error, csv_seeded_fuzz_never_panics, ...).
pub fn parse_brutex_csv(bytes: &[u8], max_rows: usize) -> Result<ParsedCsv, BrutexCsvError> {
    // BOM strip.
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let text = std::str::from_utf8(body).map_err(|_| BrutexCsvError::NotUtf8)?;
    if text.trim().is_empty() {
        return Err(BrutexCsvError::Empty);
    }

    // CRLF / lone-CR normalization is handled by splitting on '\n' and
    // trimming a trailing '\r' per line (mixed endings tolerated).
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));

    // First non-blank line is the header.
    let header_line = lines
        .by_ref()
        .find(|l| !l.trim().is_empty())
        .ok_or(BrutexCsvError::Empty)?;
    let idx = resolve_header(header_line)?;
    let max_col = [
        idx.symbol,
        idx.timestamp_ist,
        idx.open,
        idx.high,
        idx.low,
        idx.close,
        idx.volume,
        idx.oi,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);

    let mut out = ParsedCsv::default();
    // Last-wins dedup: key -> index into `rows`.
    let mut seen: HashMap<(String, i64), usize> = HashMap::new();

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if out.rows.len() >= max_rows {
            out.truncated = true;
            break;
        }
        out.total_data_lines += 1;

        let fields: Vec<&str> = line.split(',').collect();
        let parsed = (|| -> Option<BrutexRow> {
            if fields.len() <= max_col {
                return None;
            }
            let symbol = fields.get(idx.symbol)?.trim().to_ascii_uppercase();
            if symbol.is_empty()
                || symbol.chars().count() > MAX_SYMBOL_CHARS
                || !symbol_charset_ok(&symbol)
            {
                return None;
            }
            let minute_nanos = parse_ist_minute_nanos(fields.get(idx.timestamp_ist)?)?;
            let open_paise = parse_price_paise(fields.get(idx.open)?)?;
            let high_paise = parse_price_paise(fields.get(idx.high)?)?;
            let low_paise = parse_price_paise(fields.get(idx.low)?)?;
            let close_paise = parse_price_paise(fields.get(idx.close)?)?;
            let volume = parse_non_negative_i64(fields.get(idx.volume)?)?;
            let oi = parse_non_negative_i64(fields.get(idx.oi)?)?;
            Some(BrutexRow {
                symbol,
                minute_nanos,
                open_paise,
                high_paise,
                low_paise,
                close_paise,
                volume,
                oi,
            })
        })();

        match parsed {
            Some(row) => {
                let key = (row.symbol.clone(), row.minute_nanos);
                if let Some(&existing) = seen.get(&key) {
                    out.duplicate_minutes += 1;
                    // LAST wins.
                    if let Some(slot) = out.rows.get_mut(existing) {
                        *slot = row;
                    }
                } else {
                    seen.insert(key, out.rows.len());
                    out.rows.push(row);
                }
            }
            None => {
                out.bad_rows += 1;
                if out.bad_row_samples.len() < MAX_BAD_ROW_SAMPLES {
                    out.bad_row_samples
                        .push(truncate_chars(line, BAD_ROW_SAMPLE_MAX_CHARS));
                }
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Part 2 — symbol mapping (BruteX symbol → resolved instrument)
// ---------------------------------------------------------------------------

/// One `instrument_lifecycle` row projection consumed by
/// [`GrowwSymbolMap::from_lifecycle_rows`] (Unit 5 reads QuestDB and builds
/// these; this module never queries anything).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleRow {
    /// Instrument security id.
    pub security_id: i64,
    /// Exchange segment wire label (`NSE_EQ` / `BSE_EQ` / `IDX_I` /
    /// `NSE_FNO` / `BSE_FNO` …).
    pub exchange_segment: String,
    /// Instrument type wire label (`EQUITY` / `INDEX` / `FUTIDX` …).
    pub instrument_type: String,
    /// Tradable symbol name from the master.
    pub symbol_name: String,
    /// Underlying symbol (derivatives only; empty for spot).
    pub underlying_symbol: String,
    /// Derivative expiry as `YYYY-MM-DD` (None for spot/index).
    pub expiry_ymd: Option<String>,
}

/// Resolution verdict for one raw BruteX symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Uniquely resolved to one live instrument identity.
    Resolved {
        /// The resolved security id.
        security_id: i64,
        /// The resolved exchange segment wire label.
        segment: String,
    },
    /// No live instrument matched.
    Unmapped,
    /// More than one DISTINCT live instrument matched (fail-closed — never
    /// guess between candidates).
    Ambiguous,
}

/// Internal map slot: a unique hit or a poisoned (ambiguous) key.
#[derive(Clone, Debug)]
enum MapSlot {
    Unique { security_id: i64, segment: String },
    Ambiguous,
}

/// BruteX symbol → live instrument resolver built once per run from the
/// `instrument_lifecycle` master rows. All lookups are O(1) hash probes.
#[derive(Debug, Default)]
pub struct GrowwSymbolMap {
    /// UPPER(symbol_name) → equity identity (NSE_EQ preferred over BSE_EQ).
    equities: HashMap<String, MapSlot>,
    /// canonicalize_index_symbol(symbol_name) → index identity.
    indices: HashMap<String, MapSlot>,
    /// (canonical underlying, expiry `YYYY-MM-DD`) → futures identity.
    futures: HashMap<(String, String), MapSlot>,
    /// Count of keys that became ambiguous during the build.
    ambiguous_keys: usize,
}

/// True when `segment` should replace `existing_segment` for a duplicate
/// EQUITY key: NSE_EQ is preferred over BSE_EQ.
fn equity_prefers(new_segment: &str, existing_segment: &str) -> bool {
    new_segment == "NSE_EQ" && existing_segment == "BSE_EQ"
}

impl GrowwSymbolMap {
    /// Build the resolver from lifecycle master rows (EQUITY, INDEX, FUTIDX
    /// rows consumed; everything else ignored). Duplicate EQUITY symbols
    /// prefer NSE_EQ over BSE_EQ; any OTHER duplicate identity poisons the
    /// key as ambiguous (fail-closed) and is counted.
    #[must_use]
    // TEST-EXEMPT: covered by the `map_*` tests below (same-module suite).
    pub fn from_lifecycle_rows(rows: Vec<LifecycleRow>) -> Self {
        let mut map = Self::default();
        for row in rows {
            match row.instrument_type.as_str() {
                "EQUITY" => {
                    let key = row.symbol_name.trim().to_ascii_uppercase();
                    if key.is_empty() {
                        continue;
                    }
                    map.insert_equity(key, row.security_id, row.exchange_segment);
                }
                "INDEX" => {
                    let key = canonicalize_index_symbol(&row.symbol_name);
                    if key.is_empty() {
                        continue;
                    }
                    Self::insert_slot(
                        &mut map.indices,
                        &mut map.ambiguous_keys,
                        key,
                        row.security_id,
                        row.exchange_segment,
                    );
                }
                "FUTIDX" => {
                    let Some(expiry) = row.expiry_ymd else {
                        continue;
                    };
                    let underlying = canonicalize_index_symbol(&row.underlying_symbol);
                    if underlying.is_empty() || expiry.trim().is_empty() {
                        continue;
                    }
                    Self::insert_slot(
                        &mut map.futures,
                        &mut map.ambiguous_keys,
                        (underlying, expiry.trim().to_owned()),
                        row.security_id,
                        row.exchange_segment,
                    );
                }
                _ => {}
            }
        }
        map
    }

    /// Number of keys poisoned as ambiguous during the build.
    // WIRING-EXEMPT: consumed by Unit 5 runner (same PR)
    #[must_use]
    // TEST-EXEMPT: covered by map_garbage_is_unmapped_and_duplicates_are_ambiguous (same-module suite).
    pub fn ambiguous_key_count(&self) -> usize {
        self.ambiguous_keys
    }

    fn insert_equity(&mut self, key: String, security_id: i64, segment: String) {
        match self.equities.get_mut(&key) {
            None => {
                self.equities.insert(
                    key,
                    MapSlot::Unique {
                        security_id,
                        segment,
                    },
                );
            }
            Some(MapSlot::Ambiguous) => {}
            Some(slot @ MapSlot::Unique { .. }) => {
                let MapSlot::Unique {
                    security_id: existing_sid,
                    segment: existing_segment,
                } = &*slot
                else {
                    return;
                };
                if *existing_sid == security_id && *existing_segment == segment {
                    // Exact duplicate row — first wins, no ambiguity.
                    return;
                }
                if equity_prefers(&segment, existing_segment) {
                    *slot = MapSlot::Unique {
                        security_id,
                        segment,
                    };
                } else if equity_prefers(existing_segment, &segment) {
                    // Existing NSE_EQ beats the new BSE_EQ — keep it.
                } else {
                    *slot = MapSlot::Ambiguous;
                    self.ambiguous_keys += 1;
                }
            }
        }
    }

    fn insert_slot<K: std::hash::Hash + Eq>(
        map: &mut HashMap<K, MapSlot>,
        ambiguous_keys: &mut usize,
        key: K,
        security_id: i64,
        segment: String,
    ) {
        match map.get_mut(&key) {
            None => {
                map.insert(
                    key,
                    MapSlot::Unique {
                        security_id,
                        segment,
                    },
                );
            }
            Some(MapSlot::Ambiguous) => {}
            Some(slot @ MapSlot::Unique { .. }) => {
                let MapSlot::Unique {
                    security_id: existing_sid,
                    segment: existing_segment,
                } = &*slot
                else {
                    return;
                };
                if *existing_sid == security_id && *existing_segment == segment {
                    return;
                }
                *slot = MapSlot::Ambiguous;
                *ambiguous_keys += 1;
            }
        }
    }

    /// Resolve one raw BruteX symbol. Order: futures forms first (they carry
    /// an unambiguous shape), then exact EQUITY, then canonical INDEX (with
    /// an optional `NSE-`/`BSE-` prefix stripped).
    // TEST-EXEMPT-NOT-NEEDED: covered by the `map_*` tests below.
    #[must_use]
    pub fn resolve(&self, raw_symbol: &str) -> Resolution {
        let trimmed = raw_symbol.trim().to_ascii_uppercase();
        if trimmed.is_empty() {
            return Resolution::Unmapped;
        }

        if let Some((underlying, expiry)) = parse_futures_symbol(&trimmed) {
            let canonical = canonicalize_index_symbol(&underlying);
            return match self.futures.get(&(canonical, expiry)) {
                Some(MapSlot::Unique {
                    security_id,
                    segment,
                }) => Resolution::Resolved {
                    security_id: *security_id,
                    segment: segment.clone(),
                },
                Some(MapSlot::Ambiguous) => Resolution::Ambiguous,
                None => Resolution::Unmapped,
            };
        }

        if let Some(slot) = self.equities.get(&trimmed) {
            return match slot {
                MapSlot::Unique {
                    security_id,
                    segment,
                } => Resolution::Resolved {
                    security_id: *security_id,
                    segment: segment.clone(),
                },
                MapSlot::Ambiguous => Resolution::Ambiguous,
            };
        }

        // Index lookup — strip an optional exchange prefix ("NSE-NIFTY").
        let index_raw = trimmed
            .strip_prefix("NSE-")
            .or_else(|| trimmed.strip_prefix("BSE-"))
            .unwrap_or(&trimmed);
        let canonical = canonicalize_index_symbol(index_raw);
        match self.indices.get(&canonical) {
            Some(MapSlot::Unique {
                security_id,
                segment,
            }) => Resolution::Resolved {
                security_id: *security_id,
                segment: segment.clone(),
            },
            Some(MapSlot::Ambiguous) => Resolution::Ambiguous,
            None => Resolution::Unmapped,
        }
    }
}

/// Parse the two accepted BruteX futures symbol forms into
/// `(underlying, expiry YYYY-MM-DD)`:
///
/// * `"<UND>-<YYYY-MM-DD>-FUT"` (e.g. `NIFTY-2026-07-30-FUT`)
/// * `"NSE-<UND>-<DDMonYY>-FUT"` / `"BSE-<UND>-<DDMonYY>-FUT"`
///   (e.g. `NSE-NIFTY-30Jul26-FUT`, month abbreviation case-insensitive)
///
/// Input is expected UPPERCASED + trimmed. Returns `None` for anything else.
fn parse_futures_symbol(symbol: &str) -> Option<(String, String)> {
    let stem = symbol.strip_suffix("-FUT")?;

    // Form A: <UND>-YYYY-MM-DD — the last 10 chars are the ISO date and the
    // char before them is '-'.
    if stem.len() > 11 {
        let (head, date) = stem.split_at(stem.len() - 10);
        if let Some(underlying) = head.strip_suffix('-')
            && parse_iso_date_ymd(date).is_some()
            && !underlying.is_empty()
        {
            return Some((underlying.to_owned(), date.to_owned()));
        }
    }

    // Form B: NSE-<UND>-<DDMonYY> / BSE-<UND>-<DDMonYY>.
    let rest = stem
        .strip_prefix("NSE-")
        .or_else(|| stem.strip_prefix("BSE-"))?;
    let (underlying, dmy) = rest.rsplit_once('-')?;
    if underlying.is_empty() {
        return None;
    }
    let expiry = parse_dd_mon_yy(dmy)?;
    Some((underlying.to_owned(), expiry))
}

/// Validate an ISO `YYYY-MM-DD` date string (calendar-checked via chrono).
fn parse_iso_date_ymd(s: &str) -> Option<()> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .map(|_| ())
}

/// Parse `DDMonYY` (e.g. `30JUL26` — the input is already uppercased) into
/// `YYYY-MM-DD`. Two-digit years map to 20YY.
fn parse_dd_mon_yy(s: &str) -> Option<String> {
    // DD is 1 or 2 digits, Mon is 3 letters, YY is 2 digits.
    let n = s.len();
    if !(6..=7).contains(&n) {
        return None;
    }
    let day_len = n - 5;
    let (day_str, rest) = s.split_at(day_len);
    let (mon_str, yy_str) = rest.split_at(3);
    let day: u32 = day_str.parse().ok()?;
    let yy: i32 = yy_str.parse().ok()?;
    let month: u32 = match mon_str {
        "JAN" => 1,
        "FEB" => 2,
        "MAR" => 3,
        "APR" => 4,
        "MAY" => 5,
        "JUN" => 6,
        "JUL" => 7,
        "AUG" => 8,
        "SEP" => 9,
        "OCT" => 10,
        "NOV" => 11,
        "DEC" => 12,
        _ => return None,
    };
    let date = chrono::NaiveDate::from_ymd_opt(2000 + yy, month, day)?;
    Some(date.format("%Y-%m-%d").to_string())
}

// ---------------------------------------------------------------------------
// Part 3 — day comparison
// ---------------------------------------------------------------------------

/// Comparison configuration (mirrors `BrutexCrossverifyConfig` knobs; kept
/// as a tiny pure struct so the core stays config-crate-agnostic in tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct CompareCfg {
    /// Inclusive per-field price tolerance in paise
    /// (`|live − brutex| <= tolerance_paise` → matched). Default 0 = exact.
    pub tolerance_paise: i64,
    /// Whether volume is CLASSIFIED (default false — Groww live volume is
    /// always 0 on the LTP-only feed; both sides are always CAPTURED).
    pub compare_volume: bool,
}

/// One minute bar on either side of the comparison (prices integer paise).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MinuteBar {
    /// Open price, paise.
    pub open_paise: i64,
    /// High price, paise.
    pub high_paise: i64,
    /// Low price, paise.
    pub low_paise: i64,
    /// Close price, paise.
    pub close_paise: i64,
    /// Minute volume (live side is 0 on the Groww LTP-only feed).
    pub volume: i64,
    /// Live-side tick count for the minute (0 on the BruteX side).
    pub tick_count: i64,
}

/// The comparison key: `(symbol identity, IST minute-bucket nanos)`. The
/// symbol identity string must be IDENTICAL across the two input maps
/// (Unit 5 keys both sides with the resolved identity or the raw symbol).
pub type BarKey = (String, i64);

/// One reportable cell finding (divergence / missing / tail / out-of-session
/// row — matched cells are counted, not materialized).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellFinding {
    /// The symbol identity of the affected `(symbol, minute)`.
    pub symbol: String,
    /// IST minute-bucket nanos of the affected minute.
    pub minute_nanos: i64,
    /// Cell classification (persistence wire enum).
    pub kind: BrutexCrossverifyCellKind,
    /// Which field diverged (`None` for whole-minute rows).
    pub field: BrutexCrossverifyField,
    /// BruteX-side value (paise for prices; `-1` sentinel when absent).
    pub brutex_value: i64,
    /// Live-side value (paise for prices; `-1` sentinel when absent).
    pub live_value: i64,
}

/// Per-symbol tallies for the day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SymbolTally {
    /// Minutes present on both sides and compared.
    pub minutes_compared: u64,
    /// Price cells matched within tolerance.
    pub cells_matched: u64,
    /// Price cells diverged beyond tolerance.
    pub cells_diverged: u64,
    /// In-window BruteX minutes with no live bar.
    pub missing_live: u64,
    /// Live minutes with no BruteX row.
    pub missing_brutex: u64,
    /// 15:28/15:29 BruteX minutes with no live bar (close-seal timing).
    pub tail_unsealed: u64,
    /// BruteX rows outside [09:15, 15:30) IST.
    pub out_of_session: u64,
}

/// Absolute-paise noise statistics over ALL compared price cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoiseStats {
    /// Median absolute difference (paise).
    pub p50_paise: i64,
    /// 95th-percentile absolute difference (paise).
    pub p95_paise: i64,
    /// Maximum absolute difference (paise).
    pub max_paise: i64,
}

/// Full day-comparison result.
#[derive(Clone, Debug, Default)]
pub struct DayComparison {
    /// Daily outcome (persistence wire enum). `None` only before evaluation
    /// — [`compare_day`] always sets it.
    pub outcome: Option<BrutexCrossverifyDailyOutcome>,
    /// Divergent / missing / tail / out-of-session cell findings.
    pub findings: Vec<CellFinding>,
    /// Per-symbol tallies (deterministic iteration order).
    pub per_symbol: BTreeMap<String, SymbolTally>,
    /// Minutes present on both sides and compared.
    pub minutes_compared: u64,
    /// Price cells compared (4 per compared minute, + volume when active).
    pub cells_compared: u64,
    /// Price cells matched within tolerance.
    pub cells_matched: u64,
    /// Price cells diverged beyond tolerance.
    pub cells_diverged: u64,
    /// In-window BruteX minutes with no live bar.
    pub missing_live: u64,
    /// Live minutes with no BruteX row.
    pub missing_brutex: u64,
    /// Tail-window (15:28/15:29) BruteX minutes with no live bar.
    pub tail_unsealed: u64,
    /// BruteX rows outside the session window.
    pub out_of_session: u64,
    /// True when `compare_volume` was requested but EVERY live volume was 0
    /// (LTP-only feed) — volume classification was skipped as a no-op.
    pub volume_check_noop: bool,
    /// Noise statistics over all compared price cells.
    pub noise: NoiseStats,
    /// Optional ±60s minute-alignment hint (set only when the gross
    /// mismatch pattern strongly suggests a bucket shift).
    pub alignment_hint: Option<String>,
}

/// Seconds-of-day (IST) for a minute-bucket nanos key.
fn secs_of_day(minute_nanos: i64) -> i64 {
    (minute_nanos / NANOS_PER_SEC).rem_euclid(86_400)
}

/// Window classification for one BruteX minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MinuteWindow {
    /// [09:15, 15:28) — fully classified.
    Full,
    /// [15:28, 15:30) — compared when the live bar exists, else
    /// `tail_unsealed`.
    Tail,
    /// Outside [09:15, 15:30) — recorded, never classified.
    OutOfSession,
}

fn classify_window(minute_nanos: i64) -> MinuteWindow {
    let s = secs_of_day(minute_nanos);
    if (SESSION_OPEN_SECS_OF_DAY..FULL_CLASSIFY_END_SECS_OF_DAY).contains(&s) {
        MinuteWindow::Full
    } else if (FULL_CLASSIFY_END_SECS_OF_DAY..SESSION_CLOSE_SECS_OF_DAY).contains(&s) {
        MinuteWindow::Tail
    } else {
        MinuteWindow::OutOfSession
    }
}

/// Nearest-rank percentile over a SORTED ascending slice. Empty → 0.
fn percentile_sorted(sorted: &[i64], pct: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    let rank = ((pct / 100.0) * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted.get(idx).copied().unwrap_or(0)
}

/// True when both bars' four price fields all match within the tolerance.
fn minute_prices_match(brutex: &MinuteBar, live: &MinuteBar, tolerance_paise: i64) -> bool {
    (brutex.open_paise - live.open_paise).abs() <= tolerance_paise
        && (brutex.high_paise - live.high_paise).abs() <= tolerance_paise
        && (brutex.low_paise - live.low_paise).abs() <= tolerance_paise
        && (brutex.close_paise - live.close_paise).abs() <= tolerance_paise
}

/// Minute-level match rate (0..=100) for a symbol subset with the BruteX
/// minutes shifted by `shift_nanos` — the alignment-probe scorer. Returns
/// `None` when no shifted minute finds a live counterpart.
fn shifted_match_rate(
    brutex: &HashMap<BarKey, MinuteBar>,
    live: &HashMap<BarKey, MinuteBar>,
    symbols: &[&String],
    tolerance_paise: i64,
    shift_nanos: i64,
) -> Option<f64> {
    let mut considered = 0u64;
    let mut matched = 0u64;
    for ((symbol, minute), bar) in brutex {
        if !symbols.contains(&symbol) {
            continue;
        }
        if classify_window(*minute) == MinuteWindow::OutOfSession {
            continue;
        }
        considered += 1;
        let shifted_key = (symbol.clone(), minute.saturating_add(shift_nanos));
        if let Some(live_bar) = live.get(&shifted_key)
            && minute_prices_match(bar, live_bar, tolerance_paise)
        {
            matched += 1;
        }
    }
    if considered == 0 {
        None
    } else {
        Some(100.0 * matched as f64 / considered as f64)
    }
}

/// Compare one trading day of BruteX minute bars against live minute bars.
///
/// Both maps are keyed `(symbol identity, IST minute nanos)` with prices in
/// integer paise. Pure — the caller owns persistence, Telegram, and error
/// codes. See the module docs for classification semantics.
#[must_use]
// TEST-EXEMPT: covered by the `compare_*` tests below (same-module suite).
pub fn compare_day(
    brutex: &HashMap<BarKey, MinuteBar>,
    live: &HashMap<BarKey, MinuteBar>,
    cfg: &CompareCfg,
) -> DayComparison {
    let mut out = DayComparison::default();

    // Volume no-op detection: compare_volume requested but the live feed is
    // LTP-only (every live volume is 0).
    let volume_active = if cfg.compare_volume {
        let all_zero = !live.is_empty() && live.values().all(|b| b.volume == 0);
        if all_zero {
            out.volume_check_noop = true;
        }
        !all_zero && !live.is_empty()
    } else {
        false
    };

    let mut abs_diffs: Vec<i64> = Vec::new();

    for ((symbol, minute), bx) in brutex {
        let tally = out.per_symbol.entry(symbol.clone()).or_default();
        let window = classify_window(*minute);
        if window == MinuteWindow::OutOfSession {
            out.out_of_session += 1;
            tally.out_of_session += 1;
            out.findings.push(CellFinding {
                symbol: symbol.clone(),
                minute_nanos: *minute,
                kind: BrutexCrossverifyCellKind::OutOfSession,
                field: BrutexCrossverifyField::None,
                brutex_value: bx.close_paise,
                live_value: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
            });
            continue;
        }

        match live.get(&(symbol.clone(), *minute)) {
            Some(lv) => {
                out.minutes_compared += 1;
                tally.minutes_compared += 1;
                let fields: [(BrutexCrossverifyField, i64, i64); 4] = [
                    (BrutexCrossverifyField::Open, bx.open_paise, lv.open_paise),
                    (BrutexCrossverifyField::High, bx.high_paise, lv.high_paise),
                    (BrutexCrossverifyField::Low, bx.low_paise, lv.low_paise),
                    (
                        BrutexCrossverifyField::Close,
                        bx.close_paise,
                        lv.close_paise,
                    ),
                ];
                for (field, b, l) in fields {
                    out.cells_compared += 1;
                    let diff = (b - l).abs();
                    abs_diffs.push(diff);
                    if diff <= cfg.tolerance_paise {
                        out.cells_matched += 1;
                        tally.cells_matched += 1;
                    } else {
                        out.cells_diverged += 1;
                        tally.cells_diverged += 1;
                        out.findings.push(CellFinding {
                            symbol: symbol.clone(),
                            minute_nanos: *minute,
                            kind: BrutexCrossverifyCellKind::Diverged,
                            field,
                            brutex_value: b,
                            live_value: l,
                        });
                    }
                }
                if volume_active {
                    out.cells_compared += 1;
                    if bx.volume == lv.volume {
                        out.cells_matched += 1;
                        tally.cells_matched += 1;
                    } else {
                        out.cells_diverged += 1;
                        tally.cells_diverged += 1;
                        out.findings.push(CellFinding {
                            symbol: symbol.clone(),
                            minute_nanos: *minute,
                            kind: BrutexCrossverifyCellKind::Diverged,
                            field: BrutexCrossverifyField::Volume,
                            brutex_value: bx.volume,
                            live_value: lv.volume,
                        });
                    }
                }
            }
            None => {
                let (kind, counter, sym_counter): (_, &mut u64, &mut u64) =
                    if window == MinuteWindow::Tail {
                        (
                            BrutexCrossverifyCellKind::TailUnsealed,
                            &mut out.tail_unsealed,
                            &mut tally.tail_unsealed,
                        )
                    } else {
                        (
                            BrutexCrossverifyCellKind::MissingLive,
                            &mut out.missing_live,
                            &mut tally.missing_live,
                        )
                    };
                *counter += 1;
                *sym_counter += 1;
                out.findings.push(CellFinding {
                    symbol: symbol.clone(),
                    minute_nanos: *minute,
                    kind,
                    field: BrutexCrossverifyField::None,
                    brutex_value: bx.close_paise,
                    live_value: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
                });
            }
        }
    }

    // Live-only minutes → missing_brutex.
    for ((symbol, minute), lv) in live {
        if brutex.contains_key(&(symbol.clone(), *minute)) {
            continue;
        }
        let tally = out.per_symbol.entry(symbol.clone()).or_default();
        out.missing_brutex += 1;
        tally.missing_brutex += 1;
        out.findings.push(CellFinding {
            symbol: symbol.clone(),
            minute_nanos: *minute,
            kind: BrutexCrossverifyCellKind::MissingBrutex,
            field: BrutexCrossverifyField::None,
            brutex_value: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
            live_value: lv.close_paise,
        });
    }

    // Noise stats over ALL compared price cells.
    abs_diffs.sort_unstable();
    out.noise = NoiseStats {
        p50_paise: percentile_sorted(&abs_diffs, 50.0),
        p95_paise: percentile_sorted(&abs_diffs, 95.0),
        max_paise: *abs_diffs.last().unwrap_or(&0),
    };

    // Alignment hint: when (diverged cells + missing minutes) dominate the
    // comparison universe, probe ±60s bucket shifts on ≤20 symbols.
    out.alignment_hint = compute_alignment_hint(brutex, live, cfg, &out);

    // Outcome (audit Rule 11 — never a clean verdict on an empty compare).
    out.outcome = Some(if out.minutes_compared == 0 {
        if brutex.is_empty() && live.is_empty() {
            BrutexCrossverifyDailyOutcome::NoData
        } else {
            BrutexCrossverifyDailyOutcome::Blind
        }
    } else if out.cells_diverged > 0 {
        BrutexCrossverifyDailyOutcome::Diverged
    } else if out.missing_live > 0 || out.missing_brutex > 0 {
        BrutexCrossverifyDailyOutcome::Partial
    } else {
        BrutexCrossverifyDailyOutcome::Clean
    });

    out
}

/// Probe ±60s minute-bucket shifts when the gross mismatch pattern suggests
/// a systematic alignment problem. Returns a human-readable hint string.
fn compute_alignment_hint(
    brutex: &HashMap<BarKey, MinuteBar>,
    live: &HashMap<BarKey, MinuteBar>,
    cfg: &CompareCfg,
    day: &DayComparison,
) -> Option<String> {
    if day.minutes_compared == 0 && day.missing_live == 0 {
        return None;
    }
    let comparisons = day.cells_compared + day.missing_live + day.missing_brutex;
    if comparisons == 0 {
        return None;
    }
    let mismatch = day.cells_diverged + day.missing_live + day.missing_brutex;
    if (mismatch as f64) <= ALIGNMENT_TRIGGER_FRACTION * comparisons as f64 {
        return None;
    }

    // ≤20 distinct symbols with BruteX minutes.
    let mut symbols: Vec<&String> = Vec::new();
    for (symbol, _) in brutex.keys() {
        if !symbols.contains(&symbol) {
            symbols.push(symbol);
            if symbols.len() >= ALIGNMENT_PROBE_MAX_SYMBOLS {
                break;
            }
        }
    }
    if symbols.is_empty() {
        return None;
    }

    let baseline =
        shifted_match_rate(brutex, live, &symbols, cfg.tolerance_paise, 0).unwrap_or(0.0);
    let plus = shifted_match_rate(
        brutex,
        live,
        &symbols,
        cfg.tolerance_paise,
        SECS_PER_MINUTE * NANOS_PER_SEC,
    )
    .unwrap_or(0.0);
    let minus = shifted_match_rate(
        brutex,
        live,
        &symbols,
        cfg.tolerance_paise,
        -(SECS_PER_MINUTE * NANOS_PER_SEC),
    )
    .unwrap_or(0.0);

    if plus - baseline > ALIGNMENT_IMPROVEMENT_POINTS {
        Some(format!(
            "possible +60s minute-bucket misalignment: shifted match {plus:.0}% vs baseline {baseline:.0}%"
        ))
    } else if minus - baseline > ALIGNMENT_IMPROVEMENT_POINTS {
        Some(format!(
            "possible -60s minute-bucket misalignment: shifted match {minus:.0}% vs baseline {baseline:.0}%"
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "symbol,timestamp_ist,open,high,low,close,volume,oi";

    fn csv(lines: &[&str]) -> Vec<u8> {
        let mut s = String::from(HEADER);
        for l in lines {
            s.push('\n');
            s.push_str(l);
        }
        s.into_bytes()
    }

    /// IST minute nanos for HH:MM on a fixed trading date (2026-07-10).
    fn minute_nanos(hh: i64, mm: i64) -> i64 {
        // 2026-07-10 00:00:00 as naive epoch seconds.
        let midnight = chrono::NaiveDate::from_ymd_opt(2026, 7, 10)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|dt| dt.and_utc().timestamp())
            .unwrap_or(0);
        (midnight + hh * 3600 + mm * 60) * NANOS_PER_SEC
    }

    fn bar(o: i64, h: i64, l: i64, c: i64) -> MinuteBar {
        MinuteBar {
            open_paise: o,
            high_paise: h,
            low_paise: l,
            close_paise: c,
            volume: 0,
            tick_count: 1,
        }
    }

    // ----- CSV: happy path + hostile inputs -----------------------------

    #[test]
    fn csv_happy_path_three_rows_with_volume_and_oi() {
        let body = csv(&[
            "RELIANCE,2026-07-10 09:15:00,2847.50,2848.00,2847.00,2847.75,1200,0",
            "NIFTY,2026-07-10T09:16:00,25650.10,25651.00,25649.95,25650.55,0,0",
            "reliance,2026-07-10 09:17:23,2848.00,2848.10,2847.90,2848.05,300,42",
        ]);
        let parsed = parse_brutex_csv(&body, 10_000).expect("parses");
        assert_eq!(parsed.rows.len(), 3);
        assert_eq!(parsed.bad_rows, 0);
        assert_eq!(parsed.duplicate_minutes, 0);
        assert!(!parsed.truncated);
        let r0 = &parsed.rows[0];
        assert_eq!(r0.symbol, "RELIANCE");
        assert_eq!(r0.open_paise, 284_750);
        assert_eq!(r0.close_paise, 284_775);
        assert_eq!(r0.volume, 1200);
        // 09:17:23 floors to 09:17:00.
        assert_eq!(parsed.rows[2].minute_nanos % (60 * NANOS_PER_SEC), 0);
        assert_eq!(parsed.rows[2].symbol, "RELIANCE");
        assert_eq!(parsed.rows[2].oi, 42);
    }

    #[test]
    fn csv_bom_and_crlf_are_normalized() {
        let mut body = b"\xef\xbb\xbf".to_vec();
        body.extend_from_slice(HEADER.as_bytes());
        body.extend_from_slice(b"\r\nNIFTY,2026-07-10 09:15:00,100.00,101.00,99.00,100.50,0,0\r\n");
        let parsed = parse_brutex_csv(&body, 100).expect("parses");
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.bad_rows, 0);
    }

    #[test]
    fn csv_missing_header_is_typed_error() {
        let body = b"foo,bar\n1,2\n";
        let err = parse_brutex_csv(body, 100).expect_err("must fail");
        assert!(matches!(err, BrutexCsvError::MissingHeaderColumn(_)));
    }

    #[test]
    fn csv_shuffled_header_resolves_by_name() {
        let body =
            b"oi,close,volume,low,high,open,timestamp_ist,symbol\n7,100.50,5,99.00,101.00,100.00,2026-07-10 09:20:00,TCS\n";
        let parsed = parse_brutex_csv(body, 100).expect("parses");
        assert_eq!(parsed.rows.len(), 1);
        let r = &parsed.rows[0];
        assert_eq!(r.symbol, "TCS");
        assert_eq!(r.open_paise, 10_000);
        assert_eq!(r.close_paise, 10_050);
        assert_eq!(r.volume, 5);
        assert_eq!(r.oi, 7);
    }

    #[test]
    fn csv_empty_and_header_only_bodies() {
        assert_eq!(parse_brutex_csv(b"", 10), Err(BrutexCsvError::Empty));
        assert_eq!(parse_brutex_csv(b"   \n  ", 10), Err(BrutexCsvError::Empty));
        let parsed = parse_brutex_csv(HEADER.as_bytes(), 10).expect("header only ok");
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.total_data_lines, 0);
    }

    #[test]
    fn csv_not_utf8_is_typed_error() {
        let body = vec![0xff, 0xfe, 0x00, 0x41];
        assert_eq!(parse_brutex_csv(&body, 10), Err(BrutexCsvError::NotUtf8));
    }

    #[test]
    fn csv_hostile_rows_are_fail_soft_counted_and_sampled() {
        let long_symbol = "A".repeat(300);
        let script = "<script>alert(1)</script>,2026-07-10 09:15:00,1,2,1,2,0,0".to_owned();
        let body = csv(&[
            &format!("{long_symbol},2026-07-10 09:15:00,1.00,2.00,1.00,1.50,0,0"),
            &script,
            "NIFTY,2026-07-10 09:15:00,NaN,2.00,1.00,1.50,0,0",
            "NIFTY,2026-07-10 09:15:00,inf,2.00,1.00,1.50,0,0",
            "NIFTY,2026-07-10 09:15:00,-5.00,2.00,1.00,1.50,0,0",
            "NIFTY,2026-07-10 09:15:00,99999999999.00,2.00,1.00,1.50,0,0",
            "NIFTY,not-a-timestamp,1.00,2.00,1.00,1.50,0,0",
            "NIFTY,2026-07-10 09:15:00,1.00,2.00,1.00,1.50,junk,0",
            "NIFTY,2026-07-10 09:15:00,1.00,2.00,1.00,1.50,-1,0",
            "GOOD,2026-07-10 09:15:00,1.00,2.00,1.00,1.50,10,0",
        ]);
        let parsed = parse_brutex_csv(&body, 10_000).expect("parses");
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.rows[0].symbol, "GOOD");
        assert_eq!(parsed.bad_rows, 9);
        assert_eq!(parsed.bad_row_samples.len(), MAX_BAD_ROW_SAMPLES);
        for sample in &parsed.bad_row_samples {
            assert!(sample.chars().count() <= BAD_ROW_SAMPLE_MAX_CHARS);
        }
    }

    /// FIX ROUND 2: the parse charset is ALIGNED with the API's
    /// `is_valid_symbol_param` (`[A-Za-z0-9 ._-]`) — `&` and `/` symbols
    /// become counted bad rows so a comparable row can never carry a
    /// symbol the drill-down API would 400 on. NOTE: real NSE symbols
    /// like `M&M` / `M&MFIN` are therefore excluded until a FUTURE
    /// contract amendment widens the §37.3 charset on BOTH sides
    /// (parse + API) in lockstep.
    #[test]
    fn csv_symbol_with_ampersand_or_slash_is_bad_row() {
        let body = csv(&[
            "M&M,2026-07-10 09:15:00,1.00,2.00,1.00,1.50,0,0",
            "A/B,2026-07-10 09:15:00,1.00,2.00,1.00,1.50,0,0",
            "GOOD,2026-07-10 09:15:00,1.00,2.00,1.00,1.50,0,0",
        ]);
        let parsed = parse_brutex_csv(&body, 100).expect("parses");
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.rows[0].symbol, "GOOD");
        assert_eq!(parsed.bad_rows, 2);
    }

    #[test]
    fn csv_duplicate_minute_last_wins() {
        let body = csv(&[
            "NIFTY,2026-07-10 09:15:10,100.00,101.00,99.00,100.10,0,0",
            "NIFTY,2026-07-10 09:15:50,200.00,201.00,199.00,200.20,0,0",
        ]);
        let parsed = parse_brutex_csv(&body, 100).expect("parses");
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.duplicate_minutes, 1);
        assert_eq!(parsed.rows[0].close_paise, 20_020);
    }

    #[test]
    fn csv_stops_at_max_rows_with_truncated_flag() {
        let body = csv(&[
            "A,2026-07-10 09:15:00,1.00,2.00,1.00,1.50,0,0",
            "B,2026-07-10 09:15:00,1.00,2.00,1.00,1.50,0,0",
            "C,2026-07-10 09:15:00,1.00,2.00,1.00,1.50,0,0",
        ]);
        let parsed = parse_brutex_csv(&body, 2).expect("parses");
        assert_eq!(parsed.rows.len(), 2);
        assert!(parsed.truncated);
    }

    #[test]
    fn csv_seeded_fuzz_never_panics() {
        // Deterministic LCG (no external dep — proptest is not an app
        // dev-dependency); mutates a valid CSV and asserts total safety.
        let base = csv(&[
            "RELIANCE,2026-07-10 09:15:00,2847.50,2848.00,2847.00,2847.75,1200,0",
            "NIFTY,2026-07-10 09:16:00,25650.10,25651.00,25649.95,25650.55,0,0",
        ]);
        let mut state: u64 = 0x5EED_CAFE_1234_5678;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 33) as usize
        };
        for _ in 0..300 {
            let mut mutated = base.clone();
            let flips = next() % 8 + 1;
            for _ in 0..flips {
                let pos = next() % mutated.len();
                mutated[pos] = (next() % 256) as u8;
            }
            // Must never panic; counters must stay coherent.
            if let Ok(parsed) = parse_brutex_csv(&mutated, 64) {
                assert!(
                    parsed.bad_rows + parsed.rows.len()
                        <= parsed.total_data_lines + parsed.duplicate_minutes + 1
                );
                assert!(parsed.bad_row_samples.len() <= MAX_BAD_ROW_SAMPLES);
            }
        }
    }

    // ----- Symbol mapping ------------------------------------------------

    fn lifecycle_fixture() -> Vec<LifecycleRow> {
        vec![
            LifecycleRow {
                security_id: 2885,
                exchange_segment: "NSE_EQ".into(),
                instrument_type: "EQUITY".into(),
                symbol_name: "RELIANCE".into(),
                underlying_symbol: String::new(),
                expiry_ymd: None,
            },
            LifecycleRow {
                security_id: 500325,
                exchange_segment: "BSE_EQ".into(),
                instrument_type: "EQUITY".into(),
                symbol_name: "RELIANCE".into(),
                underlying_symbol: String::new(),
                expiry_ymd: None,
            },
            LifecycleRow {
                security_id: 13,
                exchange_segment: "IDX_I".into(),
                instrument_type: "INDEX".into(),
                symbol_name: "NIFTY".into(),
                underlying_symbol: String::new(),
                expiry_ymd: None,
            },
            LifecycleRow {
                security_id: 442,
                exchange_segment: "IDX_I".into(),
                instrument_type: "INDEX".into(),
                symbol_name: "NIFTY MIDCAP SELECT".into(),
                underlying_symbol: String::new(),
                expiry_ymd: None,
            },
            LifecycleRow {
                security_id: 53001,
                exchange_segment: "NSE_FNO".into(),
                instrument_type: "FUTIDX".into(),
                symbol_name: "NIFTY-Jul2026-FUT".into(),
                underlying_symbol: "NIFTY".into(),
                expiry_ymd: Some("2026-07-30".into()),
            },
            LifecycleRow {
                security_id: 87001,
                exchange_segment: "BSE_FNO".into(),
                instrument_type: "FUTIDX".into(),
                symbol_name: "SENSEX-Aug2026-FUT".into(),
                underlying_symbol: "SENSEX".into(),
                expiry_ymd: Some("2026-08-28".into()),
            },
        ]
    }

    #[test]
    fn map_equity_prefers_nse_eq_over_bse_eq() {
        let map = GrowwSymbolMap::from_lifecycle_rows(lifecycle_fixture());
        assert_eq!(
            map.resolve("reliance"),
            Resolution::Resolved {
                security_id: 2885,
                segment: "NSE_EQ".into()
            }
        );
        assert_eq!(map.ambiguous_key_count(), 0);
    }

    #[test]
    fn map_index_via_prefix_and_alias() {
        let map = GrowwSymbolMap::from_lifecycle_rows(lifecycle_fixture());
        assert_eq!(
            map.resolve("NSE-NIFTY"),
            Resolution::Resolved {
                security_id: 13,
                segment: "IDX_I".into()
            }
        );
        // INDEX_SYMBOL_ALIASES: "NIFTY MIDCAP SELECT" → MIDCPNIFTY.
        assert_eq!(
            map.resolve("NIFTY MIDCAP SELECT"),
            Resolution::Resolved {
                security_id: 442,
                segment: "IDX_I".into()
            }
        );
        assert_eq!(
            map.resolve("MIDCPNIFTY"),
            Resolution::Resolved {
                security_id: 442,
                segment: "IDX_I".into()
            }
        );
    }

    #[test]
    fn map_futures_both_forms_roundtrip_to_same_contract() {
        let map = GrowwSymbolMap::from_lifecycle_rows(lifecycle_fixture());
        let expected = Resolution::Resolved {
            security_id: 53001,
            segment: "NSE_FNO".into(),
        };
        assert_eq!(map.resolve("NIFTY-2026-07-30-FUT"), expected);
        assert_eq!(map.resolve("NSE-NIFTY-30Jul26-FUT"), expected);
        assert_eq!(map.resolve("nse-nifty-30JUL26-fut"), expected);
        assert_eq!(
            map.resolve("BSE-SENSEX-28Aug26-FUT"),
            Resolution::Resolved {
                security_id: 87001,
                segment: "BSE_FNO".into()
            }
        );
        // Wrong expiry → unmapped (never guess).
        assert_eq!(map.resolve("NIFTY-2026-08-27-FUT"), Resolution::Unmapped);
    }

    #[test]
    fn map_garbage_is_unmapped_and_duplicates_are_ambiguous() {
        let mut rows = lifecycle_fixture();
        // Two DISTINCT NSE_EQ identities for the same symbol → ambiguous.
        rows.push(LifecycleRow {
            security_id: 999,
            exchange_segment: "NSE_EQ".into(),
            instrument_type: "EQUITY".into(),
            symbol_name: "RELIANCE".into(),
            underlying_symbol: String::new(),
            expiry_ymd: None,
        });
        let map = GrowwSymbolMap::from_lifecycle_rows(rows);
        assert_eq!(map.resolve("RELIANCE"), Resolution::Ambiguous);
        assert_eq!(map.ambiguous_key_count(), 1);
        assert_eq!(map.resolve("TOTALLY-BOGUS-42"), Resolution::Unmapped);
        assert_eq!(map.resolve(""), Resolution::Unmapped);
    }

    // ----- compare_day ----------------------------------------------------

    fn key(symbol: &str, hh: i64, mm: i64) -> BarKey {
        (symbol.to_owned(), minute_nanos(hh, mm))
    }

    #[test]
    fn compare_exact_match_at_zero_tolerance_is_clean() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        brutex.insert(key("13:IDX_I", 9, 15), bar(100, 102, 99, 101));
        live.insert(key("13:IDX_I", 9, 15), bar(100, 102, 99, 101));
        let day = compare_day(&brutex, &live, &CompareCfg::default());
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::Clean));
        assert_eq!(day.minutes_compared, 1);
        assert_eq!(day.cells_compared, 4);
        assert_eq!(day.cells_matched, 4);
        assert_eq!(day.cells_diverged, 0);
        assert!(day.findings.is_empty());
    }

    #[test]
    fn compare_boundary_diff_equal_tolerance_is_matched_inclusive() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        brutex.insert(key("A", 10, 0), bar(100, 100, 100, 100));
        live.insert(key("A", 10, 0), bar(105, 95, 100, 100));
        let cfg = CompareCfg {
            tolerance_paise: 5,
            compare_volume: false,
        };
        let day = compare_day(&brutex, &live, &cfg);
        assert_eq!(day.cells_diverged, 0);
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::Clean));
    }

    #[test]
    fn compare_paise_rounding_100_004_vs_100_006_diverges_at_zero_tolerance() {
        // 100.004 → 10000 paise; 100.006 → 10001 paise.
        let b = parse_price_paise("100.004").expect("parses");
        let l = parse_price_paise("100.006").expect("parses");
        assert_eq!(b, 10_000);
        assert_eq!(l, 10_001);
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        brutex.insert(key("A", 11, 0), bar(b, b, b, b));
        live.insert(key("A", 11, 0), bar(l, l, l, l));
        let day = compare_day(&brutex, &live, &CompareCfg::default());
        assert_eq!(day.cells_diverged, 4);
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::Diverged));
        assert!(
            day.findings
                .iter()
                .all(|f| f.kind == BrutexCrossverifyCellKind::Diverged)
        );
    }

    #[test]
    fn compare_tail_unsealed_and_tail_compared() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        // 15:28 absent live → tail_unsealed; 15:29 present live → compared.
        brutex.insert(key("A", 15, 28), bar(1, 1, 1, 1));
        brutex.insert(key("A", 15, 29), bar(2, 2, 2, 2));
        live.insert(key("A", 15, 29), bar(2, 2, 2, 2));
        let day = compare_day(&brutex, &live, &CompareCfg::default());
        assert_eq!(day.tail_unsealed, 1);
        assert_eq!(day.minutes_compared, 1);
        assert_eq!(day.missing_live, 0);
        assert!(
            day.findings
                .iter()
                .any(|f| f.kind == BrutexCrossverifyCellKind::TailUnsealed)
        );
        // Tail gap is NOT a coverage gap → Clean.
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::Clean));
    }

    #[test]
    fn compare_out_of_session_rows_are_recorded_not_classified() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        brutex.insert(key("A", 9, 14), bar(1, 1, 1, 1)); // pre-open
        brutex.insert(key("A", 15, 30), bar(1, 1, 1, 1)); // post-close
        brutex.insert(key("A", 9, 15), bar(3, 3, 3, 3));
        live.insert(key("A", 9, 15), bar(3, 3, 3, 3));
        let day = compare_day(&brutex, &live, &CompareCfg::default());
        assert_eq!(day.out_of_session, 2);
        assert_eq!(day.minutes_compared, 1);
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::Clean));
    }

    #[test]
    fn compare_missing_both_ways_gives_partial() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        brutex.insert(key("A", 9, 15), bar(1, 1, 1, 1)); // brutex-only
        brutex.insert(key("A", 9, 16), bar(2, 2, 2, 2)); // compared
        live.insert(key("A", 9, 16), bar(2, 2, 2, 2));
        live.insert(key("A", 9, 17), bar(3, 3, 3, 3)); // live-only
        let day = compare_day(&brutex, &live, &CompareCfg::default());
        assert_eq!(day.missing_live, 1);
        assert_eq!(day.missing_brutex, 1);
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::Partial));
        let tally = day.per_symbol.get("A").expect("tally");
        assert_eq!(tally.missing_live, 1);
        assert_eq!(tally.missing_brutex, 1);
        assert_eq!(tally.minutes_compared, 1);
    }

    #[test]
    fn compare_no_data_and_blind_outcomes() {
        let empty: HashMap<BarKey, MinuteBar> = HashMap::new();
        let day = compare_day(&empty, &empty, &CompareCfg::default());
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::NoData));

        let mut brutex = HashMap::new();
        brutex.insert(key("A", 9, 15), bar(1, 1, 1, 1));
        let day = compare_day(&brutex, &empty, &CompareCfg::default());
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::Blind));
        assert_eq!(day.missing_live, 1);
    }

    #[test]
    fn compare_volume_noop_when_all_live_volume_zero() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        let mut bx = bar(1, 1, 1, 1);
        bx.volume = 500;
        brutex.insert(key("A", 9, 15), bx);
        live.insert(key("A", 9, 15), bar(1, 1, 1, 1)); // volume 0
        let cfg = CompareCfg {
            tolerance_paise: 0,
            compare_volume: true,
        };
        let day = compare_day(&brutex, &live, &cfg);
        assert!(day.volume_check_noop);
        // Volume never classified → 4 price cells only, Clean.
        assert_eq!(day.cells_compared, 4);
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::Clean));
    }

    #[test]
    fn compare_volume_classified_when_live_has_volume() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        let mut bx = bar(1, 1, 1, 1);
        bx.volume = 500;
        let mut lv = bar(1, 1, 1, 1);
        lv.volume = 400;
        brutex.insert(key("A", 9, 15), bx);
        live.insert(key("A", 9, 15), lv);
        let cfg = CompareCfg {
            tolerance_paise: 0,
            compare_volume: true,
        };
        let day = compare_day(&brutex, &live, &cfg);
        assert!(!day.volume_check_noop);
        assert_eq!(day.cells_compared, 5);
        assert_eq!(day.cells_diverged, 1);
        assert!(
            day.findings
                .iter()
                .any(|f| f.field == BrutexCrossverifyField::Volume)
        );
    }

    #[test]
    fn compare_noise_stats_ordering() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        // Diffs per field: 0, 1, 2, 10.
        brutex.insert(key("A", 9, 20), bar(100, 100, 100, 100));
        live.insert(key("A", 9, 20), bar(100, 101, 102, 110));
        let day = compare_day(&brutex, &live, &CompareCfg::default());
        assert!(day.noise.p50_paise <= day.noise.p95_paise);
        assert!(day.noise.p95_paise <= day.noise.max_paise);
        assert_eq!(day.noise.max_paise, 10);
        assert_eq!(day.noise.p50_paise, 1);
    }

    #[test]
    fn compare_alignment_hint_fires_on_systematic_plus_60s_shift() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        for i in 0..20i64 {
            let m = minute_nanos(10, i);
            let price = 1_000 + i;
            brutex.insert(("A".to_owned(), m), bar(price, price, price, price));
            // Live is shifted +60s relative to brutex.
            live.insert(
                ("A".to_owned(), m + 60 * NANOS_PER_SEC),
                bar(price, price, price, price),
            );
        }
        let day = compare_day(&brutex, &live, &CompareCfg::default());
        let hint = day.alignment_hint.expect("hint fires");
        assert!(hint.contains("+60s"), "hint was: {hint}");
    }

    #[test]
    fn compare_alignment_hint_absent_on_healthy_day() {
        let mut brutex = HashMap::new();
        let mut live = HashMap::new();
        for i in 0..20i64 {
            let m = minute_nanos(10, i);
            brutex.insert(("A".to_owned(), m), bar(100, 100, 100, 100));
            live.insert(("A".to_owned(), m), bar(100, 100, 100, 100));
        }
        let day = compare_day(&brutex, &live, &CompareCfg::default());
        assert!(day.alignment_hint.is_none());
        assert_eq!(day.outcome, Some(BrutexCrossverifyDailyOutcome::Clean));
    }

    // ----- internal helpers ------------------------------------------------

    #[test]
    fn helper_percentile_and_window_and_truncate() {
        assert_eq!(percentile_sorted(&[], 50.0), 0);
        assert_eq!(percentile_sorted(&[7], 95.0), 7);
        assert_eq!(percentile_sorted(&[1, 2, 3, 4], 50.0), 2);
        assert_eq!(classify_window(minute_nanos(9, 15)), MinuteWindow::Full);
        assert_eq!(classify_window(minute_nanos(15, 27)), MinuteWindow::Full);
        assert_eq!(classify_window(minute_nanos(15, 28)), MinuteWindow::Tail);
        assert_eq!(classify_window(minute_nanos(15, 29)), MinuteWindow::Tail);
        assert_eq!(
            classify_window(minute_nanos(15, 30)),
            MinuteWindow::OutOfSession
        );
        assert_eq!(
            classify_window(minute_nanos(9, 14)),
            MinuteWindow::OutOfSession
        );
        // char-boundary safe truncation of multibyte text.
        let truncated = truncate_chars("₹₹₹₹₹", 3);
        assert_eq!(truncated.chars().count(), 3);
    }

    #[test]
    fn helper_dd_mon_yy_variants() {
        assert_eq!(parse_dd_mon_yy("30JUL26").as_deref(), Some("2026-07-30"));
        assert_eq!(parse_dd_mon_yy("1JAN27").as_deref(), Some("2027-01-01"));
        assert_eq!(parse_dd_mon_yy("28AUG26").as_deref(), Some("2026-08-28"));
        assert_eq!(parse_dd_mon_yy("31FEB26"), None); // calendar-checked
        assert_eq!(parse_dd_mon_yy("30XXX26"), None);
        assert_eq!(parse_dd_mon_yy(""), None);
    }
}
