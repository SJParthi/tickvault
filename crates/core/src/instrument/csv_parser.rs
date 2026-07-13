//! Sub-PR #4 of 2026-05-27 daily-universe expansion — robust CSV
//! parser for the Dhan Detailed instrument-master file.
//!
//! **Feature-gated.** This module compiles only when the
//! `daily_universe_fetcher` cargo feature is enabled (per §21 of the
//! authoritative rule file). Under the current `Indices4Only` default
//! scope this module is dead code.
//!
//! ## Robustness contract (§26 of the rule file)
//!
//! Edge cases the parser MUST handle correctly:
//!
//! | Edge case | Behaviour |
//! |---|---|
//! | UTF-8 BOM (`\xEF\xBB\xBF`) at file start | Strip silently |
//! | Mixed CRLF / LF / CR line endings | Normalized via `csv::ReaderBuilder` defaults |
//! | Quoted field with embedded comma `"REL,IND"` | Parsed via `Trim::All` mode |
//! | UTF-8 strict | Reject ISO-8859-1 bytes outside 7-bit ASCII |
//! | BiDi override chars in `symbol_name` | Stripped via `sanitize_audit_string` |
//! | Mandatory field missing on a single row | Drop row + count |
//! | >0.1% of rows fail mandatory-field check | Reject WHOLE CSV |
//!
//! ## What this module DOES NOT do (deferred)
//!
//! - Dangling-reference check (every `UNDERLYING_SECURITY_ID` cited by
//!   FUTSTK/OPTSTK MUST exist as an NSE_EQ row) — Sub-PR #5 / #7
//!   (universe-builder layer; needs the cross-row index)
//! - SHA-256 + row-count anomaly check — Sub-PR #9 (lifecycle reconciler)
//! - Caller-side persistence to `instrument_lifecycle` table — Sub-PR #9
//!
//! This module ships ONLY the parser: bytes in, validated rows out.
//!
//! PR-C1 (2026-07-13): the module-level `daily_universe_fetcher` gate was
//! REMOVED — `index_futures` (de-gated per the daily-universe 2026-07-13
//! banner §(d)) depends on [`CsvRow`] at compile time, so this pure parse
//! module must build in every feature mode.

use thiserror::Error;
use tickvault_common::sanitize::sanitize_audit_string;

/// UTF-8 byte-order mark prefix. Some CSVs start with this; strip it
/// silently per §26.
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Reject threshold for mandatory-field failures across the whole CSV.
/// Per §3 + §26: > 0.1% of rows failing means the CSV is treated as
/// corrupt; below threshold means drop the bad rows + continue.
///
/// Value: 0.001 = 0.1% in decimal.
pub const MANDATORY_FIELD_REJECT_THRESHOLD: f64 = 0.001;

/// Derivative instrument-type prefixes — these rows additionally need a
/// non-empty `UNDERLYING_SECURITY_ID` per §26 dangling-reference rule.
/// The dangling-reference cross-check itself lives in Sub-PR #5+; this
/// parser only enforces presence (not validity) of the field.
const DERIVATIVE_INSTRUMENT_PREFIXES: &[&str] = &[
    "FUTIDX", "OPTIDX", "FUTSTK", "OPTSTK", "FUTCUR", "OPTCUR", "FUTCOM", "OPTFUT",
];

/// A single parsed row from the Detailed CSV. Field names match the
/// Dhan schema (PascalCase columns rendered into snake_case Rust fields).
///
/// Strings are owned; the parser sanitizes `symbol_name` against BiDi
/// override unicode + ILP-injection characters per the audit-string
/// rule.
// NOTE: `Eq` was dropped when the f64 detail fields (`tick_size`,
// `strike_price`) were added — `f64: !Eq`. `CsvRow` is only ever a map
// VALUE (`HashMap<String, CsvRow>`) / `Vec<CsvRow>`, never a key, so
// `Eq`/`Hash` are not required. `Default` is derived so the (test-only)
// construction sites can spread `..Default::default()` for the optional
// detail fields.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CsvRow {
    pub security_id: String,
    pub exch_id: String,
    pub segment: String,
    pub instrument: String,
    pub symbol_name: String,
    /// Empty string for non-derivative rows. The parser does NOT
    /// validate that this resolves to a real NSE_EQ row — that cross-
    /// check lives in Sub-PR #5+.
    pub underlying_security_id: String,
    // ---- Optional detail columns (Dhan Detailed CSV) ----
    // Not mandatory: indices have no lot/strike/expiry; the parser
    // defaults each to empty/0 when the column is absent or the cell is
    // blank/unparseable (best-effort — never rejects a row over these).
    /// `UNDERLYING_SYMBOL` — symbol of the underlying (derivatives only).
    pub underlying_symbol: String,
    /// `DISPLAY_NAME` — human-readable Dhan display name.
    pub display_name: String,
    /// `LOT_SIZE` — trading lot (0 if absent/unparseable).
    pub lot_size: i32,
    /// `TICK_SIZE` — minimum price increment (0.0 if absent/unparseable).
    pub tick_size: f64,
    /// `SM_EXPIRY_DATE` — raw `YYYY-MM-DD` string (empty for spot/index).
    /// Date→nanos conversion happens in the lifecycle-extraction layer.
    pub expiry_date: String,
    /// `STRIKE_PRICE` — options only (0.0 if absent/unparseable).
    pub strike_price: f64,
    /// `OPTION_TYPE` — `CE` / `PE` / empty for non-options.
    pub option_type: String,
    /// `ISIN` — the 12-char security identity (e.g. `INE002A01018`).
    /// Empty when absent (Compact CSV). The §31.1 PRIMARY join key for
    /// resolving niftyindices constituents → NSE-EQ `security_id`
    /// (Sub-PR #4 `constituent_resolver`).
    pub isin: String,
}

/// Errors that can occur during CSV parsing.
#[derive(Debug, Error)]
pub enum CsvParseError {
    /// Underlying `csv::Reader` error (malformed CSV structure,
    /// unbalanced quotes, etc.).
    #[error("CSV reader error: {0}")]
    Reader(#[from] csv::Error),

    /// Body bytes are not valid UTF-8 (e.g. ISO-8859-1).
    #[error("CSV body is not valid UTF-8")]
    NotUtf8,

    /// Required CSV column missing from the header row. The Dhan
    /// Detailed CSV schema is fixed; missing columns mean the schema
    /// changed underneath us — abort, do not silently fill blanks.
    #[error("required header column missing: {column}")]
    MissingHeaderColumn { column: String },

    /// More than `MANDATORY_FIELD_REJECT_THRESHOLD` of rows failed
    /// mandatory-field validation. CSV is corrupt; the whole batch is
    /// rejected.
    #[error(
        "{failed_rows} of {total_rows} rows ({failure_pct:.4}%) failed mandatory-field check — \
         threshold {threshold_pct:.4}%"
    )]
    RowFailureThresholdExceeded {
        total_rows: usize,
        failed_rows: usize,
        failure_pct: f64,
        threshold_pct: f64,
    },

    /// The CSV body was empty or had no rows after the header.
    #[error("CSV body has no data rows")]
    Empty,
}

/// Parse a Detailed-CSV byte buffer into a vector of validated rows.
///
/// Strips a leading UTF-8 BOM if present. Validates each row against
/// the mandatory-field contract. Drops bad rows below threshold;
/// rejects the whole CSV above threshold.
///
/// The returned `Vec<CsvRow>` is in the same order as the input CSV
/// (minus dropped bad rows).
///
/// # Errors
///
/// See [`CsvParseError`] variants.
pub fn parse_detailed_csv(bytes: &[u8]) -> Result<Vec<CsvRow>, CsvParseError> {
    let cleaned = strip_utf8_bom(bytes);

    // UTF-8 validation up-front — csv::Reader does its own per-field
    // UTF-8 validation but a single binary byte would surface as a
    // confusing per-row error; promote to a clear top-level error.
    std::str::from_utf8(cleaned).map_err(|_| CsvParseError::NotUtf8)?;

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .trim(csv::Trim::All) // strips whitespace inside quoted fields too
        .flexible(true) // tolerate trailing-empty-column rows
        .from_reader(cleaned);

    let column_indices = build_column_indices(&mut rdr)?;

    let mut rows = Vec::new();
    let mut total_rows: usize = 0;
    let mut failed_rows: usize = 0;

    for record in rdr.records() {
        total_rows = total_rows.saturating_add(1);
        let record = record?;
        match extract_row(&record, &column_indices) {
            Some(row) => rows.push(row),
            None => failed_rows = failed_rows.saturating_add(1),
        }
    }

    if total_rows == 0 {
        return Err(CsvParseError::Empty);
    }

    let failure_pct = (failed_rows as f64) / (total_rows as f64);
    if failure_pct > MANDATORY_FIELD_REJECT_THRESHOLD {
        return Err(CsvParseError::RowFailureThresholdExceeded {
            total_rows,
            failed_rows,
            failure_pct: failure_pct * 100.0,
            threshold_pct: MANDATORY_FIELD_REJECT_THRESHOLD * 100.0,
        });
    }

    Ok(rows)
}

/// Strip a leading UTF-8 BOM from the byte buffer if present. Returns
/// the buffer unchanged if no BOM is detected.
#[must_use]
fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(UTF8_BOM) {
        &bytes[UTF8_BOM.len()..]
    } else {
        bytes
    }
}

/// Indices of the mandatory columns + `UNDERLYING_SECURITY_ID` in the
/// CSV header. Resolved once from the header row so per-row extraction
/// is O(1) field lookup instead of column-name string compare.
struct ColumnIndices {
    security_id: usize,
    exch_id: usize,
    segment: usize,
    instrument: usize,
    symbol_name: usize,
    underlying_security_id: usize,
    // Optional detail columns — `usize::MAX` sentinel when absent.
    underlying_symbol: usize,
    display_name: usize,
    lot_size: usize,
    tick_size: usize,
    expiry_date: usize,
    strike_price: usize,
    option_type: usize,
    isin: usize,
}

fn build_column_indices<R: std::io::Read>(
    rdr: &mut csv::Reader<R>,
) -> Result<ColumnIndices, CsvParseError> {
    let headers = rdr.headers()?;
    let find = |name: &str| -> Result<usize, CsvParseError> {
        headers
            .iter()
            .position(|h| h.eq_ignore_ascii_case(name))
            .ok_or_else(|| CsvParseError::MissingHeaderColumn {
                column: name.to_string(),
            })
    };
    Ok(ColumnIndices {
        security_id: find("SECURITY_ID")?,
        exch_id: find("EXCH_ID")?,
        segment: find("SEGMENT")?,
        instrument: find("INSTRUMENT")?,
        symbol_name: find("SYMBOL_NAME")?,
        // UNDERLYING_SECURITY_ID is OPTIONAL in the schema — derivative
        // rows have it populated, non-derivatives may have it blank or
        // not present at all. If the header is missing we treat all
        // rows as having an empty value (handled in extract_row).
        underlying_security_id: find("UNDERLYING_SECURITY_ID").unwrap_or(usize::MAX),
        // Detail columns are all OPTIONAL — `usize::MAX` when the header
        // is absent, so the parser still works on a Compact CSV. Column
        // names per `docs/dhan-ref/09-instrument-master.md` §2.
        underlying_symbol: find("UNDERLYING_SYMBOL").unwrap_or(usize::MAX),
        display_name: find("DISPLAY_NAME").unwrap_or(usize::MAX),
        lot_size: find("LOT_SIZE").unwrap_or(usize::MAX),
        tick_size: find("TICK_SIZE").unwrap_or(usize::MAX),
        expiry_date: find("SM_EXPIRY_DATE").unwrap_or(usize::MAX),
        strike_price: find("STRIKE_PRICE").unwrap_or(usize::MAX),
        option_type: find("OPTION_TYPE").unwrap_or(usize::MAX),
        // §31.1 PRIMARY join key — OPTIONAL (absent on the Compact CSV).
        isin: find("ISIN").unwrap_or(usize::MAX),
    })
}

/// Read an optional string cell — empty when the column is absent.
/// Sanitized against ILP/BiDi per the audit-string rule.
fn opt_str(record: &csv::StringRecord, idx: usize) -> String {
    if idx == usize::MAX {
        return String::new();
    }
    sanitize_audit_string(record.get(idx).unwrap_or("").trim())
}

/// Read an optional `i32` cell — 0 when absent/blank/unparseable
/// (best-effort; these fields never reject a row).
fn opt_i32(record: &csv::StringRecord, idx: usize) -> i32 {
    if idx == usize::MAX {
        return 0;
    }
    record
        .get(idx)
        .unwrap_or("")
        .trim()
        .parse::<i32>()
        .unwrap_or(0)
}

/// Read an optional `f64` cell — 0.0 when absent/blank/unparseable.
fn opt_f64(record: &csv::StringRecord, idx: usize) -> f64 {
    if idx == usize::MAX {
        return 0.0;
    }
    record
        .get(idx)
        .unwrap_or("")
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
}

/// Map Dhan Detailed-CSV `(EXCH_ID, SEGMENT)` to the canonical
/// `ExchangeSegment` string. SEGMENT is single-char in the Detailed CSV
/// (`E`=Equity, `D`=Derivatives, `I`=Index, `C`=Currency, `M`=Commodity);
/// EXCH_ID is `NSE`/`BSE`/`MCX`. Indices share `IDX_I` across exchanges.
/// An already-canonical value (e.g. `"NSE_EQ"` from a synthetic test row)
/// or any unrecognized combo passes through verbatim. PURE.
#[must_use]
fn derive_exchange_segment(exch_id: &str, segment: &str) -> String {
    match (
        exch_id.to_ascii_uppercase().as_str(),
        segment.to_ascii_uppercase().as_str(),
    ) {
        (_, "I") => "IDX_I",
        ("NSE", "E") => "NSE_EQ",
        ("NSE", "D") => "NSE_FNO",
        ("NSE", "C") => "NSE_CURRENCY",
        ("BSE", "E") => "BSE_EQ",
        ("BSE", "D") => "BSE_FNO",
        ("BSE", "C") => "BSE_CURRENCY",
        ("MCX", "M") => "MCX_COMM",
        // Already-canonical (contains '_') or unknown: pass through verbatim.
        _ => segment,
    }
    .to_string()
}

/// Extract one validated CSV row. Returns `None` if any mandatory
/// field is empty or if a derivative row has no underlying.
fn extract_row(record: &csv::StringRecord, idx: &ColumnIndices) -> Option<CsvRow> {
    let security_id = record.get(idx.security_id)?.trim();
    let exch_id = record.get(idx.exch_id)?.trim();
    let segment = record.get(idx.segment)?.trim();
    let instrument = record.get(idx.instrument)?.trim();
    let symbol_name = record.get(idx.symbol_name)?.trim();
    let underlying_security_id = if idx.underlying_security_id == usize::MAX {
        ""
    } else {
        record.get(idx.underlying_security_id).unwrap_or("").trim()
    };

    // All five common mandatory fields must be present + non-empty.
    if security_id.is_empty()
        || exch_id.is_empty()
        || segment.is_empty()
        || instrument.is_empty()
        || symbol_name.is_empty()
    {
        return None;
    }

    // Derivative rows must additionally have a non-empty underlying.
    if is_derivative_instrument(instrument) && underlying_security_id.is_empty() {
        return None;
    }

    // Sanitize symbol_name against BiDi override characters + control
    // codes + ILP-injection commas/equals signs per §26 + the existing
    // sanitize_audit_string rule.
    let symbol_name_clean = sanitize_audit_string(symbol_name);

    Some(CsvRow {
        security_id: security_id.to_string(),
        exch_id: exch_id.to_string(),
        // Bug B (2026-05-28): Dhan Detailed CSV SEGMENT is a single-char code
        // (E/D/I/C/M per docs/dhan-ref/09-instrument-master.md:37); downstream
        // daily-universe code matches canonical strings ("NSE_EQ"/"IDX_I"/
        // "NSE_FNO"). Derive the canonical form so the whole chain works on the
        // real CSV. (Already-canonical/synthetic values pass through verbatim.)
        segment: derive_exchange_segment(exch_id, segment),
        instrument: instrument.to_string(),
        symbol_name: symbol_name_clean,
        underlying_security_id: underlying_security_id.to_string(),
        // Optional detail columns — best-effort, never reject the row.
        underlying_symbol: opt_str(record, idx.underlying_symbol),
        display_name: opt_str(record, idx.display_name),
        lot_size: opt_i32(record, idx.lot_size),
        tick_size: opt_f64(record, idx.tick_size),
        expiry_date: opt_str(record, idx.expiry_date),
        strike_price: opt_f64(record, idx.strike_price),
        option_type: opt_str(record, idx.option_type),
        isin: opt_str(record, idx.isin),
    })
}

#[must_use]
fn is_derivative_instrument(instrument: &str) -> bool {
    DERIVATIVE_INSTRUMENT_PREFIXES
        .iter()
        .any(|p| instrument.eq_ignore_ascii_case(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_HEADER: &str =
        "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID";

    fn make_csv(header: &str, rows: &[&str]) -> Vec<u8> {
        let mut buf = String::from(header);
        for row in rows {
            buf.push('\n');
            buf.push_str(row);
        }
        buf.into_bytes()
    }

    /// Detailed header with the optional detail columns appended.
    const DETAILED_HEADER: &str = "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,\
         UNDERLYING_SECURITY_ID,UNDERLYING_SYMBOL,DISPLAY_NAME,LOT_SIZE,TICK_SIZE,\
         SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE";

    #[test]
    fn minimal_csv_defaults_detail_columns_when_absent() {
        // The Compact/minimal header has none of the detail columns —
        // they must default to empty/0 (parser still works, no reject).
        let bytes = make_csv(VALID_HEADER, &["13,NSE,IDX_I,INDEX,NIFTY,"]);
        let rows = parse_detailed_csv(&bytes).expect("parse");
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.underlying_symbol, "");
        assert_eq!(r.display_name, "");
        assert_eq!(r.lot_size, 0);
        assert!((r.tick_size - 0.0).abs() < f64::EPSILON);
        assert_eq!(r.expiry_date, "");
        assert!((r.strike_price - 0.0).abs() < f64::EPSILON);
        assert_eq!(r.option_type, "");
    }

    #[test]
    fn detailed_csv_parses_option_detail_columns() {
        let bytes = make_csv(
            DETAILED_HEADER,
            &[
                "43581,NSE,NSE_FNO,OPTSTK,TCS-25Dec-4000-CE,11536,TCS,TCS 25 DEC 4000 CALL,\
               175,0.05,2025-12-25,4000.5,CE",
            ],
        );
        let rows = parse_detailed_csv(&bytes).expect("parse");
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.underlying_symbol, "TCS");
        assert_eq!(r.display_name, "TCS 25 DEC 4000 CALL");
        assert_eq!(r.lot_size, 175);
        assert!((r.tick_size - 0.05).abs() < f64::EPSILON);
        assert_eq!(r.expiry_date, "2025-12-25");
        assert!((r.strike_price - 4000.5).abs() < f64::EPSILON);
        assert_eq!(r.option_type, "CE");
    }

    #[test]
    fn detail_columns_blank_or_unparseable_default_safely() {
        // An index row in the Detailed schema: detail cells blank /
        // non-numeric must default to empty/0, never reject the row.
        let bytes = make_csv(
            DETAILED_HEADER,
            &["13,NSE,IDX_I,INDEX,NIFTY,,,NIFTY 50,,notanumber,,,"],
        );
        let rows = parse_detailed_csv(&bytes).expect("parse");
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.display_name, "NIFTY 50");
        assert_eq!(r.lot_size, 0, "blank LOT_SIZE → 0");
        assert!(
            (r.tick_size - 0.0).abs() < f64::EPSILON,
            "non-numeric TICK_SIZE → 0.0"
        );
        assert_eq!(r.option_type, "");
    }

    #[test]
    fn detail_string_columns_are_sanitized() {
        // A BiDi/ILP-injection attempt in DISPLAY_NAME must be neutralized
        // (same sanitize_audit_string path as symbol_name).
        let bytes = make_csv(
            DETAILED_HEADER,
            &["13,NSE,IDX_I,INDEX,NIFTY,,,'); DROP TABLE x;--,0,0.05,,0,"],
        );
        let rows = parse_detailed_csv(&bytes).expect("parse");
        assert_eq!(rows.len(), 1);
        assert!(
            !rows[0].display_name.contains("DROP TABLE x;"),
            "display_name injection must be neutralized: {}",
            rows[0].display_name
        );
    }

    #[test]
    fn parses_a_valid_minimal_csv() {
        let bytes = make_csv(VALID_HEADER, &["13,NSE,IDX_I,INDEX,NIFTY,"]);
        let rows = parse_detailed_csv(&bytes).expect("parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].security_id, "13");
        assert_eq!(rows[0].symbol_name, "NIFTY");
    }

    #[test]
    fn strips_utf8_bom_silently() {
        let mut bytes = Vec::from(UTF8_BOM);
        bytes.extend_from_slice(VALID_HEADER.as_bytes());
        bytes.extend_from_slice(b"\n13,NSE,IDX_I,INDEX,NIFTY,");
        let rows = parse_detailed_csv(&bytes).expect("parse with BOM");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol_name, "NIFTY");
    }

    #[test]
    fn rejects_non_utf8_iso_8859_1_bytes() {
        // 0xC9 in ISO-8859-1 is É (E-acute) but is not valid UTF-8 by
        // itself (it's a leading byte of a 2-byte sequence; the
        // following byte must be 0x80-0xBF, not 0x6E).
        let mut bytes = Vec::from(VALID_HEADER);
        bytes.extend_from_slice(b"\n13,NSE,IDX_I,INDEX,");
        bytes.push(0xC9);
        bytes.push(0x6E); // 'n'
        bytes.extend_from_slice(b",");
        let result = parse_detailed_csv(&bytes);
        assert!(matches!(result, Err(CsvParseError::NotUtf8)));
    }

    #[test]
    fn handles_quoted_field_with_embedded_comma() {
        // Some Dhan symbols have parens / commas in display name (e.g.
        // "RELIANCE,IND"). The csv crate handles quoted commas
        // correctly when Trim::All is set.
        let bytes = make_csv(VALID_HEADER, &[r#"2885,NSE,NSE_EQ,EQUITY,"RELIANCE,IND","#]);
        let rows = parse_detailed_csv(&bytes).expect("parse");
        // `sanitize_audit_string` strips ILP-injection chars (=, \n,
        // \r, control codes) but KEEPS comma — comma is allowed in
        // symbol display names.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol_name, "RELIANCE,IND");
    }

    #[test]
    fn rejects_csv_missing_header_column() {
        // Header missing INSTRUMENT column.
        let bytes = make_csv(
            "SECURITY_ID,EXCH_ID,SEGMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID",
            &["13,NSE,IDX_I,NIFTY,"],
        );
        let result = parse_detailed_csv(&bytes);
        assert!(matches!(
            result,
            Err(CsvParseError::MissingHeaderColumn { ref column })
                if column == "INSTRUMENT"
        ));
    }

    #[test]
    fn drops_single_row_with_empty_mandatory_field_below_threshold() {
        // 1 bad row out of 1000 = 0.1% which is AT the threshold, not above.
        // We need 1 bad row out of 2 = 50% to exceed threshold — too high.
        // Use 1 bad row out of 1001 to stay below.
        let mut rows: Vec<String> = Vec::new();
        for i in 0..1000 {
            rows.push(format!("{i},NSE,IDX_I,INDEX,SYM{i},"));
        }
        // One bad row with empty SYMBOL_NAME.
        rows.push("9999,NSE,IDX_I,INDEX,,".to_string());
        let row_refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let bytes = make_csv(VALID_HEADER, &row_refs);
        let parsed = parse_detailed_csv(&bytes).expect("parse below threshold");
        assert_eq!(parsed.len(), 1000, "1 bad row dropped, 1000 kept");
    }

    #[test]
    fn rejects_whole_csv_above_failure_threshold() {
        // 100 rows, 50 bad = 50% failure (>>0.1%).
        let mut rows: Vec<String> = Vec::new();
        for i in 0..50 {
            rows.push(format!("{i},NSE,IDX_I,INDEX,SYM{i},"));
        }
        for _ in 0..50 {
            rows.push(",NSE,IDX_I,INDEX,,".to_string()); // all empty
        }
        let row_refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let bytes = make_csv(VALID_HEADER, &row_refs);
        let result = parse_detailed_csv(&bytes);
        assert!(matches!(
            result,
            Err(CsvParseError::RowFailureThresholdExceeded {
                failed_rows: 50,
                total_rows: 100,
                ..
            })
        ));
    }

    #[test]
    fn rejects_empty_csv() {
        let bytes = VALID_HEADER.as_bytes().to_vec(); // header only, no rows
        let result = parse_detailed_csv(&bytes);
        assert!(matches!(result, Err(CsvParseError::Empty)));
    }

    #[test]
    fn derivative_row_without_underlying_is_dropped() {
        // Build 1001 rows: 1000 valid non-derivatives + 1 derivative
        // with empty underlying. Failure rate = 1/1001 ≈ 0.0999% which
        // is BELOW the 0.1% threshold, so the bad row is dropped
        // (not rejection of the whole CSV).
        let mut rows: Vec<String> = (0..1000)
            .map(|i| format!("{i},NSE,IDX_I,INDEX,SYM{i},"))
            .collect();
        rows.push("99999,NSE,NSE_FNO,FUTIDX,NIFTY26JUNFUT,".to_string());
        let row_refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let bytes = make_csv(VALID_HEADER, &row_refs);
        let parsed = parse_detailed_csv(&bytes).expect("parse");
        assert_eq!(
            parsed.len(),
            1000,
            "derivative-without-underlying must be dropped, 1000 valid kept"
        );
        // Verify the dropped row was the FUTIDX one — none of the
        // kept rows have instrument FUTIDX.
        assert!(parsed.iter().all(|r| r.instrument == "INDEX"));
    }

    #[test]
    fn derivative_row_without_underlying_blows_threshold_with_only_bad_rows() {
        // 2-row CSV with 1 bad derivative = 50% failure → rejection.
        let bytes = make_csv(
            VALID_HEADER,
            &[
                "100,NSE,NSE_FNO,FUTIDX,NIFTY26JUNFUT,",
                "13,NSE,IDX_I,INDEX,NIFTY,",
            ],
        );
        let result = parse_detailed_csv(&bytes);
        assert!(matches!(
            result,
            Err(CsvParseError::RowFailureThresholdExceeded { .. })
        ));
    }

    #[test]
    fn derivative_row_with_underlying_is_kept() {
        let bytes = make_csv(
            VALID_HEADER,
            &["38919,NSE,NSE_FNO,FUTSTK,RELIANCE26JUNFUT,2885"],
        );
        let rows = parse_detailed_csv(&bytes).expect("parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].underlying_security_id, "2885");
        assert_eq!(rows[0].instrument, "FUTSTK");
    }

    #[test]
    fn handles_mixed_line_endings() {
        // CRLF + LF + bare CR mix — csv crate normalizes these.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(VALID_HEADER.as_bytes());
        bytes.extend_from_slice(b"\r\n13,NSE,IDX_I,INDEX,NIFTY,");
        bytes.extend_from_slice(b"\n25,NSE,IDX_I,INDEX,BANKNIFTY,");
        bytes.extend_from_slice(b"\r\n51,BSE,IDX_I,INDEX,SENSEX,");
        let rows = parse_detailed_csv(&bytes).expect("parse mixed line endings");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].symbol_name, "NIFTY");
        assert_eq!(rows[1].symbol_name, "BANKNIFTY");
        assert_eq!(rows[2].symbol_name, "SENSEX");
    }

    #[test]
    fn case_insensitive_header_column_match() {
        // Some CSV exporters lowercase headers. The parser must match
        // case-insensitively against the locked column names.
        let bytes = make_csv(
            "security_id,exch_id,segment,instrument,symbol_name,underlying_security_id",
            &["13,NSE,IDX_I,INDEX,NIFTY,"],
        );
        let rows = parse_detailed_csv(&bytes).expect("parse lowercase header");
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn underlying_security_id_column_optional() {
        // CSV without UNDERLYING_SECURITY_ID column — non-derivative
        // rows still parse fine, derivative rows are rejected since
        // their underlying is implicitly empty.
        let bytes = make_csv(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME",
            &["13,NSE,IDX_I,INDEX,NIFTY"],
        );
        let rows = parse_detailed_csv(&bytes).expect("parse without UNDERLYING column");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].underlying_security_id, "");
    }

    #[test]
    fn is_derivative_instrument_matches_all_8_prefixes() {
        for p in DERIVATIVE_INSTRUMENT_PREFIXES {
            assert!(is_derivative_instrument(p), "expected derivative: {p}");
        }
        assert!(!is_derivative_instrument("INDEX"));
        assert!(!is_derivative_instrument("EQUITY"));
        assert!(!is_derivative_instrument(""));
    }

    #[test]
    fn symbol_name_strips_bidi_override_characters() {
        // U+202E RIGHT-TO-LEFT OVERRIDE in symbol name (potential
        // display-spoofing attack vector).
        let bytes = format!("{VALID_HEADER}\n13,NSE,IDX_I,INDEX,NI\u{202e}FTY,").into_bytes();
        let rows = parse_detailed_csv(&bytes).expect("parse");
        assert!(
            !rows[0].symbol_name.contains('\u{202e}'),
            "BiDi override must be stripped, got {:?}",
            rows[0].symbol_name
        );
    }

    #[test]
    fn threshold_constant_is_one_per_thousand() {
        assert!((MANDATORY_FIELD_REJECT_THRESHOLD - 0.001).abs() < 1e-9);
    }
}
