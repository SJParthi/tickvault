//! Sub-PR #5 of 2026-05-27 daily-universe expansion — extract the set
//! of unique F&O underlying SecurityIds + cross-validate the dangling-
//! reference invariant.
//!
//! **Feature-gated.** This module compiles only when the
//! `daily_universe_fetcher` cargo feature is enabled (per §21 of the
//! authoritative rule file). Under the current `Indices4Only` default
//! scope this module is dead code.
//!
//! ## Contract (§3 + §10 of the rule file)
//!
//! Input: validated `&[CsvRow]` from Sub-PR #4's parser.
//!
//! Output (on success): `FnoUnderlyingExtraction` containing:
//! - `unique_underlying_ids` — the set of every `UNDERLYING_SECURITY_ID`
//!   referenced by a derivative row (FUTSTK / OPTSTK / FUTIDX / OPTIDX).
//! - `nse_eq_lookup` — index of NSE_EQ rows keyed by `security_id`,
//!   built once for O(1) cross-validation + downstream universe build.
//! - `dangling_count` / `total_derivative_count` — instrumentation for
//!   the rule file §9 L3 RECONCILE layer.
//!
//! ## Dangling-reference invariant (§3 + §4)
//!
//! Every `UNDERLYING_SECURITY_ID` cited by a derivative row MUST exist
//! as an NSE_EQ row in the SAME CSV. Threshold:
//!
//! | Dangling % of derivatives | Action |
//! |---|---|
//! | <= `DANGLING_REFERENCE_REJECT_THRESHOLD` (0.5%) | Drop affected derivative rows + warn |
//! | > 0.5% | REJECT entire CSV |
//!
//! Per §26 the threshold is "drop the affected derivative rows + log
//! Severity::High; > 0.5% derivative rows → reject CSV". The reject
//! path bubbles up as an error so the orchestrator's §4 infinite-retry
//! loop can decide whether to retry.

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::{HashMap, HashSet};

use thiserror::Error;

use super::csv_parser::CsvRow;

/// Reject threshold for dangling-reference failures across the
/// derivative subset. Per rule file §26.
///
/// Value: 0.005 = 0.5% in decimal.
pub const DANGLING_REFERENCE_REJECT_THRESHOLD: f64 = 0.005;

/// NSE_EQ segment string (per Dhan ExchangeSegment enum). Used to
/// classify cash-equity rows for the underlying-resolution lookup.
const NSE_EQ_SEGMENT: &str = "NSE_EQ";

/// Derivative instrument prefixes that REFERENCE an NSE_EQ underlying.
/// Index derivatives (FUTIDX/OPTIDX) reference IDX_I underlyings —
/// those are handled separately by Sub-PR #6 (indices extractor).
/// Currency/commodity derivatives are out of scope per the operator-
/// locked 2-segment scope (NSE_EQ + NSE_FNO).
const STOCK_DERIVATIVE_PREFIXES: &[&str] = &["FUTSTK", "OPTSTK"];

/// Result of the F&O underlying extraction pass.
#[derive(Debug, Clone)]
pub struct FnoUnderlyingExtraction {
    /// Every UNDERLYING_SECURITY_ID referenced by a stock-derivative
    /// row (FUTSTK / OPTSTK). All values are validated to exist as an
    /// NSE_EQ row in `nse_eq_lookup`.
    pub unique_underlying_ids: HashSet<String>,

    /// NSE_EQ row index keyed by `security_id`. Built once at extraction
    /// time so downstream universe-builder layers (Sub-PR #7) can resolve
    /// each underlying SID to its full CsvRow in O(1).
    pub nse_eq_lookup: HashMap<String, CsvRow>,

    /// Count of derivative rows that had a dangling underlying (i.e.
    /// `UNDERLYING_SECURITY_ID` didn't resolve to any NSE_EQ row).
    /// `<=` threshold → these rows were dropped; `>` threshold → the
    /// extraction returned `Err(...)` instead.
    pub dangling_count: usize,

    /// Total stock-derivative rows observed. Used as the denominator
    /// for the threshold check + as observability output.
    pub total_derivative_count: usize,
}

/// Errors that can occur during extraction.
#[derive(Debug, Error)]
pub enum ExtractError {
    /// More than `DANGLING_REFERENCE_REJECT_THRESHOLD` of stock-derivative
    /// rows referenced an `UNDERLYING_SECURITY_ID` that did not exist
    /// as an NSE_EQ row in the same CSV. Per §3 + §26 of the rule file.
    #[error(
        "{dangling_count} of {total_derivative_count} stock-derivative rows ({pct:.4}%) had \
         dangling underlying refs — threshold {threshold_pct:.4}%"
    )]
    DanglingReferenceThresholdExceeded {
        dangling_count: usize,
        total_derivative_count: usize,
        pct: f64,
        threshold_pct: f64,
    },

    /// No stock-derivative rows found in the input. Likely a parser
    /// bug or upstream CSV is incomplete. Per §1 — boot HALTS rather
    /// than proceed with the wrong universe.
    #[error("no stock-derivative rows (FUTSTK / OPTSTK) found in {total_rows} input rows")]
    NoDerivativeRowsFound { total_rows: usize },
}

/// Extract the unique F&O underlying SIDs + cross-validate.
///
/// O(N) over `rows`. One pass to build the NSE_EQ index, one pass to
/// scan derivatives + check refs. Total: 2N CsvRow visits, ~50ns per
/// row for the HashMap operations. For ~25K Dhan Detailed-CSV rows
/// the call completes in ~1ms — fully within the cold-path boot budget.
///
/// # Errors
///
/// See [`ExtractError`] variants. Above threshold the entire extraction
/// is rejected — the caller (orchestrator in Sub-PR #10) is expected to
/// surface this as a CRITICAL Telegram event + halt boot per the §4
/// infinite-retry policy.
pub fn extract_fno_underlyings(rows: &[CsvRow]) -> Result<FnoUnderlyingExtraction, ExtractError> {
    // Pass 1 — build the NSE_EQ index.
    let mut nse_eq_lookup: HashMap<String, CsvRow> = HashMap::new();
    for row in rows {
        if row.segment.eq_ignore_ascii_case(NSE_EQ_SEGMENT) {
            nse_eq_lookup.insert(row.security_id.clone(), row.clone());
        }
    }

    // Pass 2 — scan stock-derivative rows, validate underlying refs.
    let mut unique_underlying_ids: HashSet<String> = HashSet::new();
    let mut dangling_count: usize = 0;
    let mut total_derivative_count: usize = 0;

    for row in rows {
        if !is_stock_derivative(&row.instrument) {
            continue;
        }
        total_derivative_count = total_derivative_count.saturating_add(1);

        let underlying_id = &row.underlying_security_id;
        if underlying_id.is_empty() {
            // Should never happen — Sub-PR #4 parser drops derivative
            // rows with empty underlying. Defensive: count as dangling.
            dangling_count = dangling_count.saturating_add(1);
            continue;
        }

        if nse_eq_lookup.contains_key(underlying_id) {
            unique_underlying_ids.insert(underlying_id.clone());
        } else {
            dangling_count = dangling_count.saturating_add(1);
        }
    }

    if total_derivative_count == 0 {
        return Err(ExtractError::NoDerivativeRowsFound {
            total_rows: rows.len(),
        });
    }

    let dangling_pct = (dangling_count as f64) / (total_derivative_count as f64);
    if dangling_pct > DANGLING_REFERENCE_REJECT_THRESHOLD {
        return Err(ExtractError::DanglingReferenceThresholdExceeded {
            dangling_count,
            total_derivative_count,
            pct: dangling_pct * 100.0,
            threshold_pct: DANGLING_REFERENCE_REJECT_THRESHOLD * 100.0,
        });
    }

    Ok(FnoUnderlyingExtraction {
        unique_underlying_ids,
        nse_eq_lookup,
        dangling_count,
        total_derivative_count,
    })
}

#[must_use]
fn is_stock_derivative(instrument: &str) -> bool {
    STOCK_DERIVATIVE_PREFIXES
        .iter()
        .any(|p| instrument.eq_ignore_ascii_case(p))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn stock_derivative_row(
        security_id: &str,
        instrument: &str,
        symbol: &str,
        underlying: &str,
    ) -> CsvRow {
        CsvRow {
            security_id: security_id.to_string(),
            exch_id: "NSE".to_string(),
            segment: "NSE_FNO".to_string(),
            instrument: instrument.to_string(),
            symbol_name: symbol.to_string(),
            underlying_security_id: underlying.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn extracts_unique_underlying_ids_from_derivatives() {
        let rows = vec![
            nse_eq_row("2885", "RELIANCE"),
            nse_eq_row("3045", "TCS"),
            stock_derivative_row("38919", "FUTSTK", "RELIANCE26JUNFUT", "2885"),
            stock_derivative_row("38920", "OPTSTK", "RELIANCE26JUN2500CE", "2885"),
            stock_derivative_row("39001", "FUTSTK", "TCS26JUNFUT", "3045"),
        ];
        let result = extract_fno_underlyings(&rows).expect("extract");
        assert_eq!(result.unique_underlying_ids.len(), 2);
        assert!(result.unique_underlying_ids.contains("2885"));
        assert!(result.unique_underlying_ids.contains("3045"));
        assert_eq!(result.total_derivative_count, 3);
        assert_eq!(result.dangling_count, 0);
        assert_eq!(result.nse_eq_lookup.len(), 2);
    }

    #[test]
    fn nse_eq_lookup_resolves_security_ids_to_rows() {
        let rows = vec![
            nse_eq_row("2885", "RELIANCE"),
            stock_derivative_row("38919", "FUTSTK", "RELIANCE26JUNFUT", "2885"),
        ];
        let result = extract_fno_underlyings(&rows).expect("extract");
        let reliance = result
            .nse_eq_lookup
            .get("2885")
            .expect("RELIANCE in lookup");
        assert_eq!(reliance.symbol_name, "RELIANCE");
        assert_eq!(reliance.segment, "NSE_EQ");
    }

    #[test]
    fn rejects_csv_with_above_threshold_dangling_refs() {
        // 100 stock-derivative rows, all dangling (no NSE_EQ rows at all)
        // = 100% dangling, way above 0.5% threshold.
        let mut rows: Vec<CsvRow> = (0..100)
            .map(|i| {
                stock_derivative_row(
                    &format!("4{i:04}"),
                    "FUTSTK",
                    &format!("UNKNOWN{i}FUT"),
                    &format!("99{i:04}"),
                )
            })
            .collect();
        // Add one NSE_EQ row so we don't trigger NoDerivativeRowsFound
        // via the dangling path before threshold check.
        rows.push(nse_eq_row("2885", "RELIANCE"));
        let result = extract_fno_underlyings(&rows);
        assert!(matches!(
            result,
            Err(ExtractError::DanglingReferenceThresholdExceeded {
                dangling_count: 100,
                total_derivative_count: 100,
                ..
            })
        ));
    }

    #[test]
    fn drops_single_dangling_below_threshold() {
        // 1000 valid derivatives + 1 dangling = 1/1001 = ~0.0999% < 0.5%.
        // The bad row is dropped (its underlying not added to set);
        // the extraction returns Ok.
        let mut rows: Vec<CsvRow> = Vec::new();
        // 1 NSE_EQ row to back all derivatives.
        rows.push(nse_eq_row("2885", "RELIANCE"));
        for i in 0..1000 {
            rows.push(stock_derivative_row(
                &format!("38{i:04}"),
                "FUTSTK",
                &format!("RELIANCE{i}FUT"),
                "2885",
            ));
        }
        // 1 dangling reference
        rows.push(stock_derivative_row(
            "99999",
            "FUTSTK",
            "UNKNOWNFUT",
            "GHOST",
        ));
        let result = extract_fno_underlyings(&rows).expect("extract below threshold");
        assert_eq!(result.unique_underlying_ids.len(), 1);
        assert!(result.unique_underlying_ids.contains("2885"));
        assert_eq!(result.total_derivative_count, 1001);
        assert_eq!(result.dangling_count, 1);
    }

    #[test]
    fn rejects_extraction_when_no_stock_derivatives_present() {
        let rows = vec![nse_eq_row("2885", "RELIANCE"), nse_eq_row("3045", "TCS")];
        let result = extract_fno_underlyings(&rows);
        assert!(matches!(
            result,
            Err(ExtractError::NoDerivativeRowsFound { total_rows: 2 })
        ));
    }

    #[test]
    fn ignores_non_stock_derivatives() {
        // FUTIDX/OPTIDX reference IDX_I underlyings, NOT NSE_EQ — they
        // must be ignored by this extractor (Sub-PR #6 handles them).
        let rows = vec![
            nse_eq_row("2885", "RELIANCE"),
            // Index futures should be IGNORED — would otherwise count as
            // dangling against the NSE_EQ lookup.
            CsvRow {
                security_id: "100".to_string(),
                exch_id: "NSE".to_string(),
                segment: "NSE_FNO".to_string(),
                instrument: "FUTIDX".to_string(),
                symbol_name: "NIFTY26JUNFUT".to_string(),
                underlying_security_id: "13".to_string(), // IDX_I NIFTY
                ..Default::default()
            },
            stock_derivative_row("38919", "FUTSTK", "RELIANCE26JUNFUT", "2885"),
        ];
        let result = extract_fno_underlyings(&rows).expect("extract");
        assert_eq!(result.total_derivative_count, 1, "only 1 stock derivative");
        assert_eq!(result.dangling_count, 0);
    }

    #[test]
    fn dedupes_multiple_derivatives_pointing_to_same_underlying() {
        // 5 options on RELIANCE → should resolve to 1 unique underlying.
        let mut rows = vec![nse_eq_row("2885", "RELIANCE")];
        for i in 0..5 {
            rows.push(stock_derivative_row(
                &format!("38{i:04}"),
                "OPTSTK",
                &format!("RELIANCE26JUN{}CE", 2500 + i * 50),
                "2885",
            ));
        }
        let result = extract_fno_underlyings(&rows).expect("extract");
        assert_eq!(result.unique_underlying_ids.len(), 1);
        assert_eq!(result.total_derivative_count, 5);
    }

    #[test]
    fn case_insensitive_segment_and_instrument_match() {
        // Defensive — some CSV exporters lowercase. The parser handles
        // lowercase headers (Sub-PR #4 test); the extractor should
        // handle lowercase VALUES too (segment, instrument).
        let rows = vec![
            CsvRow {
                security_id: "2885".to_string(),
                exch_id: "NSE".to_string(),
                segment: "nse_eq".to_string(),
                instrument: "equity".to_string(),
                symbol_name: "RELIANCE".to_string(),
                underlying_security_id: String::new(),
                ..Default::default()
            },
            CsvRow {
                security_id: "38919".to_string(),
                exch_id: "NSE".to_string(),
                segment: "nse_fno".to_string(),
                instrument: "futstk".to_string(),
                symbol_name: "RELIANCE26JUNFUT".to_string(),
                underlying_security_id: "2885".to_string(),
                ..Default::default()
            },
        ];
        let result = extract_fno_underlyings(&rows).expect("extract");
        assert_eq!(result.unique_underlying_ids.len(), 1);
        assert_eq!(result.nse_eq_lookup.len(), 1);
    }

    #[test]
    fn is_stock_derivative_matches_only_stk_prefixes() {
        assert!(is_stock_derivative("FUTSTK"));
        assert!(is_stock_derivative("OPTSTK"));
        assert!(is_stock_derivative("futstk"));
        assert!(!is_stock_derivative("FUTIDX"));
        assert!(!is_stock_derivative("OPTIDX"));
        assert!(!is_stock_derivative("EQUITY"));
        assert!(!is_stock_derivative("INDEX"));
        assert!(!is_stock_derivative(""));
    }

    #[test]
    fn threshold_constant_is_half_percent() {
        assert!((DANGLING_REFERENCE_REJECT_THRESHOLD - 0.005).abs() < 1e-9);
    }

    #[test]
    fn empty_rows_returns_no_derivatives_error() {
        let rows: Vec<CsvRow> = Vec::new();
        let result = extract_fno_underlyings(&rows);
        assert!(matches!(
            result,
            Err(ExtractError::NoDerivativeRowsFound { total_rows: 0 })
        ));
    }

    #[test]
    fn dangling_count_recorded_below_threshold() {
        // Verify the extraction RESULT carries forensic counts even
        // when within threshold — needed for the §9 L3 RECONCILE
        // observability gauge.
        let mut rows = vec![nse_eq_row("2885", "RELIANCE")];
        for i in 0..199 {
            rows.push(stock_derivative_row(
                &format!("38{i:04}"),
                "FUTSTK",
                &format!("F{i}"),
                "2885",
            ));
        }
        // 1 dangling out of 200 = 0.5% (AT threshold, accept).
        rows.push(stock_derivative_row("99999", "FUTSTK", "UNKNOWN", "GHOST"));
        let result = extract_fno_underlyings(&rows).expect("at threshold accepts");
        assert_eq!(result.dangling_count, 1);
        assert_eq!(result.total_derivative_count, 200);
    }
}
