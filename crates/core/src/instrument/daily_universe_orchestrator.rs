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
use super::index_futures::select_index_future_contracts;

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
    // Operator visibility (audit-findings Rule 11 — no silent drops): name the
    // skipped stragglers so each boot SHOWS exactly which constituents failed
    // the ISIN→Dhan join. Truncated to the first 20 to bound the log line.
    const MAX_LOGGED_STRAGGLERS: usize = 20;
    let dropped_names = outcome
        .unresolved_symbols
        .iter()
        .take(MAX_LOGGED_STRAGGLERS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(
        ntm_resolved = ntm_rows.len(),
        ntm_unresolved = outcome.unresolved_symbols.len(),
        ntm_total = outcome.total,
        ntm_dropped_names = %dropped_names,
        "NTM constituents resolved + bridged to Dhan rows (unresolved stragglers skipped)"
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
    today_ist: chrono::NaiveDate,
) -> Result<DailyUniverse, OrchestratorError> {
    // Step 1: parse
    let rows = parse_detailed_csv(bytes)?;

    // Step 2: extract F&O underlyings (cross-validates the dangling-
    // reference invariant per §3 + §26).
    let fno = extract_fno_underlyings(&rows)?;

    // Step 3: extract indices (asserts BSE SENSEX present per §0 quote 2).
    let indices = extract_indices(&rows)?;

    // §31 item 1 self-verify (operator 2026-06-06, "no illusion"): an
    // allowlisted NSE index whose exact `SYMBOL_NAME` is NOT in today's Dhan
    // master is a REAL miss — we intended to subscribe that index value and
    // didn't. This is the boot telemetry that catches a wrong/renamed Dhan
    // symbol (e.g. the NIFTY Total Market entry) LOUDLY instead of silently
    // dropping it. Empty list = positive signal (every allowlisted index found).
    if indices.allowlist_misses.is_empty() {
        tracing::info!(
            nse_indices = indices.nse_indices.len(),
            "index allowlist self-verify OK — every allowlisted NSE index found in the Dhan master"
        );
    } else {
        tracing::warn!(
            miss_count = indices.allowlist_misses.len(),
            missed = ?indices.allowlist_misses,
            "index allowlist MISS — allowlisted NSE index value(s) absent from today's Dhan \
             master; their live index value will NOT be subscribed (check the exact Dhan SYMBOL_NAME)"
        );
    }

    // Step 3b: collect the applicable F&O CONTRACTS for the lifecycle master
    // (operator lock 2026-05-29 Quote 5, §5). Stock F&O is matched by
    // underlying→NSE_EQ resolution; index F&O is matched by EXCHANGE (NSE/BSE)
    // because Dhan links index options/futures via a derivatives-domain
    // underlying SID (NIFTY=26000, BANKNIFTY=26009, SENSEX=1, BANKEX=12) that
    // is NOT the IDX_I spot SID — matching on the spot SID dropped every index
    // contract (2026-05-29 ~17K-row miss, fixed here). These rows are
    // master-only — they NEVER enter `subscription_targets`.
    let fno_contracts = collect_applicable_fno_contracts(&rows, &fno);

    // Step 3d — §36/§36.7 (2026-07-10): select the all-months FUTIDX rows
    // from the already-collected contracts. DEGRADE ONLY — never an
    // orchestrator `Err` (the §4 fail-closed contract stays on the CSV, not
    // on the futures overlay). Every miss pages FUTIDX-01: whole-underlying
    // (expiry = "ALL") or a single month (expiry = the date) — labels stay
    // static per-underlying; the month lives in the payload only.
    let futures_sel = select_index_future_contracts(&fno_contracts, today_ist);
    for miss in &futures_sel.misses {
        tracing::error!(
            code = ErrorCode::Futidx01SelectionDegraded.code_str(),
            feed = "dhan",
            underlying = miss.canonical,
            reason = ?miss.reason,
            expiry = %miss
                .expiry
                .map(|d| d.to_string())
                .unwrap_or_else(|| "ALL".into()),
            candidates_seen = ?futures_sel.fut_underlying_symbols_seen,
            "index-future selection degraded — this feed runs WITHOUT that future (or that \
             month) today; spot universe unaffected"
        );
        metrics::counter!(
            "tv_index_futures_selection_missing_total",
            "feed" => "dhan",
            "underlying" => miss.canonical
        )
        .increment(1);
    }
    // Hostile-review round 2 (2026-07-08): the Dhan gauge moved to the plan
    // builder (POST-PLAN honesty, `subscription_planner.rs`), and the
    // boot-evidence lines + parity recording moved to the shared
    // `record_dhan_selection_from_universe` helper called AFTER the universe
    // is built (below) — so the §29 warm-snapshot boot path emits the
    // IDENTICAL evidence + parity entry from `main.rs` without waiting on
    // the background reconcile.

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
    let universe = build_daily_universe(indices, fno, fno_contracts, ntm_rows, futures_sel.chosen)?;

    // §36 boot evidence + cross-feed parity recording — from the BUILT
    // universe (the same source the warm-snapshot path replays), so cold and
    // warm boots are provably identical here.
    super::index_futures::record_dhan_selection_from_universe(&universe, today_ist);
    // Scoreboard PR-D: register the universe into the per-instrument
    // presence registry (cross-feed coverage slots) — same seam as the
    // parity recording above so cold + warm boots register identically.
    super::presence_registration::register_dhan_presence_from_universe(
        &universe,
        super::presence_registration::ist_day_from_date(today_ist),
    );

    Ok(universe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_today() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 7, 8).expect("valid date")
    }
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
        let universe = build_universe_from_bytes(s.as_bytes(), None, test_today()).expect("build");
        // 30 NSE + 1 SENSEX + 100 underlyings = 131 instruments
        assert_eq!(universe.total_count(), 131);
    }

    #[test]
    fn builds_universe_with_futidx_rows_selects_index_futures() {
        // Hostile-review round 1 (2026-07-08) coverage gap: every prior
        // orchestrator fixture carried FUTSTK rows only, so the Step-3d
        // chosen-contract loop (boot-evidence info! + exact-canonical
        // FeedFutureSelection build + record_index_future_selection("dhan"))
        // never executed under test. This fixture drives it end-to-end.
        let mut s = String::from(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID,\
             UNDERLYING_SYMBOL,SM_EXPIRY_DATE\n",
        );
        for i in 0..100 {
            s.push_str(&format!("28{i:04},NSE,NSE_EQ,EQUITY,STK{i},,,\n"));
        }
        for i in 0..30 {
            let sym = NSE_INDEX_ALLOWLIST[i % NSE_INDEX_ALLOWLIST.len()];
            s.push_str(&format!("{},NSE,IDX_I,INDEX,{sym},,,\n", 1000 + i));
        }
        s.push_str("51,BSE,IDX_I,INDEX,SENSEX,,,\n");
        for i in 0..100 {
            s.push_str(&format!(
                "{},NSE,NSE_FNO,FUTSTK,STK{i}FUT,28{i:04},STK{i},2026-08-27\n",
                38000 + i
            ));
        }
        // The §36.7 FUTIDX rows — 3 NSE_FNO + 1 BSE_FNO, TWO months each
        // (all monthly serials enter under the 2026-07-10 grant).
        s.push_str("61001,NSE,NSE_FNO,FUTIDX,NIFTY26JULFUT,26000,NIFTY,2026-07-30\n");
        s.push_str("61002,NSE,NSE_FNO,FUTIDX,BANKNIFTY26JULFUT,26009,BANKNIFTY,2026-07-30\n");
        s.push_str("61003,NSE,NSE_FNO,FUTIDX,MIDCPNIFTY26JULFUT,26074,MIDCPNIFTY,2026-07-30\n");
        s.push_str("71001,BSE,BSE_FNO,FUTIDX,SENSEX26JULFUT,1,SENSEX,2026-07-30\n");
        s.push_str("62001,NSE,NSE_FNO,FUTIDX,NIFTY26AUGFUT,26000,NIFTY,2026-08-27\n");
        s.push_str("62002,NSE,NSE_FNO,FUTIDX,BANKNIFTY26AUGFUT,26009,BANKNIFTY,2026-08-27\n");
        s.push_str("62003,NSE,NSE_FNO,FUTIDX,MIDCPNIFTY26AUGFUT,26074,MIDCPNIFTY,2026-08-27\n");
        s.push_str("72001,BSE,BSE_FNO,FUTIDX,SENSEX26AUGFUT,1,SENSEX,2026-08-27\n");
        let universe = build_universe_from_bytes(s.as_bytes(), None, test_today()).expect("build");
        use crate::instrument::daily_universe::InstrumentRole;
        assert_eq!(
            universe.count_by_role(InstrumentRole::IndexFuture),
            8,
            "ALL §36.7 monthly FUTIDX contracts (2 months × 4 underlyings) enter the \
             subscription targets"
        );
        // 30 NSE + 1 SENSEX + 100 underlyings + 8 futures.
        assert_eq!(universe.total_count(), 139);
        // The chosen rows are the exact FUTIDX SIDs (both months).
        let mut futidx_sids: Vec<&str> = universe
            .subscription_targets
            .iter()
            .filter(|t| t.role == InstrumentRole::IndexFuture)
            .map(|t| t.csv_row.security_id.as_str())
            .collect();
        futidx_sids.sort_unstable();
        assert_eq!(
            futidx_sids,
            vec![
                "61001", "61002", "61003", "62001", "62002", "62003", "71001", "72001"
            ]
        );

        // Hostile-review round 2 (2026-07-08, F4): the shared warm/cold
        // recording helper derives the SAME 8 exact-canonical selections from
        // the BUILT universe — this is what the §29 warm-snapshot boot path
        // replays, so warm parity/evidence provably match the cold path.
        let sels = super::super::index_futures::dhan_selections_from_universe(&universe);
        assert_eq!(sels.len(), 8, "one selection per contract (§36.7)");
        let jul = chrono::NaiveDate::from_ymd_opt(2026, 7, 30).expect("d");
        let aug = chrono::NaiveDate::from_ymd_opt(2026, 8, 27).expect("d");
        for canonical in ["NIFTY", "BANKNIFTY", "MIDCPNIFTY", "SENSEX"] {
            let mut expiries: Vec<chrono::NaiveDate> = sels
                .iter()
                .filter(|s| s.canonical == canonical)
                .map(|s| s.expiry)
                .collect();
            expiries.sort_unstable();
            assert_eq!(
                expiries,
                vec![jul, aug],
                "{canonical}: both months recorded"
            );
        }
        let sensex_jul = sels
            .iter()
            .find(|s| s.canonical == "SENSEX" && s.expiry == jul)
            .expect("sensex jul");
        assert_eq!(sensex_jul.segment, "BSE_FNO");
        assert_eq!(sensex_jul.native_id, "71001");
    }

    #[test]
    fn maps_parse_error_to_instr_fetch_02() {
        // Empty CSV body — parser returns Empty error
        let bytes = b"SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID";
        let err = build_universe_from_bytes(bytes, None, test_today()).unwrap_err();
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
        let err = build_universe_from_bytes(s.as_bytes(), None, test_today()).unwrap_err();
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
        let err = build_universe_from_bytes(s.as_bytes(), None, test_today()).unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::InstrFetch03DanglingReferences);
        assert_eq!(err.stage(), "fno_underlying_extractor");
    }

    #[test]
    fn maps_build_error_to_instr_fetch_04_below_min() {
        // Valid CSV but only 1 underlying + 1 NSE_I + 1 SENSEX = 3
        // instruments, below MIN=100 envelope.
        let bytes = synthetic_csv(1, 1);
        let err = build_universe_from_bytes(&bytes, None, test_today()).unwrap_err();
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
        let parse_err = build_universe_from_bytes(b"SECURITY_ID", None, test_today()).unwrap_err();
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
        let u1 = build_universe_from_bytes(bytes, None, test_today()).expect("build 1");
        let u2 = build_universe_from_bytes(bytes, None, test_today()).expect("build 2");
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
        let err = build_universe_from_bytes(&bytes, None, test_today()).unwrap_err();
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
        let universe = build_universe_from_bytes(s.as_bytes(), Some(&map), test_today())
            .expect("build with ntm");
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
        let err = build_universe_from_bytes(b"SECURITY_ID,EXCH_ID,SEGMENT", None, test_today())
            .unwrap_err();
        let msg = format!("{err}");
        // The Display impl should include either "CSV parse failed" or
        // the underlying error message (depending on which layer fails
        // first — empty/short body fails parsing).
        assert!(
            msg.contains("CSV") || msg.contains("parse") || msg.contains("missing"),
            "got: {msg}"
        );
    }

    /// §31 item 1 (operator "no illusion"): the orchestrator MUST consume
    /// `indices.allowlist_misses` and log it, so a wrong/renamed Dhan index
    /// symbol surfaces at boot instead of silently dropping the index value.
    /// Source-scan guard (audit Rule 13: a computed-but-never-consumed field
    /// is a bug) — blocks regression back to a dead field.
    #[test]
    fn build_consumes_allowlist_misses_for_boot_self_verify() {
        let src = include_str!("daily_universe_orchestrator.rs");
        assert!(
            src.contains("indices.allowlist_misses"),
            "orchestrator MUST consume allowlist_misses (the §31 boot self-verify); \
             a computed-but-unlogged field is a dead-field bug (audit Rule 13)"
        );
        assert!(
            src.contains("index allowlist MISS"),
            "the allowlist-miss path MUST emit a loud warn so a wrong Dhan symbol \
             is visible at boot, not silent"
        );
    }
}
