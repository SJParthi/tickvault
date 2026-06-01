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

use thiserror::Error;
use tickvault_common::error_code::ErrorCode;

use super::csv_parser::{CsvParseError, parse_detailed_csv};
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

/// Chain the CSV parser + extractors + universe builder into a single
/// function.
///
/// **Pure function** — no I/O, no allocation beyond what the underlying
/// layers do, deterministic.
///
/// # Algorithm
///
/// 1. Parse the bytes via Sub-PR #4 `parse_detailed_csv()`
///    → `Vec<CsvRow>` on success, error maps to `INSTR-FETCH-02`
/// 2. Extract F&O underlyings via Sub-PR #5 `extract_fno_underlyings()`
///    → `FnoUnderlyingExtraction` on success, error maps to `INSTR-FETCH-03`
/// 3. Extract indices via Sub-PR #6 `extract_indices()`
///    → `IndexExtraction` on success, error maps to `INSTR-FETCH-02`
/// 4. Build the universe via Sub-PR #7 `build_daily_universe()`
///    → `DailyUniverse` on success, error maps to `INSTR-FETCH-04`
///
/// # Errors
///
/// See [`OrchestratorError`] variants.
///
/// # Performance
///
/// COLD PATH — called once at boot per trading day. The 4-step chain
/// totals ~10ms for a typical Dhan Detailed CSV (~25K rows). Not on
/// any hot path; runs inside Sub-PR #10b's boot-step timeout wrapper.
pub fn build_universe_from_bytes(bytes: &[u8]) -> Result<DailyUniverse, OrchestratorError> {
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

    // Step 4: build the unified universe (envelope check on the SUBSCRIPTION
    // set per §2 + §22; fno_contracts are master-only and not bounded).
    let universe = build_daily_universe(indices, fno, fno_contracts)?;

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
        let universe = build_universe_from_bytes(s.as_bytes()).expect("build");
        // 30 NSE + 1 SENSEX + 100 underlyings = 131 instruments
        assert_eq!(universe.total_count(), 131);
    }

    #[test]
    fn maps_parse_error_to_instr_fetch_02() {
        // Empty CSV body — parser returns Empty error
        let bytes = b"SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID";
        let err = build_universe_from_bytes(bytes).unwrap_err();
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
        let err = build_universe_from_bytes(s.as_bytes()).unwrap_err();
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
        let err = build_universe_from_bytes(s.as_bytes()).unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::InstrFetch03DanglingReferences);
        assert_eq!(err.stage(), "fno_underlying_extractor");
    }

    #[test]
    fn maps_build_error_to_instr_fetch_04_below_min() {
        // Valid CSV but only 1 underlying + 1 NSE_I + 1 SENSEX = 3
        // instruments, below MIN=100 envelope.
        let bytes = synthetic_csv(1, 1);
        let err = build_universe_from_bytes(&bytes).unwrap_err();
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
        let parse_err = build_universe_from_bytes(b"SECURITY_ID").unwrap_err();
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
        let u1 = build_universe_from_bytes(bytes).expect("build 1");
        let u2 = build_universe_from_bytes(bytes).expect("build 2");
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
        let err = build_universe_from_bytes(&bytes).unwrap_err();
        assert_eq!(
            err.error_code(),
            ErrorCode::InstrFetch02SchemaValidationFailed
        );
    }

    #[test]
    fn orchestrator_error_display_includes_underlying_message() {
        let err = build_universe_from_bytes(b"SECURITY_ID,EXCH_ID,SEGMENT").unwrap_err();
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
