//! Sub-PR #10b-ζ-2 (2026-05-27) — INSTR-FETCH audit-write seam.
//!
//! Maps the daily-universe fetch chain's per-attempt + terminal outcomes
//! to [`InstrumentFetchAuditRow`] values and writes them via the storage
//! helper. This seam lives in the **app** crate because the dependency
//! flow is `common ← core ← trading ← storage ← api ← app`: the
//! INSTR-FETCH runner lives in `core` and the `instrument_fetch_audit`
//! writer lives in `storage`, so `core` cannot call the writer directly.
//! The app crate is the first layer that depends on both.
//!
//! # Why per-attempt
//!
//! In production the retry loop runs with `max_attempts = None` (§4
//! infinite retry), so it only ever *terminates* on
//! [`LoopOutcome::Success`]. Every failure class
//! (`INSTR-FETCH-01..04`) exists ONLY at the per-attempt level, surfaced
//! through the loop's emit callback ([`TelegramEmit`]). A terminal-only
//! audit row would therefore record nothing but successes — defeating
//! the forensic purpose of the table (rule §9 "row per attempt"). So the
//! mapping here produces:
//!
//! * one **failure row** per Telegram-eligible attempt (attempt 4+,
//!   where `decide_retry` emits a [`TelegramEmit`]), and
//! * one **terminal Success row** once the universe builds.
//!
//! Attempts 1–3 are the §4 silent-retry tier (no emit) and are not
//! audited to the table; the first escalation row at attempt 4 records
//! that retries have been occurring.
//!
//! # Boot callsite
//!
//! The boot orchestrator (final Sub-PR #10) calls
//! [`ensure_instrument_fetch_audit_table`] once at boot, drives
//! `core::instrument::instr_fetch_loop::run_instr_fetch_loop` with an
//! emit closure that calls [`record_attempt_failure`] per attempt, and
//! calls [`record_terminal_success`] once the loop returns. This module
//! ships the pure mapping + builders + thin writers that the boot
//! callsite consumes; it is feature-gated under `daily_universe_fetcher`
//! per rule §21 and dormant until Sub-PR #10 flips the flag.
//!
//! # Timestamp convention
//!
//! Both `ts` and `trading_date_ist` are IST wall-clock nanoseconds. The
//! fetch chain runs on the host clock (`Utc::now()` → IST), so the caller
//! adds the IST offset before handing nanos here (greeks-pipeline
//! convention, NOT the live-tick convention — see `data-integrity.md`).

#![cfg(feature = "daily_universe_fetcher")]

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_core::instrument::instr_fetch_loop::LoopOutcome;
use tickvault_core::instrument::instr_fetch_retry_adapter::TelegramEmit;
use tickvault_storage::instrument_fetch_audit_persistence::{
    FETCH_AUDIT_FEED_DHAN, FetchOutcome, InstrumentFetchAuditRow, append_instrument_fetch_audit_row,
};

/// Map a per-attempt [`ErrorCode`] (carried by a [`TelegramEmit`]) to the
/// matching [`FetchOutcome`] SYMBOL.
///
/// Returns `None` for any code outside the four `INSTR-FETCH-*` failure
/// classes. The retry adapter only ever produces those four for this
/// loop, so `None` means a caller passed an unrelated code — we refuse to
/// write a misleading audit row rather than guess a default outcome.
#[must_use]
pub fn map_error_code_to_fetch_outcome(code: ErrorCode) -> Option<FetchOutcome> {
    match code {
        ErrorCode::InstrFetch01CsvHardFailed => Some(FetchOutcome::CsvHardFailed),
        ErrorCode::InstrFetch02SchemaValidationFailed => Some(FetchOutcome::SchemaValidationFailed),
        ErrorCode::InstrFetch03DanglingReferences => Some(FetchOutcome::DanglingReferences),
        ErrorCode::InstrFetch04UniverseSizeOutOfBounds => {
            Some(FetchOutcome::UniverseSizeOutOfBounds)
        }
        _ => None,
    }
}

/// Build the per-attempt **failure** audit row from a [`TelegramEmit`].
///
/// Returns `None` when the emit's `error_code` is not an `INSTR-FETCH-*`
/// failure class (see [`map_error_code_to_fetch_outcome`]). The returned
/// row borrows `detail`; the caller owns the backing string (typically
/// the emit's `stage` plus any diagnostic text).
///
/// Counts are 0 (the universe never built on a failed attempt) and
/// `source_csv_sha256` is empty (the bytes were rejected or never
/// fetched).
#[must_use]
pub fn build_attempt_failure_row<'a>(
    emit: &TelegramEmit,
    attempt: u32,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    dry_run: bool,
    detail: &'a str,
) -> Option<InstrumentFetchAuditRow<'a>> {
    let outcome = map_error_code_to_fetch_outcome(emit.error_code)?;
    Some(InstrumentFetchAuditRow {
        ts_nanos_ist,
        trading_date_ist_nanos,
        outcome,
        attempt,
        error_code: emit.error_code.code_str(),
        total_rows: 0,
        universe_size: 0,
        index_count: 0,
        underlying_count: 0,
        source_csv_sha256: "",
        dry_run,
        detail,
        // operator override 2026-06-28: feed in-key on every persisted table.
        // The instrument-fetch chain is Dhan-only today.
        feed: FETCH_AUDIT_FEED_DHAN,
    })
}

/// Build the **terminal Success** audit row once the universe builds.
///
/// `attempts_used` is read from [`LoopOutcome::Success`]; a non-Success
/// outcome returns `None` (production never terminates on failure under
/// §4 infinite retry, so this only guards the test-only `Exhausted`
/// path). `error_code` is empty for the success row.
///
/// The three counts are passed as primitives — the boot callsite reads
/// them from the built `DailyUniverse` via `total_count()` /
/// `count_by_role(...)`. Keeping the builder primitive-typed means this
/// app module needs no `DailyUniverse` construction (its `CsvRow` field
/// type is private to `core`) and stays trivially unit-testable.
#[must_use]
#[allow(clippy::too_many_arguments)] // APPROVED: audit row has 12 columns; a builder struct would just relocate the arity.
pub fn build_terminal_success_row<'a>(
    outcome: &LoopOutcome,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    total_rows: i64,
    universe_size: i64,
    index_count: i64,
    underlying_count: i64,
    index_constituent_count: i64,
    source_csv_sha256: &'a str,
    dry_run: bool,
) -> Option<InstrumentFetchAuditRow<'a>> {
    let LoopOutcome::Success { attempts_used } = outcome else {
        return None;
    };
    // The universe `role` field partitions every SID into exactly THREE
    // mutually-exclusive primary roles — Index, FnoUnderlying, IndexConstituent
    // (the NTM constituent stocks added in §31). So the total MUST equal the sum
    // of the three role counts. `index_constituent_count` here is the
    // ROLE-partition count (`count_by_role(IndexConstituent)`), NOT the
    // overlapping `is_index_constituent` flag count. Catches a future
    // boot-callsite that extracts divergent counts before it silently writes an
    // inconsistent audit row. Debug-only — no release overhead.
    debug_assert_eq!(
        universe_size,
        index_count + underlying_count + index_constituent_count,
        "universe_size must equal index + underlying + index_constituent (3 mutually-exclusive roles, §31 NTM)"
    );
    Some(InstrumentFetchAuditRow {
        ts_nanos_ist,
        trading_date_ist_nanos,
        outcome: FetchOutcome::Success,
        attempt: *attempts_used,
        error_code: "",
        total_rows,
        universe_size,
        index_count,
        underlying_count,
        source_csv_sha256,
        dry_run,
        detail: "",
        // operator override 2026-06-28: feed in-key (Dhan-only fetch chain today).
        feed: FETCH_AUDIT_FEED_DHAN,
    })
}

/// Write one per-attempt failure audit row. No-op `Ok(())` when the
/// emit's code is not an `INSTR-FETCH-*` class (nothing to audit).
///
/// Cold path — at most a few calls per boot (only Telegram-eligible
/// attempts 4+). Idempotent via the table's DEDUP UPSERT KEYS.
///
/// # Errors
///
/// Propagates the storage transport error / non-2xx `/exec` response.
pub async fn record_attempt_failure(
    questdb_config: &QuestDbConfig,
    emit: &TelegramEmit,
    attempt: u32,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    dry_run: bool,
    detail: &str,
) -> anyhow::Result<()> {
    let Some(row) = build_attempt_failure_row(
        emit,
        attempt,
        ts_nanos_ist,
        trading_date_ist_nanos,
        dry_run,
        detail,
    ) else {
        return Ok(());
    };
    append_instrument_fetch_audit_row(questdb_config, &row).await
}

/// Write the terminal Success audit row. No-op `Ok(())` for a
/// non-Success outcome (the test-only `Exhausted` path).
///
/// # Errors
///
/// Propagates the storage transport error / non-2xx `/exec` response.
#[allow(clippy::too_many_arguments)] // APPROVED: audit row has 12 columns; a builder struct would just relocate the arity.
pub async fn record_terminal_success(
    questdb_config: &QuestDbConfig,
    outcome: &LoopOutcome,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    total_rows: i64,
    universe_size: i64,
    index_count: i64,
    underlying_count: i64,
    index_constituent_count: i64,
    source_csv_sha256: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    let Some(row) = build_terminal_success_row(
        outcome,
        ts_nanos_ist,
        trading_date_ist_nanos,
        total_rows,
        universe_size,
        index_count,
        underlying_count,
        index_constituent_count,
        source_csv_sha256,
        dry_run,
    ) else {
        return Ok(());
    };
    append_instrument_fetch_audit_row(questdb_config, &row).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::error_code::Severity;

    fn emit_with(code: ErrorCode) -> TelegramEmit {
        TelegramEmit {
            severity: Severity::Critical,
            error_code: code,
            stage: "csv_fetch",
        }
    }

    fn cfg_unreachable() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    #[test]
    fn test_map_error_code_to_fetch_outcome_covers_all_four() {
        assert_eq!(
            map_error_code_to_fetch_outcome(ErrorCode::InstrFetch01CsvHardFailed),
            Some(FetchOutcome::CsvHardFailed)
        );
        assert_eq!(
            map_error_code_to_fetch_outcome(ErrorCode::InstrFetch02SchemaValidationFailed),
            Some(FetchOutcome::SchemaValidationFailed)
        );
        assert_eq!(
            map_error_code_to_fetch_outcome(ErrorCode::InstrFetch03DanglingReferences),
            Some(FetchOutcome::DanglingReferences)
        );
        assert_eq!(
            map_error_code_to_fetch_outcome(ErrorCode::InstrFetch04UniverseSizeOutOfBounds),
            Some(FetchOutcome::UniverseSizeOutOfBounds)
        );
    }

    #[test]
    fn test_map_error_code_to_fetch_outcome_none_for_non_fetch_code() {
        // A non-INSTR-FETCH code must NOT be silently mapped to a fetch
        // outcome — we refuse to write a misleading audit row.
        assert_eq!(
            map_error_code_to_fetch_outcome(ErrorCode::Boot01QuestDbSlow),
            None
        );
    }

    #[test]
    fn test_build_attempt_failure_row_populates_outcome_and_code() {
        let emit = emit_with(ErrorCode::InstrFetch01CsvHardFailed);
        let row = build_attempt_failure_row(
            &emit,
            7,
            1_700_000_000_000_000_000,
            1_699_920_000_000_000_000,
            false,
            "dhan 503",
        )
        .expect("INSTR-FETCH-01 must produce a row");
        assert_eq!(row.outcome, FetchOutcome::CsvHardFailed);
        assert_eq!(row.error_code, "INSTR-FETCH-01");
        assert_eq!(row.attempt, 7);
        assert_eq!(row.total_rows, 0);
        assert_eq!(row.universe_size, 0);
        assert_eq!(row.source_csv_sha256, "");
        assert_eq!(row.detail, "dhan 503");
    }

    #[test]
    fn test_build_attempt_failure_row_none_for_non_fetch_code() {
        let emit = emit_with(ErrorCode::Boot01QuestDbSlow);
        let row = build_attempt_failure_row(&emit, 4, 0, 0, false, "x");
        assert!(
            row.is_none(),
            "non-fetch code must not produce a failure row"
        );
    }

    #[test]
    fn test_build_terminal_success_row_populates_counts_and_attempt() {
        let outcome = LoopOutcome::Success { attempts_used: 3 };
        let row = build_terminal_success_row(
            &outcome,
            1_700_000_000_000_000_000,
            1_699_920_000_000_000_000,
            142_350,
            772,
            30,
            218,
            524,
            "deadbeef",
            false,
        )
        .expect("Success outcome must produce a row");
        assert_eq!(row.outcome, FetchOutcome::Success);
        assert_eq!(row.attempt, 3);
        assert_eq!(row.error_code, "");
        assert_eq!(row.index_count, 30);
        assert_eq!(row.underlying_count, 218);
        // §31 NTM: universe = index(30) + underlying(218) + index_constituent(524) = 772.
        assert_eq!(row.universe_size, 772);
        assert_eq!(row.total_rows, 142_350);
        assert_eq!(row.source_csv_sha256, "deadbeef");
    }

    #[test]
    fn test_build_terminal_success_row_three_role_partition_holds() {
        // §31 NTM: the universe partitions into THREE mutually-exclusive roles.
        // Sum must equal universe_size (the debug_assert guard inside the builder).
        let outcome = LoopOutcome::Success { attempts_used: 1 };
        let index = 33;
        let underlying = 218;
        let index_constituent = 524;
        let total = index + underlying + index_constituent;
        let row = build_terminal_success_row(
            &outcome,
            0,
            0,
            0,
            total,
            index,
            underlying,
            index_constituent,
            "abc",
            false,
        )
        .expect("3-role partition must produce a row");
        assert_eq!(row.universe_size, total);
        assert_eq!(row.index_count, index);
        assert_eq!(row.underlying_count, underlying);
    }

    #[test]
    fn test_build_terminal_success_row_none_for_exhausted() {
        // Production never terminates on Exhausted (§4 infinite retry);
        // the test-only path must not produce a Success row.
        let outcome = LoopOutcome::Exhausted { attempts_used: 1 };
        let row = build_terminal_success_row(&outcome, 0, 0, 0, 0, 0, 0, 0, "", false);
        assert!(row.is_none(), "Exhausted must not map to a Success row");
    }

    #[test]
    fn test_build_terminal_success_row_propagates_dry_run() {
        let outcome = LoopOutcome::Success { attempts_used: 1 };
        let row =
            build_terminal_success_row(&outcome, 0, 0, 0, 248, 30, 218, 0, "", true).expect("row");
        assert!(row.dry_run, "dry_run flag must propagate to the row");
    }

    #[tokio::test]
    async fn test_record_attempt_failure_returns_err_when_questdb_unreachable() {
        let cfg = cfg_unreachable();
        let emit = emit_with(ErrorCode::InstrFetch02SchemaValidationFailed);
        let result = record_attempt_failure(&cfg, &emit, 5, 0, 0, false, "schema drift").await;
        assert!(result.is_err(), "must propagate transport error");
    }

    #[tokio::test]
    async fn test_record_attempt_failure_noop_ok_for_non_fetch_code() {
        // A non-fetch code is a no-op Ok — nothing to audit, no transport
        // attempt, so even an unreachable host returns Ok.
        let cfg = cfg_unreachable();
        let emit = emit_with(ErrorCode::Boot01QuestDbSlow);
        let result = record_attempt_failure(&cfg, &emit, 5, 0, 0, false, "x").await;
        assert!(
            result.is_ok(),
            "non-fetch code must be a no-op Ok (no write attempted)"
        );
    }

    #[tokio::test]
    async fn test_record_terminal_success_returns_err_when_questdb_unreachable() {
        let cfg = cfg_unreachable();
        let outcome = LoopOutcome::Success { attempts_used: 1 };
        let result =
            record_terminal_success(&cfg, &outcome, 0, 0, 100, 248, 30, 218, 0, "sha", false).await;
        assert!(result.is_err(), "must propagate transport error");
    }

    #[tokio::test]
    async fn test_record_terminal_success_noop_ok_for_exhausted() {
        let cfg = cfg_unreachable();
        let outcome = LoopOutcome::Exhausted { attempts_used: 1 };
        let result = record_terminal_success(&cfg, &outcome, 0, 0, 0, 0, 0, 0, 0, "", false).await;
        assert!(
            result.is_ok(),
            "Exhausted must be a no-op Ok (no write attempted)"
        );
    }
}
