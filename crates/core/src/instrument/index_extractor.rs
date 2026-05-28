//! Sub-PR #6 of 2026-05-27 daily-universe expansion — extract every
//! `IDX_I` index row from the parsed Dhan Detailed CSV per the §2
//! universe scope (all NSE indices + 1 BSE SENSEX).
//!
//! **Feature-gated.** This module compiles only when the
//! `daily_universe_fetcher` cargo feature is enabled (per §21 of the
//! authoritative rule file). Under the current `Indices4Only` default
//! scope this module is dead code.
//!
//! ## Contract (§2 of the rule file)
//!
//! From §2 — universe size envelope:
//!
//! > "all `IDX_I` rows where `EXCH_ID IN (NSE, BSE)` AND
//! >  `INSTRUMENT == INDEX` (~30)"
//!
//! Input: validated `&[CsvRow]` from Sub-PR #4's parser.
//!
//! Output (on success): `IndexExtraction` containing:
//! - `nse_indices` — every NSE `IDX_I` `INDEX` row (NIFTY, BANKNIFTY,
//!   FINNIFTY, MIDCPNIFTY, INDIA VIX, sectoral indices...)
//! - `bse_sensex` — the single BSE SENSEX `IDX_I` `INDEX` row
//! - `total_index_count` = `nse_indices.len() + 1` (~30 today)
//!
//! ## What this module does NOT do
//!
//! - F&O underlying extraction (NSE_EQ stocks) — Sub-PR #5 already
//!   shipped that
//! - Daily universe builder combining indices + underlyings — Sub-PR #7
//! - Persistence to `instrument_lifecycle` table — Sub-PR #9

#![cfg(feature = "daily_universe_fetcher")]

use thiserror::Error;

use super::csv_parser::CsvRow;

/// IDX_I segment string (per Dhan ExchangeSegment enum) — every index
/// row in the Detailed CSV lives in this segment.
const IDX_I_SEGMENT: &str = "IDX_I";

/// INSTRUMENT value for index rows. Filters out any non-index rows
/// that share the IDX_I segment.
const INSTRUMENT_INDEX: &str = "INDEX";

/// Exchange ID for the NSE-side indices.
const EXCH_ID_NSE: &str = "NSE";

/// Exchange ID for the BSE-side SENSEX.
const EXCH_ID_BSE: &str = "BSE";

/// Symbol name of the one BSE index that is in scope per the operator
/// directive 2026-05-27 quote 2: "all nse indices along with one
/// sensex bse index also needed dude entirely".
const BSE_SENSEX_SYMBOL: &str = "SENSEX";

/// Result of the index extraction pass.
#[derive(Debug, Clone)]
pub struct IndexExtraction {
    /// Every NSE `IDX_I` `INDEX` row (`NIFTY`, `BANKNIFTY`, `FINNIFTY`,
    /// `INDIA VIX`, sectoral indices, etc.).
    pub nse_indices: Vec<CsvRow>,

    /// The single BSE SENSEX row. `None` if the CSV does not contain
    /// one — boot HALTS in that case per the operator's §0 quote 2.
    pub bse_sensex: Option<CsvRow>,
}

impl IndexExtraction {
    /// Total index count = NSE indices + 1 if BSE SENSEX present.
    /// Used by the §2 + §22 universe-size envelope check.
    #[must_use]
    pub fn total_index_count(&self) -> usize {
        self.nse_indices.len() + usize::from(self.bse_sensex.is_some())
    }
}

/// Errors that can occur during index extraction.
#[derive(Debug, Error)]
pub enum IndexExtractError {
    /// No NSE `IDX_I` rows found. Likely a parser bug or the upstream
    /// CSV is incomplete — boot HALTS per §1.
    #[error("no NSE IDX_I INDEX rows found in {total_rows} input rows")]
    NoNseIndicesFound { total_rows: usize },

    /// BSE SENSEX row is missing from the CSV. Per operator quote 2 of
    /// the rule file §0 this row MUST be present; boot HALTS otherwise.
    #[error("BSE SENSEX IDX_I row missing — operator-locked §0 quote 2")]
    BseSensexMissing,
}

/// Extract every `IDX_I` index row from the parsed CSV per §2.
///
/// O(N) over `rows` — single pass, no allocation beyond the result
/// `Vec`. For ~25K Dhan Detailed-CSV rows the call completes in well
/// under 1ms (cold-path boot budget).
///
/// # Errors
///
/// See [`IndexExtractError`] variants. Both are fail-closed per the
/// rule file §1 / §0: the orchestrator (Sub-PR #10) surfaces them as
/// CRITICAL Telegram events + HALT boot.
pub fn extract_indices(rows: &[CsvRow]) -> Result<IndexExtraction, IndexExtractError> {
    let mut nse_indices: Vec<CsvRow> = Vec::new();
    let mut bse_sensex: Option<CsvRow> = None;

    for row in rows {
        if !is_idx_i_index(&row.segment, &row.instrument) {
            continue;
        }

        if row.exch_id.eq_ignore_ascii_case(EXCH_ID_NSE) {
            nse_indices.push(row.clone());
        } else if row.exch_id.eq_ignore_ascii_case(EXCH_ID_BSE) && is_bse_sensex(&row.symbol_name) {
            // Operator-locked: only the SINGLE BSE SENSEX row is in
            // scope, not other BSE indices.
            bse_sensex = Some(row.clone());
        }
        // Any other (segment=IDX_I, exch=BSE, symbol != SENSEX) row is
        // silently ignored per §2 universe scope.
    }

    if nse_indices.is_empty() {
        return Err(IndexExtractError::NoNseIndicesFound {
            total_rows: rows.len(),
        });
    }

    if bse_sensex.is_none() {
        return Err(IndexExtractError::BseSensexMissing);
    }

    Ok(IndexExtraction {
        nse_indices,
        bse_sensex,
    })
}

#[inline]
#[must_use]
fn is_idx_i_index(segment: &str, instrument: &str) -> bool {
    segment.eq_ignore_ascii_case(IDX_I_SEGMENT) && instrument.eq_ignore_ascii_case(INSTRUMENT_INDEX)
}

#[inline]
#[must_use]
fn is_bse_sensex(symbol_name: &str) -> bool {
    symbol_name.eq_ignore_ascii_case(BSE_SENSEX_SYMBOL)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx_i_row(security_id: &str, exch_id: &str, instrument: &str, symbol: &str) -> CsvRow {
        CsvRow {
            security_id: security_id.to_string(),
            exch_id: exch_id.to_string(),
            segment: "IDX_I".to_string(),
            instrument: instrument.to_string(),
            symbol_name: symbol.to_string(),
            underlying_security_id: String::new(),
            ..Default::default()
        }
    }

    fn nse_eq_row(security_id: &str, symbol: &str) -> CsvRow {
        CsvRow {
            security_id: security_id.to_string(),
            exch_id: "NSE".to_string(),
            segment: "NSE_EQ".to_string(),
            instrument: "EQUITY".to_string(),
            symbol_name: symbol.to_string(),
            underlying_security_id: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn extracts_all_nse_idx_i_indices() {
        let rows = vec![
            idx_i_row("13", "NSE", "INDEX", "NIFTY"),
            idx_i_row("25", "NSE", "INDEX", "BANKNIFTY"),
            idx_i_row("21", "NSE", "INDEX", "INDIA VIX"),
            idx_i_row("27", "NSE", "INDEX", "FINNIFTY"),
            idx_i_row("51", "BSE", "INDEX", "SENSEX"),
        ];
        let result = extract_indices(&rows).expect("extract");
        assert_eq!(result.nse_indices.len(), 4);
        assert!(result.bse_sensex.is_some());
        assert_eq!(result.total_index_count(), 5);
    }

    #[test]
    fn includes_bse_sensex_when_present() {
        let rows = vec![
            idx_i_row("13", "NSE", "INDEX", "NIFTY"),
            idx_i_row("51", "BSE", "INDEX", "SENSEX"),
        ];
        let result = extract_indices(&rows).expect("extract");
        let sensex = result.bse_sensex.expect("SENSEX present");
        assert_eq!(sensex.symbol_name, "SENSEX");
        assert_eq!(sensex.exch_id, "BSE");
    }

    #[test]
    fn rejects_when_bse_sensex_missing() {
        // Operator quote 2 requires the BSE SENSEX row — its absence
        // is a HALT condition.
        let rows = vec![idx_i_row("13", "NSE", "INDEX", "NIFTY")];
        let result = extract_indices(&rows);
        assert!(matches!(result, Err(IndexExtractError::BseSensexMissing)));
    }

    #[test]
    fn rejects_when_no_nse_idx_i_rows_present() {
        // Only BSE SENSEX, no NSE indices — defensive parser-bug check.
        let rows = vec![idx_i_row("51", "BSE", "INDEX", "SENSEX")];
        let result = extract_indices(&rows);
        assert!(matches!(
            result,
            Err(IndexExtractError::NoNseIndicesFound { total_rows: 1 })
        ));
    }

    #[test]
    fn ignores_non_sensex_bse_indices() {
        // BSE has many indices (BANKEX, AUTO, IT, FMCG, etc.) — per
        // operator §0 quote 2, ONLY SENSEX is in scope. The rest are
        // silently ignored.
        let rows = vec![
            idx_i_row("13", "NSE", "INDEX", "NIFTY"),
            idx_i_row("51", "BSE", "INDEX", "SENSEX"),
            idx_i_row("52", "BSE", "INDEX", "BANKEX"),
            idx_i_row("53", "BSE", "INDEX", "BSE100"),
        ];
        let result = extract_indices(&rows).expect("extract");
        assert_eq!(result.nse_indices.len(), 1);
        assert!(result.bse_sensex.is_some());
        // Only NIFTY and SENSEX present → total = 2 (BANKEX + BSE100
        // explicitly dropped).
        assert_eq!(result.total_index_count(), 2);
    }

    #[test]
    fn ignores_non_index_segments() {
        let rows = vec![
            idx_i_row("13", "NSE", "INDEX", "NIFTY"),
            idx_i_row("51", "BSE", "INDEX", "SENSEX"),
            nse_eq_row("2885", "RELIANCE"),
            CsvRow {
                security_id: "39001".to_string(),
                exch_id: "NSE".to_string(),
                segment: "NSE_FNO".to_string(),
                instrument: "FUTSTK".to_string(),
                symbol_name: "RELIANCE26JUNFUT".to_string(),
                underlying_security_id: "2885".to_string(),
                ..Default::default()
            },
        ];
        let result = extract_indices(&rows).expect("extract");
        assert_eq!(result.nse_indices.len(), 1, "only NIFTY counted");
        assert_eq!(result.total_index_count(), 2);
    }

    #[test]
    fn ignores_idx_i_rows_with_non_index_instrument() {
        // Some CSV rows might share segment=IDX_I but have
        // instrument != INDEX (e.g. an FX rate row). They must NOT be
        // included in the index extraction.
        let rows = vec![
            idx_i_row("13", "NSE", "INDEX", "NIFTY"),
            idx_i_row("99", "NSE", "FX_RATE", "USDINR"),
            idx_i_row("51", "BSE", "INDEX", "SENSEX"),
        ];
        let result = extract_indices(&rows).expect("extract");
        assert_eq!(result.nse_indices.len(), 1);
        assert_eq!(result.total_index_count(), 2);
    }

    #[test]
    fn case_insensitive_match_for_segment_exch_instrument_symbol() {
        let rows = vec![
            CsvRow {
                security_id: "13".to_string(),
                exch_id: "nse".to_string(), // lowercase
                segment: "idx_i".to_string(),
                instrument: "index".to_string(),
                symbol_name: "NIFTY".to_string(),
                underlying_security_id: String::new(),
                ..Default::default()
            },
            CsvRow {
                security_id: "51".to_string(),
                exch_id: "bse".to_string(),
                segment: "idx_i".to_string(),
                instrument: "index".to_string(),
                symbol_name: "sensex".to_string(), // lowercase
                underlying_security_id: String::new(),
                ..Default::default()
            },
        ];
        let result = extract_indices(&rows).expect("extract");
        assert_eq!(result.nse_indices.len(), 1);
        assert!(result.bse_sensex.is_some());
    }

    #[test]
    fn empty_rows_returns_no_nse_indices_error() {
        let result = extract_indices(&[]);
        assert!(matches!(
            result,
            Err(IndexExtractError::NoNseIndicesFound { total_rows: 0 })
        ));
    }

    #[test]
    fn is_idx_i_index_helper_matches_only_idx_i_index() {
        assert!(is_idx_i_index("IDX_I", "INDEX"));
        assert!(is_idx_i_index("idx_i", "index"));
        assert!(!is_idx_i_index("NSE_EQ", "INDEX"));
        assert!(!is_idx_i_index("IDX_I", "EQUITY"));
        assert!(!is_idx_i_index("IDX_I", ""));
        assert!(!is_idx_i_index("", "INDEX"));
    }

    #[test]
    fn is_bse_sensex_helper_matches_only_sensex() {
        assert!(is_bse_sensex("SENSEX"));
        assert!(is_bse_sensex("sensex"));
        assert!(is_bse_sensex("Sensex"));
        assert!(!is_bse_sensex("SENSEX50"));
        assert!(!is_bse_sensex("BANKEX"));
        assert!(!is_bse_sensex(""));
    }

    #[test]
    fn total_index_count_is_zero_when_default() {
        let extraction = IndexExtraction {
            nse_indices: Vec::new(),
            bse_sensex: None,
        };
        assert_eq!(extraction.total_index_count(), 0);
    }

    #[test]
    fn realistic_universe_count_matches_30() {
        // Per §2 the expected count is "~30 NSE indices + 1 BSE SENSEX".
        // Build a 30-index NSE set + SENSEX and verify the count.
        let mut rows = Vec::new();
        for i in 0..30 {
            rows.push(idx_i_row(
                &format!("{i}"),
                "NSE",
                "INDEX",
                &format!("INDEX{i}"),
            ));
        }
        rows.push(idx_i_row("51", "BSE", "INDEX", "SENSEX"));
        let result = extract_indices(&rows).expect("extract");
        assert_eq!(result.nse_indices.len(), 30);
        assert_eq!(result.total_index_count(), 31);
    }
}
