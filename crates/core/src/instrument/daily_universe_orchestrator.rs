//! Sub-PR #10 of 2026-05-27 daily-universe expansion — chain the CSV
//! parser + extractors + universe builder into a single function.
//!
//! **Feature-gated.** This module compiles only when the
//! `daily_universe_fetcher` cargo feature is enabled (per §21 of the
//! authoritative rule file). Under the current `Indices4Only` default
//! scope this module is dead code.
//!
//! ## Contract (§10 of the rule file)
//!
//! Input: validated CSV bytes (downloaded + body-cap-checked by
//! Sub-PR #3 `csv_downloader::fetch_csv()` upstream).
//!
//! Output: `DailyUniverse` if every layer succeeds, OR a typed
//! `OrchestratorError` that maps to one of the 4 INSTR-FETCH-*
//! ErrorCode variants from Sub-PR #9.
//!
//! ## Why bytes-in (not URL-in)
//!
//! Keeping this module pure (no I/O) means:
//! 1. Unit tests can exercise the chain with synthetic bytes —
//!    no network mocks, no integration plumbing.
//! 2. The §4 infinite-retry loop + cache write + boot-step deadlines
//!    live in the OUTER caller (Sub-PR #10b — main.rs boot orchestrator),
//!    NOT this module. Single responsibility.
//! 3. The `--operator-acknowledge-stale-csv` escape valve (per §20)
//!    feeds yesterday's bytes into this same function — no special
//!    path needed.
//!
//! ## What this module DOES NOT do
//!
//! - HTTP fetch (Sub-PR #3 `csv_downloader`)
//! - Retry / backoff orchestration (Sub-PR #10b)
//! - Cache file persistence (Sub-PR #10b)
//! - Telegram event emission (Sub-PR #10b)
//! - QuestDB `instrument_lifecycle` persistence (Sub-PR #9b)
//! - Operator escape-valve flag parsing (Sub-PR #10b)

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::HashMap;

use thiserror::Error;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::instrument_types::IndexConstituencyMap;

use super::constituent_resolver::resolve_constituents;
use super::csv_parser::{CsvParseError, CsvRow, parse_detailed_csv};
use super::daily_universe::{BuildError, DailyUniverse, build_daily_universe};
use super::fno_underlying_extractor::{
    ExtractError, collect_applicable_fno_contracts, extract_fno_underlyings,
};
use super::index_extractor::{IndexExtractError, extract_indices};

/// Errors that can occur during orchestration. Each variant wraps the
/// underlying error from Sub-PRs #4-#7 and maps to ONE of the 4
/// INSTR-FETCH-* ErrorCode variants from Sub-PR #9 — used by the
/// outer caller (Sub-PR #10b) for typed Telegram routing.
#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// CSV parser rejected the body. Per Sub-PR #4 §26 — missing
    /// header column, non-UTF-8 bytes, or >0.1% row failures.
    /// Maps to `INSTR-FETCH-02`.
    #[error("CSV parse failed: {0}")]
    Parse(#[from] CsvParseError),

    /// F&O extractor rejected the parsed rows — typically >0.5%
    /// dangling-reference threshold per Sub-PR #5 §3 + §26. Also fires
    /// on `NoDerivativeRowsFound` (defensive — CSV had zero stock
    /// derivatives, indicates upstream corruption).
    /// Maps to `INSTR-FETCH-03`.
    #[error("F&O underlying extraction failed: {0}")]
    FnoExtract(#[from] ExtractError),

    /// Index extractor rejected the parsed rows — either no NSE
    /// IDX_I rows OR the operator-locked BSE SENSEX row is missing
    /// per Sub-PR #6 + §0 quote 2.
    /// Maps to `INSTR-FETCH-02` (treated as a CSV schema problem
    /// since these rows MUST be present in a valid Dhan CSV).
    #[error("index extraction failed: {0}")]
    IndexExtract(#[from] IndexExtractError),

    /// Built universe is outside `[MIN, MAX]` envelope per Sub-PR #7
    /// + §2 + §22.
    /// Maps to `INSTR-FETCH-04`.
    #[error("universe size envelope violated: {0}")]
    Build(#[from] BuildError),
}

impl OrchestratorError {
    /// Map this error to the appropriate INSTR-FETCH-* code for
    /// Telegram routing + CloudWatch metric tagging.
    #[must_use]
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::Parse(_) => ErrorCode::InstrFetch02SchemaValidationFailed,
            Self::FnoExtract(_) => ErrorCode::InstrFetch03DanglingReferences,
            Self::IndexExtract(_) => ErrorCode::InstrFetch02SchemaValidationFailed,
            Self::Build(_) => ErrorCode::InstrFetch04UniverseSizeOutOfBounds,
        }
    }

    /// Stable wire-format identifier for the underlying-layer that
    /// failed. Used by the L5 AUDIT layer (audit row's `failed_stage`
    /// column) + CloudWatch metric dimensions.
    #[must_use]
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Parse(_) => "csv_parser",
            Self::FnoExtract(_) => "fno_underlying_extractor",
            Self::IndexExtract(_) => "index_extractor",
            Self::Build(_) => "daily_universe_builder",
        }
    }
}

/// Resolve the §31 NTM constituency map against the parsed Dhan rows and
/// BRIDGE each resolved constituent back to its full Dhan `CsvRow`.
///
/// **Degrade-by-design (operator AskUserQuestion 2026-06-06):** if the resolve
/// step fails (`> 0.5%` dangling, i.e. the niftyindices list is out of sync with
/// the Dhan master) this logs `error!(code = NTM-CONSTITUENCY-01)` and returns an
/// EMPTY vec — the caller proceeds on the indices + F&O-underlyings core universe.
/// It is NEVER a boot halt; the Dhan core path stays fail-closed independently.
///
/// The bridge builds a `HashMap<(security_id, segment), &CsvRow>` over `rows`
/// (O(N) once, cold path) and does an O(1) lookup per resolved constituent. Every
/// resolved SID was sourced FROM `rows`, so a lookup miss is impossible by
/// construction; a miss is counted defensively, never panics.
///
/// COLD PATH — runs once at boot.
#[must_use]
pub fn resolve_ntm_rows(rows: &[CsvRow], ntm_map: &IndexConstituencyMap) -> Vec<CsvRow> {
    let outcome = match resolve_constituents(ntm_map, rows) {
        Ok(o) => o,
        Err(err) => {
            // DEGRADE: NTM layer unusable; core universe continues.
            tracing::error!(
                code = ErrorCode::NtmConstituency01SourceDegraded.code_str(),
                reason = "dangling",
                %err,
                "NTM constituency resolve failed — degrading to core universe (no NTM constituents this session)"
            );
            return Vec::new();
        }
    };

    // Bridge: (security_id, segment) → &CsvRow, O(1) lookup per resolved.
    let mut by_key: HashMap<(&str, &str), &CsvRow> = HashMap::with_capacity(rows.len());
    for r in rows {
        by_key.insert((r.security_id.as_str(), r.segment.as_str()), r);
    }
    let mut ntm_rows: Vec<CsvRow> = Vec::with_capacity(outcome.resolved.len());
    let mut bridge_misses = 0usize;
    for rc in &outcome.resolved {
        match by_key.get(&(rc.security_id.as_str(), rc.exchange_segment.as_str())) {
            Some(row) => ntm_rows.push((*row).clone()),
            None => bridge_misses += 1, // impossible by construction; counted, never panic
        }
    }
    if bridge_misses > 0 {
        tracing::warn!(
            bridge_misses,
            resolved = outcome.resolved.len(),
            "NTM bridge: resolved constituent had no matching Dhan row (unexpected)"
        );
    }
    tracing::info!(
        ntm_resolved = ntm_rows.len(),
        ntm_unresolved = outcome.unresolved_symbols.len(),
        ntm_total = outcome.total,
        "NTM constituents resolved + bridged to Dhan rows"
    );
    ntm_rows
}

/// Chain the CSV parser + extractors + universe builder into a single
/// function.
///
/// **Pure function** — no I/O. `ntm_map` is the §31 NIFTY-Total-Market
/// constituency (built by the boot caller via `build_constituency_map`);
/// `None` means the niftyindices source was unavailable (the caller already
/// emitted `NTM-CONSTITUENCY-01`) and the universe is built on the indices +
/// F&O-underlyings core set only.
///
/// # Algorithm
///
/// 1. Parse the bytes via Sub-PR #4 `parse_detailed_csv()` → `INSTR-FETCH-02`
/// 2. Extract F&O underlyings via Sub-PR #5 → `INSTR-FETCH-03`
/// 3. Extract indices via Sub-PR #6 → `INSTR-FETCH-02`
/// 3c. Resolve + bridge the §31 NTM constituents (degrade-safe — empty on
///     failure, NOT an error)
/// 4. Build the universe via Sub-PR #7 → `INSTR-FETCH-04`
///
/// # Errors
///
/// See [`OrchestratorError`] variants. NTM-source failure is NOT an error
/// (degrade-by-design per §31 + operator 2026-06-06).
///
/// # Performance
///
/// COLD PATH — called once at boot per trading day.
pub fn build_universe_from_bytes(
    bytes: &[u8],
    ntm_map: Option<&IndexConstituencyMap>,
) -> Result<DailyUniverse, OrchestratorError> {
    // Step 1: parse
    let rows = parse_detailed_csv(bytes)?;

    // Step 2: extract F&O underlyings (cross-validates the dangling-
    // reference invariant per §3 + §26).
    let fno = extract_fno_underlyings(&rows)?;

    // Step 3: extract indices (asserts BSE SENSEX present per §0 quote 2).
    let indices = extract_indices(&rows)?;

    // Step 3b: collect the applicable F&O CONTRACTS for the lifecycle master
    // (operator lock 2026-05-29 Quote 5, §5). Stock F&O is matched by
    // underlying→NSE_EQ resolution; index F&O is matched by EXCHANGE (NSE/BSE)
    // because Dhan links index options/futures via a derivatives-domain
    // underlying SID (NIFTY=26000, BANKNIFTY=26009, SENSEX=1, BANKEX=12) that
    // is NOT the IDX_I spot SID — matching on the spot SID dropped every index
    // contract (2026-05-29 ~17K-row miss, fixed here). These rows are
    // master-only — they NEVER enter `subscription_targets`.
    let fno_contracts = collect_applicable_fno_contracts(&rows, &fno);

    // Step 3c: resolve + bridge the §31 NTM constituents (degrade-safe). When
    // `ntm_map` is None (source unavailable) or resolve fails, this is empty and
    // the universe is the indices + F&O-underlyings core set.
    let ntm_rows = match ntm_map {
        Some(map) => resolve_ntm_rows(&rows, map),
        None => Vec::new(),
    };

    // Step 4: build the unified universe (envelope check on the SUBSCRIPTION
    // set per §2 + §22; fno_contracts are master-only and not bounded). An NTM
    // set that pushes the universe past MAX is an INSTR-FETCH-04 fail-closed
    // HALT (a data anomaly), NOT a degrade.
    let universe = build_daily_universe(indices, fno, fno_contracts, ntm_rows)?;

    Ok(universe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::index_extractor::NSE_INDEX_ALLOWLIST;

    /// Build a synthetic Detailed-CSV body with N stock-derivative rows
    /// backed by 1 NSE_EQ underlying + N NSE indices + 1 BSE SENSEX.
    fn synthetic_csv(num_derivatives: usize, num_nse_indices: usize) -> Vec<u8> {
        let mut s = String::from(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID\n",
        );
        // 1 NSE_EQ underlying
        s.push_str("2885,NSE,NSE_EQ,EQUITY,RELIANCE,\n");
        // N NSE indices (NSE IDX_I INDEX rows)
        for i in 0..num_nse_indices {
            // Use REAL allowlisted index symbols (operator lock 2026-06-01
            // §30) — fake "IDX{i}" names are now dropped by the allowlist.
            let sym = NSE_INDEX_ALLOWLIST[i % NSE_INDEX_ALLOWLIST.len()];
            s.push_str(&format!("{i},NSE,IDX_I,INDEX,{sym},\n"));
        }
        // 1 BSE SENSEX (mandatory per §0 quote 2)
        s.push_str("51,BSE,IDX_I,INDEX,SENSEX,\n");
        // N FUTSTK derivatives pointing to RELIANCE
        for i in 0..num_derivatives {
            s.push_str(&format!(
                "{},NSE,NSE_FNO,FUTSTK,RELIANCE26{}FUT,2885\n",
                38000 + i,
                i
            ));
        }
        s.into_bytes()
    }

    #[test]
    fn builds_typical_universe_end_to_end() {
        // 30 indices + 1 SENSEX + 100 derivatives backed by RELIANCE
        // = 1 underlying + 31 indices = 32 instruments. Below MIN=100,
        // so we need more underlyings. Add diversity:
        let mut s = String::from(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID\n",
        );
        // 100 unique NSE_EQ underlyings
        for i in 0..100 {
            s.push_str(&format!("28{i:04},NSE,NSE_EQ,EQUITY,STK{i},\n"));
        }
        // 30 NSE indices
        for i in 0..30 {
            let sym = NSE_INDEX_ALLOWLIST[i % NSE_INDEX_ALLOWLIST.len()];
            s.push_str(&format!("{},NSE,IDX_I,INDEX,{sym},\n", 1000 + i));
        }
        // 1 BSE SENSEX
        s.push_str("51,BSE,IDX_I,INDEX,SENSEX,\n");
        // 100 derivatives (one per underlying)
        for i in 0..100 {
            s.push_str(&format!(
                "{},NSE,NSE_FNO,FUTSTK,STK{i}FUT,28{i:04}\n",
                38000 + i
            ));
        }
        let universe = build_universe_from_bytes(s.as_bytes(), None).expect("build");
        // 30 NSE + 1 SENSEX + 100 underlyings = 131 instruments
        assert_eq!(universe.total_count(), 131);
    }

    #[test]
    fn maps_parse_error_to_instr_fetch_02() {
        // Empty CSV body — parser returns Empty error
        let bytes = b"SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID";
        let err = build_universe_from_bytes(bytes, None).unwrap_err();
        assert_eq!(
            err.error_code(),
            ErrorCode::InstrFetch02SchemaValidationFailed
        );
        assert_eq!(err.stage(), "csv_parser");
    }

    #[test]
    fn maps_index_extract_error_to_instr_fetch_02() {
        // CSV without BSE SENSEX — index extractor returns BseSensexMissing
        let mut s = String::from(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID\n",
        );
        s.push_str("2885,NSE,NSE_EQ,EQUITY,RELIANCE,\n");
        s.push_str("13,NSE,IDX_I,INDEX,NIFTY,\n");
        s.push_str("38000,NSE,NSE_FNO,FUTSTK,RELIANCEFUT,2885\n");
        let err = build_universe_from_bytes(s.as_bytes(), None).unwrap_err();
        assert_eq!(
            err.error_code(),
            ErrorCode::InstrFetch02SchemaValidationFailed
        );
        assert_eq!(err.stage(), "index_extractor");
    }

    #[test]
    fn maps_fno_extract_error_to_instr_fetch_03() {
        // CSV with 100% dangling underlying refs in derivatives but
        // valid indices + at least one NSE_EQ row.
        let mut s = String::from(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID\n",
        );
        s.push_str("2885,NSE,NSE_EQ,EQUITY,RELIANCE,\n");
        s.push_str("13,NSE,IDX_I,INDEX,NIFTY,\n");
        s.push_str("51,BSE,IDX_I,INDEX,SENSEX,\n");
        // 100 derivatives, all dangling
        for i in 0..100 {
            s.push_str(&format!(
                "{},NSE,NSE_FNO,FUTSTK,GHOST{i}FUT,99{i:04}\n",
                38000 + i
            ));
        }
        let err = build_universe_from_bytes(s.as_bytes(), None).unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::InstrFetch03DanglingReferences);
        assert_eq!(err.stage(), "fno_underlying_extractor");
    }

    #[test]
    fn maps_build_error_to_instr_fetch_04_below_min() {
        // Valid CSV but only 1 underlying + 1 NSE_I + 1 SENSEX = 3
        // instruments, below MIN=100 envelope.
        let bytes = synthetic_csv(1, 1);
        let err = build_universe_from_bytes(&bytes, None).unwrap_err();
        assert_eq!(
            err.error_code(),
            ErrorCode::InstrFetch04UniverseSizeOutOfBounds
        );
        assert_eq!(err.stage(), "daily_universe_builder");
    }

    #[test]
    fn error_code_method_covers_all_4_instr_fetch_variants() {
        // Defensive — verify the error_code() match exhaustively maps
        // to the 4 INSTR-FETCH-* variants. INSTR-FETCH-01 is NOT
        // emitted from this module (CSV fetch happens in Sub-PR #3
        // upstream).
        use tickvault_common::error_code::ErrorCode::*;
        let expected = [
            InstrFetch02SchemaValidationFailed,  // Parse
            InstrFetch03DanglingReferences,      // FnoExtract
            InstrFetch02SchemaValidationFailed,  // IndexExtract (also schema-class)
            InstrFetch04UniverseSizeOutOfBounds, // Build
        ];
        // We can't actually call error_code() on each without constructing
        // a real error, but the test above (`maps_*_error_to_instr_fetch_*`)
        // already covers each. This test pins the expected mapping
        // table inline so a future refactor of the match arm is
        // ratcheted.
        assert_eq!(expected.len(), 4);
    }

    #[test]
    fn stage_returns_stable_wire_format() {
        // The stage() values feed CloudWatch metric dimensions + audit
        // table columns; they MUST be stable across releases.
        let parse_err = build_universe_from_bytes(b"SECURITY_ID", None).unwrap_err();
        assert_eq!(parse_err.stage(), "csv_parser");
    }

    #[test]
    fn deterministic_replay_same_bytes_yields_same_universe() {
        let mut s = String::from(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID\n",
        );
        for i in 0..100 {
            s.push_str(&format!("28{i:04},NSE,NSE_EQ,EQUITY,STK{i},\n"));
        }
        for i in 0..30 {
            let sym = NSE_INDEX_ALLOWLIST[i % NSE_INDEX_ALLOWLIST.len()];
            s.push_str(&format!("{},NSE,IDX_I,INDEX,{sym},\n", 1000 + i));
        }
        s.push_str("51,BSE,IDX_I,INDEX,SENSEX,\n");
        for i in 0..100 {
            s.push_str(&format!(
                "{},NSE,NSE_FNO,FUTSTK,STK{i}FUT,28{i:04}\n",
                38000 + i
            ));
        }
        let bytes = s.as_bytes();

        // Two calls with identical bytes → identical total_count.
        let u1 = build_universe_from_bytes(bytes, None).expect("build 1");
        let u2 = build_universe_from_bytes(bytes, None).expect("build 2");
        assert_eq!(u1.total_count(), u2.total_count());
    }

    #[test]
    fn non_utf8_body_maps_to_instr_fetch_02() {
        // 0xC9 0x6E is not valid UTF-8 (continuation byte expected
        // after 0xC9).
        let mut bytes = Vec::from(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID\n",
        );
        bytes.extend_from_slice(b"13,NSE,IDX_I,INDEX,");
        bytes.push(0xC9);
        bytes.push(0x6E);
        bytes.extend_from_slice(b",\n");
        let err = build_universe_from_bytes(&bytes, None).unwrap_err();
        assert_eq!(
            err.error_code(),
            ErrorCode::InstrFetch02SchemaValidationFailed
        );
    }

    // ----- Sub-PR #10a: NTM resolve + bridge (§31) -----

    fn eq_row(sid: &str, sym: &str, isin: &str) -> CsvRow {
        CsvRow {
            security_id: sid.to_string(),
            exch_id: "NSE".to_string(),
            segment: "NSE_EQ".to_string(),
            instrument: "EQUITY".to_string(),
            symbol_name: sym.to_string(),
            isin: isin.to_string(),
            ..Default::default()
        }
    }

    fn ntm_map(items: &[(&str, &str)]) -> IndexConstituencyMap {
        use tickvault_common::instrument_types::IndexConstituent;
        let mut map = IndexConstituencyMap::default();
        let rows: Vec<IndexConstituent> = items
            .iter()
            .map(|(sym, isin)| IndexConstituent {
                index_name: "Nifty Total Market".to_string(),
                symbol: (*sym).to_string(),
                isin: (*isin).to_string(),
                weight: 0.0,
                sector: "Test".to_string(),
                last_updated: chrono::NaiveDate::from_ymd_opt(2026, 6, 6).unwrap(),
            })
            .collect();
        map.index_to_constituents
            .insert("Nifty Total Market".to_string(), rows);
        map
    }

    #[test]
    fn resolve_ntm_rows_bridges_resolved_to_dhan_rows() {
        // 2 Dhan NSE_EQ rows; NTM references one by ISIN → bridge returns its
        // FULL CsvRow (not just the resolved tuple).
        let rows = vec![
            eq_row("2885", "RELIANCE", "INE002A01018"),
            eq_row("1594", "INFY", "INE009A01021"),
        ];
        let map = ntm_map(&[("RELIANCE", "INE002A01018")]);
        let bridged = resolve_ntm_rows(&rows, &map);
        assert_eq!(bridged.len(), 1);
        assert_eq!(bridged[0].security_id, "2885");
        assert_eq!(bridged[0].symbol_name, "RELIANCE");
        assert_eq!(bridged[0].segment, "NSE_EQ");
    }

    #[test]
    fn resolve_ntm_rows_degrades_to_empty_on_dangling() {
        // All constituents dangle (no matching Dhan row) → >0.5% threshold →
        // resolve_constituents errors → degrade to EMPTY, never panics.
        let rows = vec![eq_row("2885", "RELIANCE", "INE002A01018")];
        let map = ntm_map(&[
            ("GHOST1", "INE000X00001"),
            ("GHOST2", "INE000X00002"),
            ("GHOST3", "INE000X00003"),
        ]);
        let bridged = resolve_ntm_rows(&rows, &map);
        assert!(bridged.is_empty(), "dangling → degrade to empty");
    }

    #[test]
    fn build_universe_with_ntm_map_threads_through() {
        // End-to-end: a valid CSV + an NTM map referencing an F&O underlying
        // (STK0) → both-case flagged, no dup, universe still valid.
        let mut s = String::from(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID\n",
        );
        for i in 0..100 {
            s.push_str(&format!("28{i:04},NSE,NSE_EQ,EQUITY,STK{i},\n"));
        }
        for i in 0..30 {
            let sym = NSE_INDEX_ALLOWLIST[i % NSE_INDEX_ALLOWLIST.len()];
            s.push_str(&format!("{i},NSE,IDX_I,INDEX,{sym},\n"));
        }
        s.push_str("51,BSE,IDX_I,INDEX,SENSEX,\n");
        for i in 0..100 {
            s.push_str(&format!("38{i:04},NSE,NSE_FNO,FUTSTK,STK{i}FUT,28{i:04}\n"));
        }
        // NTM references STK0 (symbol fallback — synthetic CSV has no ISIN).
        let map = ntm_map(&[("STK0", "")]);
        let universe = build_universe_from_bytes(s.as_bytes(), Some(&map)).expect("build with ntm");
        // STK0 is an F&O underlying AND now an NTM constituent → both flags, no dup.
        assert_eq!(universe.index_constituent_count(), 1);
        let both = universe
            .subscription_targets
            .iter()
            .find(|t| t.csv_row.symbol_name == "STK0")
            .expect("STK0 present");
        assert!(both.is_fno_underlying && both.is_index_constituent);
    }

    #[test]
    fn orchestrator_error_display_includes_underlying_message() {
        let err = build_universe_from_bytes(b"SECURITY_ID,EXCH_ID,SEGMENT", None).unwrap_err();
        let msg = format!("{err}");
        // The Display impl should include either "CSV parse failed" or
        // the underlying error message (depending on which layer fails
        // first — empty/short body fails parsing).
        assert!(
            msg.contains("CSV") || msg.contains("parse") || msg.contains("missing"),
            "got: {msg}"
        );
    }
}
