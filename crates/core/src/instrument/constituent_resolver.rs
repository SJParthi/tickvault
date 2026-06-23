//! NTM Sub-PR #4 — resolve niftyindices constituents → Dhan NSE-EQ
//! `security_id` per the §31.1 mapping contract.
//!
//! **Feature-gated** behind `daily_universe_fetcher`. Pure + network-free:
//! `(IndexConstituencyMap, &[CsvRow]) -> Result<ResolveOutcome, _>`.
//!
//! ## §31.1 mapping rule
//!
//! - **PRIMARY key = ISIN.** Build an O(1) `HashMap<ISIN, (security_id,
//!   segment)>` from the Dhan rows filtered to `EXCH_ID=NSE AND SEGMENT=E`
//!   (canonical `NSE_EQ`) AND `INSTRUMENT=EQUITY`. Match each constituent's
//!   `ISIN Code` → that map.
//! - **SECONDARY / fallback = Symbol.** When a constituent has no ISIN (or
//!   the ISIN doesn't resolve), fall back to a `HashMap<Symbol, …>` over the
//!   same NSE-EQ-EQUITY rows. Symbol-alone is never the PRIMARY key.
//! - **Fail-closed (NTM membership tolerance = 2%).** A constituent that
//!   resolves to ZERO Dhan rows is counted as dangling; if
//!   `dangling / total > 2%` the whole resolve is REJECTED. This NTM-only
//!   membership-list tolerance (operator lock 2026-06-08) is DELIBERATELY
//!   LOOSER than the order-critical Dhan-master F&O dangling guard
//!   (`fno_underlying_extractor.rs`, §3, unchanged): a ~750-stock index
//!   membership list naturally carries a handful of unmatchable names
//!   (new listings / SME / ISIN edge cases), and 5 stragglers must NOT
//!   throw away 743 good stocks. Below the bar, the unresolved few are
//!   simply skipped (and logged by the caller); above it, the whole list
//!   is rejected (a genuinely broken niftyindices feed).
//! - **Dedup** the resolved set by `(security_id, exchange_segment)` (I-P1-11).
//!
//! Role tagging + the union with the F&O underlyings happen at the
//! subscription-wiring step (Sub-PR #5); this module only does the join.

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::{HashMap, HashSet};

use thiserror::Error;
use tickvault_common::instrument_types::IndexConstituencyMap;

use super::csv_parser::CsvRow;

/// §31.1(4) NTM membership reject threshold: > 2% unresolvable constituents.
///
/// Operator lock 2026-06-08 raised this from 0.5% → 2% after the live AWS
/// boot degraded the entire NTM universe (244 instead of ~1000) because
/// 5 of 748 constituents (0.67%) failed the ISIN→Dhan-master join and
/// 0.67% exceeded the old 0.5% bar. 2% (~15 of 748) gives headroom for the
/// handful of genuinely-unmatchable names (new listings / SME / ISIN edge
/// cases) while still fail-closing a materially broken niftyindices feed.
/// NOTE: this is the NTM membership-list tolerance ONLY — the Dhan-master
/// F&O dangling guard (`fno_underlying_extractor.rs`, §3) is a SEPARATE,
/// stricter, order-critical check and is unchanged.
pub const DANGLING_REJECT_FRACTION: f64 = 0.02;

/// A constituent successfully resolved to a Dhan NSE-EQ instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConstituent {
    /// NSE ticker (normalized uppercase).
    pub symbol: String,
    /// ISIN used for the match (empty if resolved via the symbol fallback).
    pub isin: String,
    /// Resolved Dhan `security_id`.
    pub security_id: String,
    /// Resolved exchange segment (always `NSE_EQ` here).
    pub exchange_segment: String,
    /// `true` if matched via the ISIN primary key, `false` via the symbol
    /// fallback. Surfaced for audit (the §31.1 cross-check).
    pub via_isin: bool,
}

/// Outcome of a resolve pass.
#[derive(Debug, Clone)]
pub struct ResolveOutcome {
    /// Resolved constituents, deduped by `(security_id, exchange_segment)`.
    pub resolved: Vec<ResolvedConstituent>,
    /// Constituent symbols that matched NO Dhan NSE-EQ row (dangling).
    pub unresolved_symbols: Vec<String>,
    /// Total unique constituents considered.
    pub total: usize,
}

/// Resolve failed the §31.1 fail-closed gate.
#[derive(Debug, Error, PartialEq)]
pub enum ConstituentResolveError {
    #[error(
        "constituent resolve dangling fraction {dangling}/{total} = {fraction:.4} exceeds {max}"
    )]
    TooManyDangling {
        dangling: usize,
        total: usize,
        fraction: f64,
        max: f64,
    },
}

/// Is this Dhan row an NSE cash-equity (`NSE_EQ` + `EQUITY`)? `CsvRow.segment`
/// is already canonical (`derive_exchange_segment`).
fn is_nse_eq_equity(row: &CsvRow) -> bool {
    row.segment == "NSE_EQ" && row.instrument.eq_ignore_ascii_case("EQUITY")
}

/// O(1) lookup index: uppercased ISIN (or symbol) → `(security_id, segment)`.
type EquityLookup = HashMap<String, (String, String)>;

/// Build the two O(1) lookup indexes (ISIN-primary + symbol-fallback) from
/// the NSE-EQ-EQUITY subset of the Dhan rows. First writer wins on a
/// duplicate key (Dhan should not have dup ISIN/symbol within NSE-EQ-EQUITY).
fn build_indexes(dhan_rows: &[CsvRow]) -> (EquityLookup, EquityLookup) {
    let mut by_isin = HashMap::new();
    let mut by_symbol = HashMap::new();
    for row in dhan_rows.iter().filter(|r| is_nse_eq_equity(r)) {
        let value = (row.security_id.clone(), row.segment.clone());
        let isin = row.isin.trim().to_uppercase();
        if !isin.is_empty() {
            by_isin.entry(isin).or_insert_with(|| value.clone());
        }
        let symbol = row.symbol_name.trim().to_uppercase();
        if !symbol.is_empty() {
            by_symbol.entry(symbol).or_insert(value);
        }
    }
    (by_isin, by_symbol)
}

/// Unique `(symbol, isin)` constituents across all indices in the map.
/// Same symbol in multiple indices → one entry (the first non-empty ISIN
/// wins so the ISIN primary key is preferred).
fn unique_constituents(constituency: &IndexConstituencyMap) -> Vec<(String, String)> {
    let mut seen: HashMap<String, String> = HashMap::new();
    for constituents in constituency.index_to_constituents.values() {
        for c in constituents {
            let entry = seen.entry(c.symbol.clone()).or_default();
            if entry.is_empty() && !c.isin.is_empty() {
                *entry = c.isin.clone();
            }
        }
    }
    let mut out: Vec<(String, String)> = seen.into_iter().collect();
    out.sort_unstable(); // deterministic order
    out
}

/// Resolve every unique constituent → Dhan NSE-EQ `security_id`.
///
/// # Errors
///
/// [`ConstituentResolveError::TooManyDangling`] when the unresolved
/// fraction exceeds [`DANGLING_REJECT_FRACTION`].
pub fn resolve_constituents(
    constituency: &IndexConstituencyMap,
    dhan_rows: &[CsvRow],
) -> Result<ResolveOutcome, ConstituentResolveError> {
    let (by_isin, by_symbol) = build_indexes(dhan_rows);
    let constituents = unique_constituents(constituency);
    let total = constituents.len();

    let mut resolved = Vec::new();
    let mut unresolved_symbols = Vec::new();
    let mut seen_sids: HashSet<(String, String)> = HashSet::new();

    for (symbol, isin) in constituents {
        let isin_key = isin.trim().to_uppercase();
        // PRIMARY: ISIN.
        let (hit, via_isin) = if !isin_key.is_empty() && by_isin.contains_key(&isin_key) {
            (by_isin.get(&isin_key), true)
        } else {
            // SECONDARY: symbol fallback.
            (by_symbol.get(&symbol), false)
        };
        match hit {
            Some((security_id, segment)) => {
                let key = (security_id.clone(), segment.clone());
                if seen_sids.insert(key) {
                    resolved.push(ResolvedConstituent {
                        symbol,
                        isin: if via_isin { isin_key } else { String::new() },
                        security_id: security_id.clone(),
                        exchange_segment: segment.clone(),
                        via_isin,
                    });
                }
            }
            None => unresolved_symbols.push(symbol),
        }
    }

    // §31.1(4) fail-closed gate (guard against 0/0).
    if total > 0 {
        let dangling = unresolved_symbols.len();
        let fraction = dangling as f64 / total as f64;
        if fraction > DANGLING_REJECT_FRACTION {
            return Err(ConstituentResolveError::TooManyDangling {
                dangling,
                total,
                fraction,
                max: DANGLING_REJECT_FRACTION,
            });
        }
    }

    Ok(ResolveOutcome {
        resolved,
        unresolved_symbols,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use tickvault_common::instrument_types::IndexConstituent;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 6).unwrap()
    }

    fn eq_row(security_id: &str, symbol: &str, isin: &str) -> CsvRow {
        CsvRow {
            security_id: security_id.to_string(),
            exch_id: "NSE".to_string(),
            segment: "NSE_EQ".to_string(),
            instrument: "EQUITY".to_string(),
            symbol_name: symbol.to_string(),
            isin: isin.to_string(),
            ..Default::default()
        }
    }

    fn map_with(constituents: &[(&str, &str)]) -> IndexConstituencyMap {
        let mut map = IndexConstituencyMap::default();
        let rows: Vec<IndexConstituent> = constituents
            .iter()
            .map(|(sym, isin)| IndexConstituent {
                index_name: "Nifty Total Market".to_string(),
                symbol: sym.to_string(),
                isin: isin.to_string(),
                weight: 0.0,
                sector: "Test".to_string(),
                last_updated: today(),
            })
            .collect();
        map.index_to_constituents
            .insert("Nifty Total Market".to_string(), rows);
        map
    }

    #[test]
    fn resolve_constituents_by_isin_primary() {
        let map = map_with(&[("RELIANCE", "INE002A01018")]);
        let dhan = vec![eq_row("2885", "RELIANCE", "INE002A01018")];
        let out = resolve_constituents(&map, &dhan).unwrap();
        assert_eq!(out.resolved.len(), 1);
        assert_eq!(out.resolved[0].security_id, "2885");
        assert!(out.resolved[0].via_isin);
        assert!(out.unresolved_symbols.is_empty());
    }

    #[test]
    fn isin_match_ignores_dhan_symbol_rename() {
        // Constituent symbol differs from Dhan SYMBOL_NAME, but ISIN matches.
        let map = map_with(&[("OLDTICKER", "INE002A01018")]);
        let dhan = vec![eq_row("2885", "NEWTICKER", "INE002A01018")];
        let out = resolve_constituents(&map, &dhan).unwrap();
        assert_eq!(out.resolved.len(), 1);
        assert_eq!(out.resolved[0].security_id, "2885");
        assert!(out.resolved[0].via_isin);
    }

    #[test]
    fn falls_back_to_symbol_when_isin_absent() {
        let map = map_with(&[("INFY", "")]);
        let dhan = vec![eq_row("1594", "INFY", "INE009A01021")];
        let out = resolve_constituents(&map, &dhan).unwrap();
        assert_eq!(out.resolved.len(), 1);
        assert_eq!(out.resolved[0].security_id, "1594");
        assert!(!out.resolved[0].via_isin, "matched via symbol fallback");
    }

    #[test]
    fn does_not_match_non_nse_eq_rows() {
        // A derivative row with the same ISIN must NOT resolve.
        let map = map_with(&[("RELIANCE", "INE002A01018")]);
        let mut deriv = eq_row("99", "RELIANCE", "INE002A01018");
        deriv.segment = "NSE_FNO".to_string();
        deriv.instrument = "FUTSTK".to_string();
        let out = resolve_constituents(&map, &[deriv]).unwrap_err();
        assert!(matches!(
            out,
            ConstituentResolveError::TooManyDangling { .. }
        ));
    }

    #[test]
    fn dedups_by_security_id_segment() {
        // Two constituents whose ISINs both map to the same Dhan SID → one.
        let map = map_with(&[("A", "INE111"), ("B", "INE222")]);
        let dhan = vec![
            eq_row("100", "A", "INE111"),
            eq_row("100", "B", "INE222"), // same SID (pathological) — dedup
        ];
        let out = resolve_constituents(&map, &dhan).unwrap();
        assert_eq!(out.resolved.len(), 1, "deduped by (security_id, segment)");
    }

    #[test]
    fn test_dangling_reject_fraction_is_two_percent() {
        // Operator lock 2026-06-08: NTM membership tolerance = 2% (was 0.5%).
        assert!((DANGLING_REJECT_FRACTION - 0.02).abs() < f64::EPSILON);
    }

    #[test]
    fn fail_closed_above_two_percent_dangling() {
        // 3 of 100 unresolved = 3% > 2% → reject.
        let mut pairs: Vec<(String, String)> = (0..97)
            .map(|i| (format!("SYM{i}"), format!("INE{i:09}")))
            .collect();
        for d in 0..3 {
            pairs.push((format!("DANGLE{d}"), format!("INE99999999{d}")));
        }
        let constituents: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(s, i)| (s.as_str(), i.as_str()))
            .collect();
        let map = map_with(&constituents);
        // Dhan has the first 97; the 3 DANGLEs are absent.
        let dhan: Vec<CsvRow> = pairs
            .iter()
            .take(97)
            .enumerate()
            .map(|(i, (s, isin))| eq_row(&format!("{i}"), s, isin))
            .collect();
        let err = resolve_constituents(&map, &dhan).unwrap_err();
        match err {
            ConstituentResolveError::TooManyDangling {
                dangling, total, ..
            } => {
                assert_eq!(dangling, 3);
                assert_eq!(total, 100);
            }
        }
    }

    #[test]
    fn tolerates_ntm_straggler_fraction() {
        // The live 2026-06-08 scenario: 5 of 748 unresolved = 0.67% < 2% →
        // Ok with 743 resolved, 5 skipped (NOT a whole-list rejection).
        let mut pairs: Vec<(String, String)> = (0..743)
            .map(|i| (format!("SYM{i}"), format!("INE{i:09}")))
            .collect();
        for d in 0..5 {
            pairs.push((format!("DANGLE{d}"), format!("INE88888888{d}")));
        }
        let constituents: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(s, i)| (s.as_str(), i.as_str()))
            .collect();
        let map = map_with(&constituents);
        // Dhan has the first 743; the 5 DANGLEs are absent.
        let dhan: Vec<CsvRow> = pairs
            .iter()
            .take(743)
            .enumerate()
            .map(|(i, (s, isin))| eq_row(&format!("{i}"), s, isin))
            .collect();
        let out = resolve_constituents(&map, &dhan).unwrap();
        assert_eq!(out.total, 748);
        assert_eq!(out.resolved.len(), 743);
        assert_eq!(out.unresolved_symbols.len(), 5);
    }

    #[test]
    fn tolerates_below_threshold_dangling() {
        // 1 of 300 unresolved = 0.33% < 0.5% → OK.
        let mut pairs: Vec<(String, String)> = (0..299)
            .map(|i| (format!("SYM{i}"), format!("INE{i:09}")))
            .collect();
        pairs.push(("DANGLE".to_string(), "INE999999999".to_string()));
        let constituents: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(s, i)| (s.as_str(), i.as_str()))
            .collect();
        let map = map_with(&constituents);
        let dhan: Vec<CsvRow> = pairs
            .iter()
            .take(299)
            .enumerate()
            .map(|(i, (s, isin))| eq_row(&format!("{i}"), s, isin))
            .collect();
        let out = resolve_constituents(&map, &dhan).unwrap();
        assert_eq!(out.total, 300);
        assert_eq!(out.resolved.len(), 299);
        assert_eq!(out.unresolved_symbols, vec!["DANGLE".to_string()]);
    }

    #[test]
    fn empty_constituency_is_ok() {
        let map = IndexConstituencyMap::default();
        let out = resolve_constituents(&map, &[]).unwrap();
        assert_eq!(out.total, 0);
        assert!(out.resolved.is_empty());
    }
}
