//! `instrument_fetch_audit` table contract — Sub-PR #10b-ε of the
//! 2026-05-27 daily-universe expansion.
//!
//! **Status:** CONTRACT STUBS ONLY. This module ships:
//!
//! * [`QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT`] — wire-format table name
//! * [`DEDUP_KEY_INSTRUMENT_FETCH_AUDIT`] — composite DEDUP UPSERT KEYS clause
//! * [`FetchOutcome`] — typed SYMBOL column wire-format labels
//!
//! The DDL string, `ensure_*_table` helper, `append_*_row` helper, and
//! the row struct land in Sub-PR #10b-ζ alongside the boot-orchestrator
//! callsite. Splitting the contract from the DDL/append helpers keeps
//! each sub-PR focused per the operator's "one PR at a time + small
//! focused changes" preference (operator-charter-forever.md §H).
//!
//! **Feature-gated.** Compiles only when the `daily_universe_fetcher`
//! Cargo feature is enabled, mirroring the `tickvault-core` gate per
//! `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
//! §21. Activation lands when Sub-PR #10b-ζ + Sub-PR #10 (boot
//! orchestrator) flip the feature on; until then the contract surface is
//! dormant and downstream `use` paths see "no such module" cleanly.
//!
//! # Purpose
//!
//! The daily-universe fetch chain (Sub-PR #3 → #4 → #5 → #6 → #7 → #10)
//! drives boot from raw bytes → validated CSV → unique underlyings →
//! indices → ~250-SID universe. Every attempt — success, declared
//! holiday, the 4 INSTR-FETCH-* failure classes, operator override
//! (§20), and dry-run (§27) — gets a forensic row in this SEBI-relevant
//! audit table. 5-year retention via the existing
//! `partition_manager` lifecycle (90d hot QuestDB → S3 IT → Glacier
//! Deep Archive).
//!
//! # DEDUP key contract
//!
//! `(trading_date_ist, outcome, attempt, ts)` — 4 columns. Each column
//! is mandatory:
//!
//! * `trading_date_ist` — QuestDB designated-timestamp invariant
//!   (regression class 2026-04-28: omitting the designated timestamp
//!   returns HTTP 400 from `/exec` at boot, silently breaking the
//!   table).
//! * `outcome` — without this, a `Success` outcome on the SAME trading
//!   day would overwrite the earlier `CsvHardFailed` retry-failure
//!   audit rows. The forensic chain MUST preserve both — operators
//!   answer "how many CSV fetches failed before today's success?" by
//!   counting non-success outcomes for the day.
//! * `attempt` — without this, two `CsvHardFailed` rows for the SAME
//!   trading day (e.g. attempt 3 failed at 08:33 IST, attempt 7 failed
//!   at 08:39 IST) would collapse to a single row, losing the §4
//!   retry-ladder forensic detail.
//! * `ts` — minute-level (or finer) timestamp; pairs with `attempt` to
//!   preserve rapid-succession rows when two attempts of the same kind
//!   land in the same second.
//!
//! # I-P1-11 stub
//!
//! The current contract does NOT carry `security_id` (the audit row
//! describes the FETCH ATTEMPT, not a single instrument). If a future
//! sub-PR adds `security_id` to the row — e.g. for per-instrument
//! validation-failure forensics — the DEDUP key MUST be extended with
//! `exchange_segment` per `.claude/rules/project/security-id-uniqueness.md`
//! and the corresponding I-P1-11 ratchet. The
//! [`test_dedup_key_segment_pairing_invariant_if_security_id_ever_added`]
//! unit test is the watchdog.
//!
//! # Cross-references
//!
//! * Rule §3 + §22 — `daily-universe-scope-expansion-2026-05-27.md`
//! * Rule §0 — `daily-universe-instr-fetch-error-codes.md`
//! * Rule §21 — feature-gate contract (compile-time scope barrier)
//! * `ErrorCode::InstrFetch01..04` — failure classes routed through
//!   the matching `FetchOutcome::*` SYMBOL value.

#![cfg(feature = "daily_universe_fetcher")]

/// Wire-format table name. Stable across releases — operators,
/// dashboards, and the `partition_manager` S3 archive job depend on
/// the exact string. Pinned by
/// [`tests::test_table_name_constant`].
pub const QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT: &str = "instrument_fetch_audit";

/// Composite DEDUP key. Four columns covering the full identity of a
/// per-attempt fetch outcome. See module docs for the full rationale.
///
/// Order rationale: `trading_date_ist` first matches the designated
/// timestamp partition column; `outcome` second so the SYMBOL column
/// presence dominates the equality check before the higher-cardinality
/// `attempt` integer; `ts` last because microsecond-level granularity is
/// the tie-breaker for rapid-succession rows of the same kind.
pub const DEDUP_KEY_INSTRUMENT_FETCH_AUDIT: &str = "trading_date_ist, outcome, attempt, ts";

/// Stable wire-format strings for the `outcome` SYMBOL column. Operators
/// query this column by exact string match in QuestDB SQL and
/// CloudWatch dashboards; bumping any label is a breaking change
/// requiring a coordinated dashboard + alarm update.
///
/// # Variant taxonomy
///
/// | Variant | Class | Meaning |
/// |---|---|---|
/// | `Success` | terminal-ok | CSV fetched + parsed + universe built |
/// | `HolidayObservation` | terminal-ok | Trading-calendar holiday — single non-retrying fetch attempt per §22 |
/// | `CsvHardFailed` | retryable-failure | Maps to `ErrorCode::InstrFetch01CsvHardFailed` |
/// | `SchemaValidationFailed` | retryable-failure | Maps to `ErrorCode::InstrFetch02SchemaValidationFailed` |
/// | `DanglingReferences` | retryable-failure | Maps to `ErrorCode::InstrFetch03DanglingReferences` |
/// | `UniverseSizeOutOfBounds` | retryable-failure | Maps to `ErrorCode::InstrFetch04UniverseSizeOutOfBounds` |
/// | `OperatorOverride` | other | Operator paste of yesterday's SHA-256 per §20 escape valve |
/// | `DryRun` | other | `--dry-run-universe` flag per §27 — fetch + validate but NO subscription dispatch |
///
/// Exactly 8 variants — pinned by
/// [`tests::test_eight_distinct_wire_format_labels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FetchOutcome {
    /// CSV fetched, parsed, universe built, lifecycle reconciled.
    /// Subscription dispatch proceeds.
    Success,
    /// Trading-calendar holiday — orchestrator made one non-retrying
    /// attempt (§22) so the audit trail records the day, but no
    /// subscription dispatch is expected.
    HolidayObservation,
    /// INSTR-FETCH-01: CSV download retry budget exhausted at the
    /// `Critical` tier (§4 attempt 11+). The §4 retry loop is still
    /// active — this row marks the escalation moment, not give-up.
    CsvHardFailed,
    /// INSTR-FETCH-02: CSV parse rejected. Header column missing,
    /// non-UTF-8 bytes, or >0.1% mandatory-field row failures.
    SchemaValidationFailed,
    /// INSTR-FETCH-03: >0.5% of FUTSTK/OPTSTK rows reference an
    /// `UNDERLYING_SECURITY_ID` not present in any NSE_EQ row in the
    /// same CSV.
    DanglingReferences,
    /// INSTR-FETCH-04: Computed universe outside the
    /// `[MIN_DAILY_UNIVERSE_SIZE, MAX_DAILY_UNIVERSE_SIZE]` envelope.
    UniverseSizeOutOfBounds,
    /// Operator pasted yesterday's SHA-256 via
    /// `--operator-acknowledge-stale-csv` (§20 escape valve). Boot
    /// proceeded against yesterday's `instrument_lifecycle` snapshot
    /// for one trading day; row carries the operator's required free
    /// text in a follow-on column when Sub-PR #10b-ζ extends the schema.
    OperatorOverride,
    /// `--dry-run-universe` invocation per §27. Fetch + validate +
    /// compute universe completed, audit row written, but the
    /// subscription dispatcher was NOT invoked. Used on first
    /// production boot to verify the pipeline end-to-end without
    /// committing to live ticks.
    DryRun,
}

impl FetchOutcome {
    /// Returns the wire-format SYMBOL label. ALWAYS lowercase
    /// snake_case — matches the convention used by every other
    /// `*_audit` SYMBOL column in the storage crate
    /// (cf. `FetchOutcome::Fresh` → `"fresh"` in
    /// `option_chain_minute_snapshot_persistence.rs`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::HolidayObservation => "holiday_observation",
            Self::CsvHardFailed => "csv_hard_failed",
            Self::SchemaValidationFailed => "schema_validation_failed",
            Self::DanglingReferences => "dangling_references",
            Self::UniverseSizeOutOfBounds => "universe_size_out_of_bounds",
            Self::OperatorOverride => "operator_override",
            Self::DryRun => "dry_run",
        }
    }

    /// Predicate — true only for outcomes that represent a CLEAN end
    /// of the day's fetch attempt: `Success` (universe live) or
    /// `HolidayObservation` (no universe expected). `OperatorOverride`
    /// and `DryRun` are explicitly NOT terminal-ok — both require
    /// operator review and neither commits today's universe to the
    /// normal live path.
    ///
    /// Pinned exhaustively by
    /// [`tests::test_is_terminal_ok_only_covers_success_and_holiday`].
    #[must_use]
    pub const fn is_terminal_ok(self) -> bool {
        matches!(self, Self::Success | Self::HolidayObservation)
    }

    /// Predicate — true only for the 4 `INSTR-FETCH-*` outcomes that
    /// trigger the §4 infinite-retry ladder. `OperatorOverride` and
    /// `DryRun` are NOT retryable failures (they're explicit operator
    /// invocations).
    ///
    /// Pinned exhaustively by
    /// [`tests::test_is_retryable_failure_only_covers_four_instr_fetch_codes`].
    #[must_use]
    pub const fn is_retryable_failure(self) -> bool {
        matches!(
            self,
            Self::CsvHardFailed
                | Self::SchemaValidationFailed
                | Self::DanglingReferences
                | Self::UniverseSizeOutOfBounds
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every wire-format variant the column SYMBOL set should ever
    /// carry. The audit-trail forensic guarantee depends on this list
    /// being exhaustive.
    const ALL_VARIANTS: &[FetchOutcome] = &[
        FetchOutcome::Success,
        FetchOutcome::HolidayObservation,
        FetchOutcome::CsvHardFailed,
        FetchOutcome::SchemaValidationFailed,
        FetchOutcome::DanglingReferences,
        FetchOutcome::UniverseSizeOutOfBounds,
        FetchOutcome::OperatorOverride,
        FetchOutcome::DryRun,
    ];

    #[test]
    fn test_table_name_constant() {
        // Wire-format stability — operators + dashboards + the S3
        // archive job depend on the exact string.
        assert_eq!(
            QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT,
            "instrument_fetch_audit"
        );
    }

    #[test]
    fn test_dedup_key_lists_trading_date_outcome_attempt_ts() {
        // Regression class 2026-04-28: QuestDB requires the designated
        // timestamp column in `DEDUP UPSERT KEYS(...)` or the CREATE
        // TABLE returns HTTP 400 from `/exec` at boot and the table is
        // silently absent. `outcome` + `attempt` are the forensic
        // dimensions — without them, retry-ladder rows collapse and
        // the SEBI audit chain loses the "how many failures before
        // success?" answer.
        assert!(
            DEDUP_KEY_INSTRUMENT_FETCH_AUDIT.contains("trading_date_ist"),
            "DEDUP key must include trading_date_ist (QuestDB designated timestamp invariant)"
        );
        assert!(
            DEDUP_KEY_INSTRUMENT_FETCH_AUDIT.contains("outcome"),
            "DEDUP key must include outcome — otherwise Success would overwrite earlier failure rows"
        );
        assert!(
            DEDUP_KEY_INSTRUMENT_FETCH_AUDIT.contains("attempt"),
            "DEDUP key must include attempt — otherwise two same-kind failures on the same day collapse"
        );
        let has_ts_token = DEDUP_KEY_INSTRUMENT_FETCH_AUDIT
            .split([',', ' '])
            .map(str::trim)
            .any(|tok| tok == "ts");
        assert!(
            has_ts_token,
            "DEDUP key must include the bare `ts` token \
             (regression class 2026-04-28: QuestDB rejects DDL without designated-timestamp column)"
        );
    }

    #[test]
    fn test_dedup_key_has_exactly_four_columns() {
        // Lock the contract surface — Sub-PR #10b-ζ's DDL must match.
        // Comma-count + 1 is the simplest invariant check.
        let column_count = DEDUP_KEY_INSTRUMENT_FETCH_AUDIT.matches(',').count() + 1;
        assert_eq!(
            column_count, 4,
            "DEDUP key must have exactly 4 columns; got {column_count} in \"{DEDUP_KEY_INSTRUMENT_FETCH_AUDIT}\""
        );
    }

    #[test]
    fn test_dedup_key_segment_pairing_invariant_if_security_id_ever_added() {
        // I-P1-11 watchdog: the current contract does NOT carry
        // `security_id`. If a future sub-PR adds it (e.g. for
        // per-instrument validation-failure forensics), the DEDUP key
        // MUST be extended with `exchange_segment` (or `segment` /
        // `exchange`) per the security-id-uniqueness rule. Without
        // that, two distinct instruments with the same Dhan-reused id
        // would silently collapse into one audit row.
        //
        // This test passes vacuously today (no `security_id`) AND
        // catches the future regression — if anyone adds `security_id`
        // here without also adding `exchange_segment`, the assertion
        // fires.
        if DEDUP_KEY_INSTRUMENT_FETCH_AUDIT.contains("security_id") {
            let has_segment = DEDUP_KEY_INSTRUMENT_FETCH_AUDIT.contains("segment")
                || DEDUP_KEY_INSTRUMENT_FETCH_AUDIT.contains("exchange_segment")
                || DEDUP_KEY_INSTRUMENT_FETCH_AUDIT.contains("exchange");
            assert!(
                has_segment,
                "I-P1-11 violation — DEDUP key adds `security_id` without `segment`/\
                 `exchange_segment`/`exchange`. See \
                 .claude/rules/project/security-id-uniqueness.md"
            );
        }
    }

    #[test]
    fn test_eight_distinct_wire_format_labels() {
        // Forensic chain requires every variant to have a UNIQUE wire
        // label. Duplicates would silently merge attempt rows.
        assert_eq!(
            ALL_VARIANTS.len(),
            8,
            "exactly 8 FetchOutcome variants — Success + HolidayObservation + 4 INSTR-FETCH-* + OperatorOverride + DryRun"
        );
        let mut seen: Vec<&'static str> = Vec::with_capacity(ALL_VARIANTS.len());
        for v in ALL_VARIANTS {
            let label = v.as_str();
            assert!(
                !seen.contains(&label),
                "duplicate wire-format label `{label}` — every variant must map to a distinct SYMBOL value"
            );
            seen.push(label);
        }
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn test_wire_format_is_lowercase_snake_case() {
        // Cross-crate convention: every `*_audit` SYMBOL column uses
        // lowercase snake_case. Bumping any label to PascalCase / kebab
        // would break dashboards + SQL operator-cheats.
        for v in ALL_VARIANTS {
            let label = v.as_str();
            assert!(
                !label.is_empty(),
                "wire-format label for {v:?} must not be empty"
            );
            for ch in label.chars() {
                let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_';
                assert!(
                    ok,
                    "wire-format label `{label}` for {v:?} contains non-snake_case char `{ch}`; \
                     allowed: [a-z0-9_]"
                );
            }
            assert!(
                !label.starts_with('_') && !label.ends_with('_'),
                "wire-format label `{label}` must not start or end with underscore"
            );
            assert!(
                !label.contains("__"),
                "wire-format label `{label}` must not contain double underscore"
            );
        }
    }

    #[test]
    fn test_is_terminal_ok_only_covers_success_and_holiday() {
        // Pin the predicate exhaustively. A future 9th variant must
        // NOT silently be classified terminal-ok by downstream
        // metric panels.
        assert!(FetchOutcome::Success.is_terminal_ok());
        assert!(FetchOutcome::HolidayObservation.is_terminal_ok());
        assert!(!FetchOutcome::CsvHardFailed.is_terminal_ok());
        assert!(!FetchOutcome::SchemaValidationFailed.is_terminal_ok());
        assert!(!FetchOutcome::DanglingReferences.is_terminal_ok());
        assert!(!FetchOutcome::UniverseSizeOutOfBounds.is_terminal_ok());
        assert!(!FetchOutcome::OperatorOverride.is_terminal_ok());
        assert!(!FetchOutcome::DryRun.is_terminal_ok());

        // Double-check by counting the truthy set.
        let terminal_ok_count = ALL_VARIANTS.iter().filter(|v| v.is_terminal_ok()).count();
        assert_eq!(
            terminal_ok_count, 2,
            "is_terminal_ok must cover EXACTLY Success + HolidayObservation"
        );
    }

    #[test]
    fn test_is_retryable_failure_only_covers_four_instr_fetch_codes() {
        // The §4 infinite-retry ladder fires on these 4 outcomes and
        // ONLY these. OperatorOverride is the manual escape valve;
        // DryRun is an explicit non-retry mode.
        assert!(FetchOutcome::CsvHardFailed.is_retryable_failure());
        assert!(FetchOutcome::SchemaValidationFailed.is_retryable_failure());
        assert!(FetchOutcome::DanglingReferences.is_retryable_failure());
        assert!(FetchOutcome::UniverseSizeOutOfBounds.is_retryable_failure());
        assert!(!FetchOutcome::Success.is_retryable_failure());
        assert!(!FetchOutcome::HolidayObservation.is_retryable_failure());
        assert!(!FetchOutcome::OperatorOverride.is_retryable_failure());
        assert!(!FetchOutcome::DryRun.is_retryable_failure());

        let retryable_count = ALL_VARIANTS
            .iter()
            .filter(|v| v.is_retryable_failure())
            .count();
        assert_eq!(
            retryable_count, 4,
            "is_retryable_failure must cover EXACTLY the 4 INSTR-FETCH-* codes"
        );
    }

    #[test]
    fn test_terminal_ok_and_retryable_failure_are_mutually_exclusive() {
        // A variant must never be both terminal-ok AND a retryable
        // failure — that would make the outcome ambiguous to any
        // downstream observability code that branches on the two
        // predicates. OperatorOverride + DryRun deliberately fall
        // into NEITHER bucket (they need bespoke routing).
        for v in ALL_VARIANTS {
            assert!(
                !(v.is_terminal_ok() && v.is_retryable_failure()),
                "{v:?} is classified as BOTH terminal-ok AND retryable-failure — must be one or the other or neither"
            );
        }
    }

    #[test]
    fn test_eq_and_hash_derived() {
        // The audit row writer (Sub-PR #10b-ζ) will use FetchOutcome
        // as a HashMap key for per-outcome counter aggregation;
        // deriving Eq + Hash is part of the contract.
        use std::collections::HashSet;
        let mut set: HashSet<FetchOutcome> = HashSet::new();
        for v in ALL_VARIANTS {
            assert!(
                set.insert(*v),
                "duplicate variant {v:?} inserted into HashSet"
            );
        }
        assert_eq!(set.len(), ALL_VARIANTS.len());
        // PartialEq self-check.
        assert_eq!(FetchOutcome::Success, FetchOutcome::Success);
        assert_ne!(FetchOutcome::Success, FetchOutcome::HolidayObservation);
    }
}
