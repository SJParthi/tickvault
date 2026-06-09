//! NTM Sub-PR #3 — robust parser for a niftyindices.com constituent CSV.
//!
//! **Feature-gated** behind `daily_universe_fetcher` (same lifecycle as
//! the downloader). Pure function: `(raw_bytes, index_name, today) ->
//! Result<Vec<IndexConstituent>, ConstituencyParseError>`. No network,
//! no I/O — fully unit-testable from in-memory fixtures.
//!
//! ## Expected columns (niftyindices `ind_*_list.csv`)
//!
//! `Company Name, Industry, Symbol, Series, ISIN Code`
//!
//! `Symbol`, `ISIN Code`, and `Industry` are stored on each row;
//! `Series` is used to KEEP ONLY cash-equity (`EQ`) rows per §31.1 (when
//! the column is present). The constituent list carries NO weight column,
//! so `weight = 0.0`.
//!
//! ## Robustness (mirrors §26 of the rule file)
//!
//! - Strip a UTF-8 BOM.
//! - Reject non-UTF-8 bodies (no lossy parse).
//! - `csv` crate with `Trim::All` + `flexible(true)` handles quoted
//!   commas + ragged trailing disclaimer rows.
//! - Skip rows with an empty `Symbol` (blank / disclaimer lines).
//! - Normalize `Symbol`/`ISIN` (trim + uppercase).
//! - Reject if ZERO valid constituents survive (fail-closed — an empty
//!   parse must never look like success).

#![cfg(feature = "daily_universe_fetcher")]

use chrono::NaiveDate;
use thiserror::Error;
use tickvault_common::instrument_types::IndexConstituent;
use tickvault_common::sanitize::sanitize_audit_string;

/// Errors from parsing one constituent CSV.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConstituencyParseError {
    /// Body was not valid UTF-8 (reject — no lossy decode).
    #[error("constituent CSV for {index_name} is not valid UTF-8")]
    NotUtf8 { index_name: String },

    /// A required header column was absent.
    #[error("constituent CSV for {index_name} missing column: {column}")]
    MissingColumn {
        index_name: String,
        column: &'static str,
    },

    /// Zero valid constituents after parsing (empty/garbage body).
    #[error("constituent CSV for {index_name} yielded zero valid rows")]
    NoConstituents { index_name: String },
}

/// Result of a parse, carrying diagnostics alongside the rows.
#[derive(Debug, Clone)]
pub struct ParsedConstituents {
    /// The valid constituent rows.
    pub constituents: Vec<IndexConstituent>,
    /// Count of rows skipped because `Symbol` was empty (disclaimer/blank).
    pub skipped_blank_rows: usize,
    /// Count of kept rows whose `ISIN Code` was empty (Sub-PR #4 falls
    /// back to symbol+series for these).
    pub missing_isin: usize,
    /// Count of rows dropped because `Series` was present and != `EQ`
    /// (§31.1 keeps only cash-equity constituents). Zero when the CSV
    /// has no `Series` column.
    pub skipped_non_eq: usize,
}

const COL_SYMBOL: &str = "Symbol";
const COL_ISIN: &str = "ISIN Code";
const COL_INDUSTRY: &str = "Industry";
const COL_SERIES: &str = "Series";
/// §31.1 cross-check key keeps only the cash-equity series.
const SERIES_EQ: &str = "EQ";

/// Strip a leading UTF-8 BOM if present.
fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// Find a header column index by exact (case-insensitive, trimmed) name.
fn find_col(headers: &csv::StringRecord, name: &str) -> Option<usize> {
    headers
        .iter()
        .position(|h| h.trim().eq_ignore_ascii_case(name))
}

/// Parse a niftyindices constituent CSV into `IndexConstituent` rows.
///
/// `index_name` is the human label (e.g. `"Nifty Total Market"`) stamped
/// on every constituent; `today` is the IST trading date for `last_updated`.
///
/// # Errors
///
/// See [`ConstituencyParseError`].
pub fn parse_constituents(
    raw: &[u8],
    index_name: &str,
    today: NaiveDate,
) -> Result<ParsedConstituents, ConstituencyParseError> {
    let text =
        std::str::from_utf8(strip_bom(raw)).map_err(|_| ConstituencyParseError::NotUtf8 {
            index_name: index_name.to_string(),
        })?;

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());

    let headers = reader
        .headers()
        .map_err(|_| ConstituencyParseError::MissingColumn {
            index_name: index_name.to_string(),
            column: COL_SYMBOL,
        })?
        .clone();

    let symbol_idx =
        find_col(&headers, COL_SYMBOL).ok_or_else(|| ConstituencyParseError::MissingColumn {
            index_name: index_name.to_string(),
            column: COL_SYMBOL,
        })?;
    let isin_idx =
        find_col(&headers, COL_ISIN).ok_or_else(|| ConstituencyParseError::MissingColumn {
            index_name: index_name.to_string(),
            column: COL_ISIN,
        })?;
    // Industry is optional — absence is tolerated (sector becomes empty).
    let industry_idx = find_col(&headers, COL_INDUSTRY);
    // Series is optional — when present we keep only `EQ` (§31.1).
    let series_idx = find_col(&headers, COL_SERIES);

    let mut constituents = Vec::new();
    let mut skipped_blank_rows = 0usize;
    let mut missing_isin = 0usize;
    let mut skipped_non_eq = 0usize;

    for record in reader.records() {
        // Per-row visibility: a malformed record (e.g. unterminated quote)
        // is logged + skipped, never silently dropped.
        let record = match record {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(index = index_name, %err, "constituency CSV record skipped (malformed)");
                continue;
            }
        };
        // Sanitize every untrusted field (strip control / BiDi / SQL tokens
        // per §26) — these flow into logs + QuestDB ILP in Sub-PR #6/#10.
        let symbol = record
            .get(symbol_idx)
            .map(|s| sanitize_audit_string(s.trim()).to_uppercase())
            .unwrap_or_default();
        // Skip blank / disclaimer rows (no symbol).
        if symbol.is_empty() {
            skipped_blank_rows += 1;
            continue;
        }
        // §31.1: keep only cash-equity (`Series == EQ`) rows when the
        // column exists. Absent column → keep all (don't over-filter).
        if let Some(series_idx) = series_idx {
            let series = record.get(series_idx).map(str::trim).unwrap_or_default();
            if !series.is_empty() && !series.eq_ignore_ascii_case(SERIES_EQ) {
                skipped_non_eq += 1;
                continue;
            }
        }
        let isin = record
            .get(isin_idx)
            .map(|s| sanitize_audit_string(s.trim()).to_uppercase())
            .unwrap_or_default();
        if isin.is_empty() {
            missing_isin += 1;
        }
        let sector = industry_idx
            .and_then(|i| record.get(i))
            .map(|s| sanitize_audit_string(s.trim()))
            .unwrap_or_default();

        constituents.push(IndexConstituent {
            index_name: index_name.to_string(),
            symbol,
            isin,
            weight: 0.0,
            sector,
            last_updated: today,
        });
    }

    if constituents.is_empty() {
        return Err(ConstituencyParseError::NoConstituents {
            index_name: index_name.to_string(),
        });
    }

    Ok(ParsedConstituents {
        constituents,
        skipped_blank_rows,
        missing_isin,
        skipped_non_eq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 6).unwrap()
    }

    const SAMPLE: &str = "Company Name,Industry,Symbol,Series,ISIN Code\n\
        \"Reliance Industries Ltd.\",\"Oil Gas & Consumable Fuels\",RELIANCE,EQ,INE002A01018\n\
        \"HDFC Bank Ltd.\",Financial Services,HDFCBANK,EQ,INE040A01034\n";

    #[test]
    fn parses_basic_two_row_csv() {
        let out = parse_constituents(SAMPLE.as_bytes(), "Nifty Total Market", today()).unwrap();
        assert_eq!(out.constituents.len(), 2);
        assert_eq!(out.constituents[0].symbol, "RELIANCE");
        assert_eq!(out.constituents[0].isin, "INE002A01018");
        assert_eq!(out.constituents[0].sector, "Oil Gas & Consumable Fuels");
        assert_eq!(out.constituents[0].index_name, "Nifty Total Market");
        assert_eq!(out.constituents[1].symbol, "HDFCBANK");
        assert_eq!(out.missing_isin, 0);
    }

    #[test]
    fn strips_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(SAMPLE.as_bytes());
        let out = parse_constituents(&bytes, "Nifty 50", today()).unwrap();
        assert_eq!(out.constituents.len(), 2);
    }

    #[test]
    fn handles_crlf_line_endings() {
        let crlf = SAMPLE.replace('\n', "\r\n");
        let out = parse_constituents(crlf.as_bytes(), "Nifty 50", today()).unwrap();
        assert_eq!(out.constituents.len(), 2);
    }

    #[test]
    fn skips_blank_and_trailing_disclaimer_rows() {
        let with_junk = format!("{SAMPLE}\n,,,,\n\"Disclaimer: data provided as-is\"\n");
        let out = parse_constituents(with_junk.as_bytes(), "Nifty 50", today()).unwrap();
        assert_eq!(out.constituents.len(), 2, "only the 2 real rows survive");
        assert!(out.skipped_blank_rows >= 1);
    }

    #[test]
    fn counts_missing_isin_but_keeps_row() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
            \"Foo Ltd.\",Tech,FOO,EQ,\n";
        let out = parse_constituents(csv.as_bytes(), "Nifty 50", today()).unwrap();
        assert_eq!(out.constituents.len(), 1);
        assert_eq!(out.missing_isin, 1);
        assert_eq!(out.constituents[0].isin, "");
    }

    #[test]
    fn filters_non_eq_series_rows() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
            \"Eq Co\",Tech,EQCO,EQ,INE001A01011\n\
            \"Be Co\",Tech,BECO,BE,INE002A01012\n";
        let out = parse_constituents(csv.as_bytes(), "Nifty 50", today()).unwrap();
        assert_eq!(out.constituents.len(), 1, "only the EQ row survives");
        assert_eq!(out.constituents[0].symbol, "EQCO");
        assert_eq!(out.skipped_non_eq, 1);
    }

    #[test]
    fn keeps_all_rows_when_no_series_column() {
        let csv = "Company Name,Industry,Symbol,ISIN Code\n\
            \"Foo\",Tech,FOO,INE001A01011\n";
        let out = parse_constituents(csv.as_bytes(), "Nifty 50", today()).unwrap();
        assert_eq!(out.constituents.len(), 1);
        assert_eq!(out.skipped_non_eq, 0);
    }

    #[test]
    fn rejects_missing_symbol_column() {
        let csv = "Company Name,Industry,Series,ISIN Code\nFoo,Tech,EQ,INE001\n";
        let err = parse_constituents(csv.as_bytes(), "Nifty 50", today()).unwrap_err();
        assert!(matches!(
            err,
            ConstituencyParseError::MissingColumn {
                column: "Symbol",
                ..
            }
        ));
    }

    #[test]
    fn rejects_missing_isin_column() {
        let csv = "Company Name,Industry,Symbol,Series\nFoo,Tech,FOO,EQ\n";
        let err = parse_constituents(csv.as_bytes(), "Nifty 50", today()).unwrap_err();
        assert!(matches!(
            err,
            ConstituencyParseError::MissingColumn {
                column: "ISIN Code",
                ..
            }
        ));
    }

    #[test]
    fn rejects_non_utf8() {
        let bad = vec![0xFF, 0xFE, 0x00, 0x01];
        let err = parse_constituents(&bad, "Nifty 50", today()).unwrap_err();
        assert!(matches!(err, ConstituencyParseError::NotUtf8 { .. }));
    }

    #[test]
    fn rejects_header_only_csv() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n";
        let err = parse_constituents(csv.as_bytes(), "Nifty 50", today()).unwrap_err();
        assert!(matches!(err, ConstituencyParseError::NoConstituents { .. }));
    }

    #[test]
    fn handles_quoted_comma_in_company_name() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
            \"Bajaj Holdings, Inc.\",Financial Services,BAJAJHLDNG,EQ,INE118A01012\n";
        let out = parse_constituents(csv.as_bytes(), "Nifty 50", today()).unwrap();
        assert_eq!(out.constituents.len(), 1);
        assert_eq!(out.constituents[0].symbol, "BAJAJHLDNG");
    }
}
