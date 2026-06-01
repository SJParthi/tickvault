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
use tickvault_common::constants::CSV_TEST_SYMBOL_MARKER;

use super::csv_parser::CsvRow;

/// Reject threshold for dangling-reference failures across the
/// derivative subset. Per rule file §26.
///
/// Value: 0.005 = 0.5% in decimal.
pub const DANGLING_REFERENCE_REJECT_THRESHOLD: f64 = 0.005;

/// NSE_EQ segment string (per Dhan ExchangeSegment enum). Used to
/// classify cash-equity rows for the underlying-resolution lookup.
const NSE_EQ_SEGMENT: &str = "NSE_EQ";

/// `EXCH_ID` value for the National Stock Exchange. Stock derivatives on
/// any other exchange (e.g. BSE) reference a NON-`NSE_EQ` underlying and
/// are therefore OUT OF SCOPE for the NSE_EQ-only daily universe (rule
/// §2 + §4 — "every UNDERLYING_SECURITY_ID from FUTSTK/OPTSTK rows
/// resolves to an NSE_EQ row"). Excluding them mirrors the deleted
/// `universe_builder` Pass-3 behaviour, which skipped BSE stock futures.
const NSE_EXCH_ID: &str = "NSE";

/// `EXCH_ID` value for the Bombay Stock Exchange. In scope for INDEX
/// derivatives only (SENSEX, BANKEX, SENSEX50, FOCIT options/futures) per the
/// operator 2026-05-29 "everything incl BANKEX" scope — NOT for BSE single-
/// stock F&O (operator: "BSE only SENSEX [index]"), which the stock path
/// still excludes via its `NSE`-only gate.
const BSE_EXCH_ID: &str = "BSE";

/// Maximum number of dangling-reference samples carried in the rejection
/// error for operator triage. Bounded so a fully-dangling CSV can't
/// allocate a multi-thousand-entry Vec into the error/log chain.
const MAX_DANGLING_SAMPLES: usize = 10;

/// Derivative instrument prefixes that REFERENCE an NSE_EQ underlying.
/// Index derivatives (FUTIDX/OPTIDX) reference IDX_I underlyings —
/// those are handled separately by Sub-PR #6 (indices extractor).
/// Currency/commodity derivatives are out of scope per the operator-
/// locked 2-segment scope (NSE_EQ + NSE_FNO).
const STOCK_DERIVATIVE_PREFIXES: &[&str] = &["FUTSTK", "OPTSTK"];

/// Index F&O contract instrument types — the applicable index derivatives for
/// the `instrument_lifecycle` master (operator lock 2026-05-29 Quote 5, §5).
const INDEX_DERIVATIVE_PREFIXES: &[&str] = &["FUTIDX", "OPTIDX"];

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
         dangling underlying refs — threshold {threshold_pct:.4}% — sample: [{}]",
        sample.join(", ")
    )]
    DanglingReferenceThresholdExceeded {
        dangling_count: usize,
        total_derivative_count: usize,
        pct: f64,
        threshold_pct: f64,
        /// Up to [`MAX_DANGLING_SAMPLES`] `"<symbol>→<underlying_id>"`
        /// strings naming the first dangling rows — surfaced in the
        /// error/log chain so the operator can see WHICH underlyings
        /// failed to resolve without re-running with a debugger.
        sample: Vec<String>,
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
    // Pass 1 — build the NSE_EQ index, keyed by the CANONICAL integer form
    // of `SECURITY_ID`. Both `SECURITY_ID` and `UNDERLYING_SECURITY_ID` are
    // documented integer SecurityIds, but a raw-string `==` compare is
    // fragile against format drift on the real Dhan CSV (leading zeros,
    // trailing `.0`, surrounding whitespace). Normalising both sides to the
    // parsed integer makes "11536", " 11536 ", "011536" and "11536.0" all
    // resolve to the same equity — matching how `today_instrument` parses
    // these cells to `i64` downstream. NSE_EQ rows whose id is non-numeric
    // (should never happen) are skipped so they can't shadow a real id.
    let mut nse_eq_lookup: HashMap<String, CsvRow> = HashMap::new();
    for row in rows {
        if row.segment.eq_ignore_ascii_case(NSE_EQ_SEGMENT)
            && let Some(canonical) = canonical_security_id(&row.security_id)
        {
            nse_eq_lookup.insert(canonical, row.clone());
        }
    }

    // Pass 2 — scan IN-SCOPE stock-derivative rows, validate underlying refs.
    //
    // In scope = NSE FUTSTK/OPTSTK that are NOT Dhan TEST contracts. The
    // real Dhan Detailed CSV also ships (a) BSE stock derivatives whose
    // underlyings live in BSE_EQ (absent from the NSE_EQ-only lookup) and
    // (b) placeholder TEST contracts with synthetic underlyings — both
    // would otherwise be miscounted as dangling and blow past the 0.5%
    // threshold, blocking boot forever. The deleted `universe_builder`
    // Pass-3 excluded both for exactly this reason.
    let mut unique_underlying_ids: HashSet<String> = HashSet::new();
    let mut dangling_count: usize = 0;
    let mut total_derivative_count: usize = 0;
    let mut sample: Vec<String> = Vec::new();

    for row in rows {
        if !is_in_scope_stock_derivative(&row.instrument, &row.exch_id) {
            continue;
        }
        if is_test_instrument(&row.symbol_name, &row.underlying_symbol) {
            continue;
        }
        total_derivative_count = total_derivative_count.saturating_add(1);

        // Normalise the underlying to its canonical integer form before
        // resolving — see Pass 1. An empty / non-numeric underlying yields
        // `None` and is counted as dangling (should never happen: Sub-PR #4
        // drops derivative rows with an empty underlying).
        match canonical_security_id(&row.underlying_security_id) {
            Some(canonical) if nse_eq_lookup.contains_key(&canonical) => {
                unique_underlying_ids.insert(canonical);
            }
            _ => {
                dangling_count = dangling_count.saturating_add(1);
                if sample.len() < MAX_DANGLING_SAMPLES {
                    sample.push(format!(
                        "{}→{}",
                        row.symbol_name, row.underlying_security_id
                    ));
                }
            }
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
            sample,
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

/// A stock derivative that is in scope for the NSE_EQ-only daily universe:
/// FUTSTK/OPTSTK on the National Stock Exchange. BSE stock derivatives are
/// excluded because their underlyings live in `BSE_EQ`, which the
/// NSE_EQ-only lookup intentionally does not contain (rule §2 + §4).
#[must_use]
fn is_in_scope_stock_derivative(instrument: &str, exch_id: &str) -> bool {
    is_stock_derivative(instrument) && exch_id.eq_ignore_ascii_case(NSE_EXCH_ID)
}

/// An index derivative (FUTIDX/OPTIDX) is in scope when it trades on an
/// EQUITY-index exchange — NSE or BSE. Index F&O cannot be matched by the
/// IDX_I spot SecurityId because Dhan references the index via a separate
/// derivatives-domain SID (NIFTY=26000, BANKNIFTY=26009, SENSEX=1, BANKEX=12,
/// VERIFIED on the live 2026-05-28 Detailed CSV), so we gate by exchange
/// instead. MCX commodity-index derivatives (MCXBULLDEX/MCXMETLDEX) are
/// EXCLUDED here per the operator no-commodity lock — only NSE + BSE equity
/// indices are in scope ("everything incl BANKEX", operator 2026-05-29).
#[must_use]
fn is_equity_index_exchange(exch_id: &str) -> bool {
    exch_id.eq_ignore_ascii_case(NSE_EXCH_ID) || exch_id.eq_ignore_ascii_case(BSE_EXCH_ID)
}

/// Dhan ships placeholder TEST contracts in the instrument master whose
/// `UNDERLYING_SECURITY_ID` is synthetic and never resolves to a real
/// `NSE_EQ` row. Skip them (mirrors the deleted `universe_builder` Pass-3
/// `CSV_TEST_SYMBOL_MARKER` skip) so they are not miscounted as dangling.
/// Normalise a raw CSV SecurityId cell to its canonical integer string.
///
/// Returns `None` for empty / non-numeric cells. Accepts surrounding
/// whitespace, leading zeros (`"011536"`), and a trailing `.0`
/// (`"11536.0"` — some CSV exporters render integer columns as floats).
/// Two ids that denote the same instrument always normalise to the same
/// `String`, so the equity lookup is robust to Dhan-side format drift.
#[must_use]
fn canonical_security_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Tolerate a single trailing ".0" / ".00…" float rendering.
    let digits = match trimmed.split_once('.') {
        Some((int_part, frac)) if frac.chars().all(|c| c == '0') => int_part,
        Some(_) => return None, // non-zero fractional part → not a SecurityId
        None => trimmed,
    };
    digits.parse::<u64>().ok().map(|n| n.to_string())
}

#[must_use]
fn is_test_instrument(symbol_name: &str, underlying_symbol: &str) -> bool {
    let marker = CSV_TEST_SYMBOL_MARKER.to_ascii_uppercase();
    symbol_name.to_ascii_uppercase().contains(&marker)
        || underlying_symbol.to_ascii_uppercase().contains(&marker)
}

#[must_use]
fn is_index_derivative(instrument: &str) -> bool {
    INDEX_DERIVATIVE_PREFIXES
        .iter()
        .any(|p| instrument.eq_ignore_ascii_case(p))
}

/// Collect the **applicable F&O contract rows** for the `instrument_lifecycle`
/// master (operator lock 2026-05-29 Quote 5, §5 of
/// `daily-universe-scope-expansion-2026-05-27.md`):
///
///  - in-scope NSE `FUTSTK`/`OPTSTK` whose `UNDERLYING_SECURITY_ID` resolves to
///    one of our tracked NSE_EQ underlyings (`fno.unique_underlying_ids`), AND
///  - `FUTIDX`/`OPTIDX` whose `UNDERLYING_SECURITY_ID` resolves to one of our
///    tracked index SIDs (`index_sids`).
///
/// EXCLUDES currency F&O (`FUTCUR`/`OPTCUR`), commodity F&O (`FUTCOM`/`OPTFUT`),
/// Dhan TEST placeholders, and any derivative whose underlying is NOT tracked.
/// These rows are persisted to the master ONLY — they are NEVER subscribed
/// (the WebSocket reads `DailyUniverse::subscription_targets`).
///
/// PURE: no I/O, no allocation beyond the returned `Vec`. O(rows).
#[must_use]
pub fn collect_applicable_fno_contracts(
    rows: &[CsvRow],
    fno: &FnoUnderlyingExtraction,
) -> Vec<CsvRow> {
    let mut contracts: Vec<CsvRow> = Vec::new();
    for row in rows {
        if is_test_instrument(&row.symbol_name, &row.underlying_symbol) {
            continue;
        }
        let applicable = if is_in_scope_stock_derivative(&row.instrument, &row.exch_id) {
            // Stock F&O: underlying MUST resolve to a tracked NSE_EQ spot.
            // Dhan links stock options to the cash-equity SecurityId, e.g.
            // HDFCBANK option → underlying 1333 = HDFCBANK NSE_EQ spot. A
            // non-numeric/empty underlying yields None → excluded.
            canonical_security_id(&row.underlying_security_id)
                .is_some_and(|canonical| fno.unique_underlying_ids.contains(&canonical))
        } else if is_index_derivative(&row.instrument) {
            // Index F&O: Dhan links these via a DERIVATIVES-domain underlying
            // SID (NIFTY=26000, BANKNIFTY=26009, SENSEX=1, BANKEX=12) that is
            // NOT the IDX_I spot SID (NIFTY=13, BANKNIFTY=25, SENSEX=51) —
            // VERIFIED against the live 2026-05-28 Detailed CSV. Matching on
            // the index spot id therefore dropped EVERY index option/future
            // (the 2026-05-29 ~17K-row miss). Operator scope 2026-05-29
            // ("everything incl BANKEX") = ALL NSE + BSE equity-index F&O, so
            // we gate by EXCHANGE. MCX commodity-index derivatives
            // (MCXBULLDEX/MCXMETLDEX) stay excluded per the no-commodity lock.
            is_equity_index_exchange(&row.exch_id)
        } else {
            false
        };
        if applicable {
            contracts.push(row.clone());
        }
    }
    contracts
}

/// Canonicalise a raw CSV `SECURITY_ID` cell (public wrapper over the internal
/// normaliser) so callers assembling the `index_sids` set use the SAME
/// normalisation as the underlying-resolution path.
#[must_use]
pub fn canonical_sid(raw: &str) -> Option<String> {
    canonical_security_id(raw)
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

    fn bse_stock_derivative_row(
        security_id: &str,
        instrument: &str,
        symbol: &str,
        underlying: &str,
    ) -> CsvRow {
        CsvRow {
            security_id: security_id.to_string(),
            exch_id: "BSE".to_string(),
            segment: "BSE_FNO".to_string(),
            instrument: instrument.to_string(),
            symbol_name: symbol.to_string(),
            underlying_security_id: underlying.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn bse_stock_derivatives_are_out_of_scope_not_dangling() {
        // The real Dhan CSV ships BSE FUTSTK/OPTSTK whose underlyings live
        // in BSE_EQ (absent from the NSE_EQ-only lookup). They MUST be
        // ignored entirely — neither counted nor flagged dangling — else a
        // valid CSV blows past the 0.5% threshold and blocks boot forever.
        let rows = vec![
            nse_eq_row("2885", "RELIANCE"),
            stock_derivative_row("38919", "FUTSTK", "RELIANCE26JUNFUT", "2885"),
            // 50 BSE stock derivatives with BSE_EQ underlyings — would all
            // be "dangling" against the NSE_EQ lookup if not excluded.
            bse_stock_derivative_row("70001", "OPTSTK", "BSESTK1", "500001"),
            bse_stock_derivative_row("70002", "FUTSTK", "BSESTK2", "500002"),
        ];
        let result = extract_fno_underlyings(&rows).expect("BSE rows excluded → accepts");
        assert_eq!(
            result.total_derivative_count, 1,
            "only the NSE derivative counts"
        );
        assert_eq!(result.dangling_count, 0);
        assert!(result.unique_underlying_ids.contains("2885"));
    }

    #[test]
    fn test_marker_contracts_are_skipped() {
        // Dhan TEST placeholder contracts carry synthetic underlyings that
        // never resolve. They must be skipped (matches deleted
        // universe_builder Pass-3 CSV_TEST_SYMBOL_MARKER skip).
        let rows = vec![
            nse_eq_row("2885", "RELIANCE"),
            stock_derivative_row("38919", "FUTSTK", "RELIANCE26JUNFUT", "2885"),
            // TEST contract on the symbol name…
            stock_derivative_row("88001", "FUTSTK", "TESTSTK26JUNFUT", "999001"),
            // …and one flagged via the underlying symbol.
            CsvRow {
                security_id: "88002".to_string(),
                exch_id: "NSE".to_string(),
                segment: "NSE_FNO".to_string(),
                instrument: "OPTSTK".to_string(),
                symbol_name: "ZZ26JUN100CE".to_string(),
                underlying_security_id: "999002".to_string(),
                underlying_symbol: "TESTSTK".to_string(),
                ..Default::default()
            },
        ];
        let result = extract_fno_underlyings(&rows).expect("TEST rows skipped → accepts");
        assert_eq!(
            result.total_derivative_count, 1,
            "TEST contracts not counted"
        );
        assert_eq!(result.dangling_count, 0);
    }

    #[test]
    fn dangling_error_carries_sample_of_offending_rows() {
        // 100 NSE FUTSTK rows, all dangling → rejection error must name the
        // first few offenders for operator triage.
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
        rows.push(nse_eq_row("2885", "RELIANCE"));
        let err = extract_fno_underlyings(&rows).expect_err("must reject");
        let ExtractError::DanglingReferenceThresholdExceeded { sample, .. } = err else {
            panic!("expected DanglingReferenceThresholdExceeded, got {err:?}");
        };
        assert_eq!(sample.len(), MAX_DANGLING_SAMPLES, "sample is bounded");
        assert!(
            sample.iter().any(|s| s.contains("UNKNOWN0FUT→990000")),
            "sample must name an offending symbol→underlying pair: {sample:?}",
        );
    }

    #[test]
    fn is_in_scope_filters_exchange_and_instrument() {
        assert!(is_in_scope_stock_derivative("FUTSTK", "NSE"));
        assert!(is_in_scope_stock_derivative("optstk", "nse"));
        assert!(!is_in_scope_stock_derivative("FUTSTK", "BSE"));
        assert!(!is_in_scope_stock_derivative("FUTIDX", "NSE"));
    }

    #[test]
    fn is_test_instrument_matches_either_field() {
        assert!(is_test_instrument("TESTSTK26JUNFUT", ""));
        assert!(is_test_instrument("ZZ", "teststk"));
        assert!(!is_test_instrument("RELIANCE26JUNFUT", "RELIANCE"));
    }

    #[test]
    fn excludes_real_nsetest_scrips_011_through_181() {
        // 2026-06-01 cross-check vs NSE fo_mktlots.csv: Dhan's master carries
        // 18 `0N1NSETEST`/`1N1NSETEST` dummy underlyings (011NSETEST..181NSETEST)
        // that are NOT real F&O stocks. NSE's official list = 211 stocks;
        // Dhan FUTSTK/OPTSTK distinct underlyings = 229; the 18 difference is
        // exactly these test scrips. They MUST be excluded so we subscribe 211,
        // not 229. (`CSV_TEST_SYMBOL_MARKER == "TEST"` catches the NSETEST suffix.)
        for sid in ["011", "091", "181"] {
            let und = format!("{sid}NSETEST");
            let sym = format!("{und}26JUNFUT");
            assert!(
                is_test_instrument(&sym, &und),
                "{und} must be filtered as a TEST scrip"
            );
        }
        // A real stock with TEST nowhere in it is kept.
        assert!(!is_test_instrument("HDFCBANK26JUNFUT", "HDFCBANK"));
    }

    #[test]
    fn canonical_security_id_normalizes_format_drift() {
        assert_eq!(canonical_security_id("11536").as_deref(), Some("11536"));
        assert_eq!(canonical_security_id(" 11536 ").as_deref(), Some("11536"));
        assert_eq!(canonical_security_id("011536").as_deref(), Some("11536"));
        assert_eq!(canonical_security_id("11536.0").as_deref(), Some("11536"));
        assert_eq!(canonical_security_id("11536.00").as_deref(), Some("11536"));
        assert_eq!(canonical_security_id(""), None);
        assert_eq!(canonical_security_id("   "), None);
        assert_eq!(canonical_security_id("TCS"), None);
        assert_eq!(canonical_security_id("115.36"), None);
    }

    #[test]
    fn canonical_sid_public_wrapper_matches_internal() {
        assert_eq!(canonical_sid("011536").as_deref(), Some("11536"));
        assert_eq!(canonical_sid(" 13 ").as_deref(), Some("13"));
        assert_eq!(canonical_sid("TCS"), None);
        assert_eq!(canonical_sid(""), None);
    }

    #[test]
    fn underlying_resolves_despite_float_and_zero_pad_format_drift() {
        // Equity id is the clean "2885"; the derivative rows reference it
        // with a trailing ".0" and a leading zero. Both MUST resolve —
        // exact-string compare would have flagged them dangling.
        let rows = vec![
            nse_eq_row("2885", "RELIANCE"),
            stock_derivative_row("38919", "FUTSTK", "RELIANCE26JUNFUT", "2885.0"),
            stock_derivative_row("38920", "OPTSTK", "RELIANCE26JUN2500CE", "0002885"),
        ];
        let result = extract_fno_underlyings(&rows).expect("format drift resolves");
        assert_eq!(result.dangling_count, 0);
        assert_eq!(result.total_derivative_count, 2);
        assert_eq!(result.unique_underlying_ids.len(), 1);
        assert!(result.unique_underlying_ids.contains("2885"));
    }

    // ---- collect_applicable_fno_contracts (operator lock 2026-05-29 Quote 5) ----

    fn deriv_row(instrument: &str, exch: &str, underlying_sid: &str, symbol: &str) -> CsvRow {
        CsvRow {
            security_id: "999".to_string(),
            exch_id: exch.to_string(),
            segment: if instrument.contains("IDX") {
                "NSE_FNO".to_string()
            } else {
                "NSE_FNO".to_string()
            },
            instrument: instrument.to_string(),
            symbol_name: symbol.to_string(),
            underlying_security_id: underlying_sid.to_string(),
            ..Default::default()
        }
    }

    fn fno_with_underlying(sid: &str) -> FnoUnderlyingExtraction {
        let mut unique_underlying_ids = HashSet::new();
        unique_underlying_ids.insert(sid.to_string());
        let mut nse_eq_lookup = HashMap::new();
        nse_eq_lookup.insert(sid.to_string(), nse_eq_row(sid, "RELIANCE"));
        FnoUnderlyingExtraction {
            unique_underlying_ids,
            nse_eq_lookup,
            dangling_count: 0,
            total_derivative_count: 1,
        }
    }

    #[test]
    fn collect_applicable_fno_contracts_keeps_stock_and_nse_bse_index_excludes_rest() {
        // 2026-05-29 fix: index F&O is matched by EXCHANGE (NSE/BSE), NOT by
        // the IDX_I spot SID — Dhan links index options via a derivatives-
        // domain underlying SID (NIFTY=26000, SENSEX=1) that never equals the
        // spot SID (13/51). MCX commodity-index stays excluded.
        let fno = fno_with_underlying("2885"); // RELIANCE tracked underlying

        let rows = vec![
            // ✅ stock derivatives whose underlying 2885 resolves to NSE_EQ
            deriv_row("FUTSTK", "NSE", "2885", "RELIANCE26JUNFUT"),
            deriv_row("OPTSTK", "NSE", "2885", "RELIANCE26JUN2800CE"),
            // ✅ NSE index F&O — derivatives-domain underlying SID 26000/26009
            //    (NOT spot 13/25); matched purely by exch=NSE now.
            deriv_row("FUTIDX", "NSE", "26000", "NIFTY26JUNFUT"),
            deriv_row("OPTIDX", "NSE", "26000", "NIFTY26JUN24000CE"),
            deriv_row("OPTIDX", "NSE", "26009", "BANKNIFTY26JUN50000CE"),
            // ✅ BSE index F&O — SENSEX (underlying 1) + BANKEX (underlying 12)
            deriv_row("OPTIDX", "BSE", "1", "SENSEX26JUN80000CE"),
            deriv_row("OPTIDX", "BSE", "12", "BANKEX26JUN60000CE"),
            // ❌ stock derivative whose underlying is NOT tracked
            deriv_row("OPTSTK", "NSE", "99999", "UNTRACKED26JUN100CE"),
            // ❌ MCX commodity-index F&O — excluded (no-commodity lock)
            deriv_row("OPTIDX", "MCX", "568", "MCXBULLDEX26JUN100CE"),
            // ❌ currency / commodity F&O — not stock/index derivatives
            deriv_row("OPTCUR", "NSE", "13", "USDINR26JUN90CE"),
            deriv_row("FUTCOM", "MCX", "13", "GOLD26JUNFUT"),
            // ❌ TEST placeholder — excluded
            deriv_row("OPTSTK", "NSE", "2885", "TESTSTK26JUN100CE"),
        ];

        let got = collect_applicable_fno_contracts(&rows, &fno);
        let symbols: Vec<&str> = got.iter().map(|r| r.symbol_name.as_str()).collect();
        assert_eq!(
            got.len(),
            7,
            "2 stock + 3 NSE-index + 2 BSE-index; got {symbols:?}"
        );
        assert!(symbols.contains(&"RELIANCE26JUNFUT"));
        assert!(symbols.contains(&"RELIANCE26JUN2800CE"));
        assert!(symbols.contains(&"NIFTY26JUNFUT"));
        assert!(symbols.contains(&"NIFTY26JUN24000CE"));
        assert!(symbols.contains(&"BANKNIFTY26JUN50000CE"));
        assert!(symbols.contains(&"SENSEX26JUN80000CE"));
        assert!(symbols.contains(&"BANKEX26JUN60000CE"));
        // MCX commodity-index must NOT leak in.
        assert!(!symbols.contains(&"MCXBULLDEX26JUN100CE"));
    }

    #[test]
    fn collect_applicable_fno_contracts_excludes_untracked_stock_and_mcx_index() {
        let fno = fno_with_underlying("2885");
        let rows = vec![
            // untracked stock underlying → excluded
            deriv_row("FUTSTK", "NSE", "77777", "OTHER26JUNFUT"),
            // MCX commodity-index → excluded (only NSE/BSE equity-index in scope)
            deriv_row("OPTIDX", "MCX", "568", "MCXBULLDEX26JUN100CE"),
        ];
        assert!(collect_applicable_fno_contracts(&rows, &fno).is_empty());
    }
}
