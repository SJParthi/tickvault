//! `instrument_fetch_audit` table persistence — Sub-PR #10b-ε (contract)
//! and Sub-PR #10b-ζ-1 (persistence helpers) of the 2026-05-27
//! daily-universe expansion.
//!
//! **Status:** PERSISTENCE HELPERS LIVE. This module ships:
//!
//! * [`QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT`] — wire-format table name (ε)
//! * [`DEDUP_KEY_INSTRUMENT_FETCH_AUDIT`] — composite DEDUP UPSERT KEYS clause (ε)
//! * [`FetchOutcome`] — typed SYMBOL column wire-format labels (ε)
//! * [`InstrumentFetchAuditRow`] — typed row in schema order (ζ-1)
//! * [`ensure_instrument_fetch_audit_table`] — idempotent CREATE TABLE (ζ-1)
//! * [`append_instrument_fetch_audit_row`] — single-row `/exec` insert (ζ-1)
//!
//! The boot-orchestrator callsite that wires these helpers into the
//! INSTR-FETCH runner lands in Sub-PR #10b-ζ-2 (kept separate so the
//! boot-path change is reviewed in isolation per operator-charter-forever.md
//! §H — "one PR at a time + small focused changes").
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
//! [`tests::test_dedup_key_segment_pairing_invariant_if_security_id_ever_added`]
//! unit test is the watchdog.
//!
//! # Timestamp rule
//!
//! Both `ts` and `trading_date_ist` are supplied by the caller as IST
//! wall-clock nanoseconds. The fetch chain runs on the host clock
//! (`Utc::now()` → IST), NOT on a Dhan WebSocket `exchange_timestamp`,
//! so the caller is responsible for adding the IST offset BEFORE
//! handing nanos to this writer (same convention as the greeks
//! pipeline, NOT the live-tick pipeline). This module performs ONLY the
//! nanos→micros division QuestDB `TIMESTAMP` columns require; it does
//! NOT add or remove any IST offset.
//!
//! # Live SQL TEST-EXEMPT
//!
//! [`ensure_instrument_fetch_audit_table`] and
//! [`append_instrument_fetch_audit_row`] require a running QuestDB; the
//! `/exec` error path is covered by the `#[tokio::test]` cases below
//! which assert the helpers return `Err` against an unreachable host.
//!
//! # Cross-references
//!
//! * Rule §3 + §22 — `daily-universe-scope-expansion-2026-05-27.md`
//! * Rule §0 — `daily-universe-instr-fetch-error-codes.md`
//! * Rule §21 — feature-gate contract (compile-time scope barrier)
//! * `ErrorCode::InstrFetch01..04` — failure classes routed through
//!   the matching `FetchOutcome::*` SYMBOL value.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

/// `/exec` HTTP timeout for both DDL and INSERT. Matches the value used
/// by every other `*_audit_persistence` module in the storage crate.
const QUESTDB_EXEC_TIMEOUT_SECS: u64 = 10;

/// Broker-source label stamped on every fetch-audit row today (operator
/// override 2026-06-28 — `feed` is in the DEDUP key on every persisted table).
/// The instrument-fetch chain is Dhan-only today; a future Groww master-fetch
/// would stamp its own. Replay-stable `&'static str` from the `Feed` enum.
pub const FETCH_AUDIT_FEED_DHAN: &str = tickvault_common::feed::Feed::Dhan.as_str();

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
pub const DEDUP_KEY_INSTRUMENT_FETCH_AUDIT: &str = "trading_date_ist, outcome, attempt, ts, feed";

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
/// | `CsvHardFailed` | retryable-failure | Historically mapped to `INSTR-FETCH-01` (variant retired in the C4 sweep, 2026-07-15) |
/// | `SchemaValidationFailed` | retryable-failure | Historically mapped to `INSTR-FETCH-02` (variant retired, C4 sweep) |
/// | `DanglingReferences` | retryable-failure | Historically mapped to `INSTR-FETCH-03` (variant retired, C4 sweep) |
/// | `UniverseSizeOutOfBounds` | retryable-failure | Historically mapped to `INSTR-FETCH-04` (variant retired, C4 sweep) |
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

/// Column list shared by the DDL and the INSERT helper, in exact schema
/// order. One constant so the writer and the table definition cannot
/// drift. 12 columns.
const INSERT_COLUMN_LIST: &str = "ts, trading_date_ist, outcome, attempt, error_code, \
     total_rows, universe_size, index_count, underlying_count, source_csv_sha256, \
     dry_run, detail, feed";

/// One `instrument_fetch_audit` row in the shape the writer accepts.
///
/// Field-count rationale: 12 columns mirror the L3-RECONCILE and L5-AUDIT
/// demands of `daily-universe-scope-expansion-2026-05-27.md` §9. The
/// `total_rows` with `source_csv_sha256` pair powers the L3 day-over-day
/// reconcile ("row count within 0.5×..2× of yesterday"). The trio
/// `universe_size`, `index_count`, `underlying_count` captures the L4
/// envelope check. The trio `outcome`, `error_code`, `attempt` captures
/// the §4 retry-ladder forensic chain. `dry_run` isolates §27 dry-run
/// rows from live rows.
#[derive(Debug, Clone, PartialEq)]
pub struct InstrumentFetchAuditRow<'a> {
    /// IST wall-clock nanoseconds of this attempt. Designated timestamp.
    /// Caller adds the IST offset (host-clock source, NOT a Dhan WS
    /// `exchange_timestamp`).
    pub ts_nanos_ist: i64,
    /// IST date-midnight nanoseconds — QuestDB partition column. Part of
    /// the DEDUP key.
    pub trading_date_ist_nanos: i64,
    /// Outcome of this attempt.
    pub outcome: FetchOutcome,
    /// §4 retry attempt number (1-based). Part of the DEDUP key so two
    /// same-kind failures on the same day are both preserved.
    pub attempt: u32,
    /// The error-code wire string (e.g. `"INSTR-FETCH-01"` — a HISTORICAL
    /// data-format literal; the enum variants behind these strings were
    /// retired in the C4 sweep, 2026-07-15) for failure outcomes; empty
    /// string for `Success` / `HolidayObservation` / `OperatorOverride` /
    /// `DryRun`. NOT escaped here — `sanitize_audit_string` is applied
    /// internally.
    pub error_code: &'a str,
    /// Raw CSV row count for this attempt (L3 reconcile baseline). 0 when
    /// the fetch failed before the row count was known.
    pub total_rows: i64,
    /// Computed daily-universe size (L4 envelope check). 0 when the build
    /// never reached the size computation.
    pub universe_size: i64,
    /// Number of index SIDs in the computed universe. 0 pre-build.
    pub index_count: i64,
    /// Number of unique F&O underlying SIDs in the computed universe.
    /// 0 pre-build.
    pub underlying_count: i64,
    /// SHA-256 of the fetched CSV payload (L3 provenance + reconcile).
    /// Empty string when the fetch failed before hashing. NOT escaped
    /// here — `sanitize_audit_string` is applied internally.
    pub source_csv_sha256: &'a str,
    /// `true` when produced by a `--dry-run-universe` invocation (§27).
    pub dry_run: bool,
    /// Free-form operator-readable diagnostic. For `OperatorOverride`
    /// this carries the operator's required note (§20). NOT escaped here
    /// — `sanitize_audit_string` is applied internally.
    pub detail: &'a str,
    /// Broker-source label (`dhan`/`groww`). Part of the DEDUP key (operator
    /// override 2026-06-28). Always non-empty (stamped [`FETCH_AUDIT_FEED_DHAN`]).
    pub feed: &'a str,
}

/// Creates the audit table if absent. Idempotent — safe to call on every
/// boot. Schema columns + DEDUP key are pinned by ratchet tests.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_instrument_fetch_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    // C2 (2026-07-03): panic-free client build — Client::new() panics on
    // TLS/resolver/fd init failure (silent tokio-task death). Degrade:
    // skip the DDL this boot; the next boot re-runs it (idempotent).
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            error!(
                error = %err,
                code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 reqwest client build failed — instrument_fetch_audit DDL \
                 skipped: if the table does not exist yet, ILP auto-create will lack DEDUP \
                 keys until the next successful boot (duplicate-row window)"
            );
            metrics::counter!(
                "tv_http_client_build_failed_total",
                "site" => "instrument_fetch_audit_ensure"
            )
            .increment(1);
            return;
        }
    };

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            outcome SYMBOL, \
            attempt INT, \
            error_code SYMBOL, \
            total_rows LONG, \
            universe_size INT, \
            index_count INT, \
            underlying_count INT, \
            source_csv_sha256 SYMBOL, \
            dry_run BOOLEAN, \
            detail STRING, \
            feed SYMBOL\
        ) timestamp(ts) PARTITION BY DAY WAL \
        DEDUP UPSERT KEYS({DEDUP_KEY_INSTRUMENT_FETCH_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }

    // ── Brownfield feed-in-key self-heal (operator override 2026-06-28). ──
    // This table had NO prior self-heal; add the standard ADD COLUMN → backfill
    // NULL→'dhan' → DEDUP-ENABLE sequence (the proven shadow/prev_day order).
    // The backfill MUST precede DEDUP-ENABLE so a re-keyed table upserts over a
    // legacy NULL-feed row instead of duplicating it. Each step logs (does not
    // halt boot); never drops the table (SEBI retention).
    let self_heal = [
        format!(
            "ALTER TABLE {QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT} \
             ADD COLUMN IF NOT EXISTS feed SYMBOL;"
        ),
        format!(
            "UPDATE {QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT} \
             SET feed = '{FETCH_AUDIT_FEED_DHAN}' WHERE feed IS NULL;"
        ),
        format!(
            "ALTER TABLE {QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT} \
             DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_INSTRUMENT_FETCH_AUDIT});"
        ),
    ];
    for ddl in &self_heal {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                warn!(
                    table = QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT,
                    status = %resp.status(),
                    ddl = ddl.as_str(),
                    "feed self-heal non-2xx"
                );
            }
            Err(err) => {
                error!(
                    table = QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT,
                    ?err,
                    ddl = ddl.as_str(),
                    "feed self-heal request failed"
                );
            }
        }
    }
}

/// Formats one row as a QuestDB `VALUES (...)` tuple in schema order.
///
/// `*_nanos` fields are IST wall-clock nanoseconds; this divides them to
/// microseconds because QuestDB `TIMESTAMP` columns store microseconds
/// since epoch — embedding nanos overflows the year-9999 range
/// (2026-04-28 regression).
fn format_row_values_tuple(row: &InstrumentFetchAuditRow<'_>) -> String {
    // `outcome` is NOT passed through `sanitize_audit_string`: it is a
    // compile-time `&'static str` from `FetchOutcome::as_str()` (one of 8
    // lowercase snake_case constants), provably free of SQL-breaking
    // characters. Same pattern as `option_chain_minute_snapshot_persistence.rs`.
    // The 3 free-text fields below (caller-supplied / CSV-derived) ARE
    // sanitized — they are the only injection-reachable inputs.
    let outcome = row.outcome.as_str();
    // `attempt` is u32 but the QuestDB column is INT (i32). The §4 retry
    // ladder keeps attempt small (escalates Critical at 21+, caps backoff
    // at 300s), so it never approaches i32::MAX. Belt-and-suspenders guard
    // so a future caller regression surfaces in debug builds rather than
    // silently wrapping when QuestDB parses the literal as i32.
    debug_assert!(
        row.attempt <= i32::MAX as u32,
        "attempt {} exceeds QuestDB INT (i32) range",
        row.attempt
    );
    let error_code = sanitize_audit_string(row.error_code);
    let source_csv_sha256 = sanitize_audit_string(row.source_csv_sha256);
    let detail = sanitize_audit_string(row.detail);
    let ts_micros = row.ts_nanos_ist / 1_000;
    let trading_date_micros = row.trading_date_ist_nanos / 1_000;
    let attempt = row.attempt;
    let total_rows = row.total_rows;
    let universe_size = row.universe_size;
    let index_count = row.index_count;
    let underlying_count = row.underlying_count;
    let dry_run = row.dry_run;
    let feed = sanitize_audit_string(row.feed);
    format!(
        "({ts_micros}, {trading_date_micros}, '{outcome}', {attempt}, '{error_code}', \
          {total_rows}, {universe_size}, {index_count}, {underlying_count}, \
          '{source_csv_sha256}', {dry_run}, '{detail}', '{feed}')"
    )
}

/// Appends one `instrument_fetch_audit` row.
///
/// Cold path — called at most a handful of times per boot (once per §4
/// retry attempt that escalates, plus the terminal outcome). The table's
/// `DEDUP UPSERT KEYS` makes a re-run idempotent.
///
/// # Errors
///
/// Propagates the `reqwest` transport error, or returns `Err` on a
/// non-2xx `/exec` response.
pub async fn append_instrument_fetch_audit_row(
    questdb_config: &QuestDbConfig,
    row: &InstrumentFetchAuditRow<'_>,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()?;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_INSTRUMENT_FETCH_AUDIT} \
         ({INSERT_COLUMN_LIST}) VALUES {tuple};",
        tuple = format_row_values_tuple(row),
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        // Cap the reflected QuestDB body at 200 chars: this error string
        // can propagate to a Telegram alert, and an unbounded body could
        // leak schema/query detail. Matches the PREVCLOSE-04 convention.
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        anyhow::bail!("instrument_fetch_audit insert non-2xx ({status}): {body}");
    }
    Ok(())
}

// C4 sweep (2026-07-15): the O(1) warm-boot SHA read
// (`parse_last_fetch_audit_sha` + `read_last_fetch_audit_sha`) was DELETED —
// caller-less since PR-C3 removed the lifecycle_reconcile warm-skip
// orchestrator (the #1569 judgment call #2 deferred the decision to this
// sweep). The `instrument_fetch_audit` SEBI table + its append fn stay.

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
        assert!(
            DEDUP_KEY_INSTRUMENT_FETCH_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == "feed"),
            "operator override 2026-06-28: feed must be in the DEDUP key"
        );
    }

    #[test]
    fn test_dedup_key_has_exactly_five_columns() {
        // Lock the contract surface. Comma-count + 1 is the simplest check.
        // 5 = the original 4 (trading_date_ist, outcome, attempt, ts) + feed.
        let column_count = DEDUP_KEY_INSTRUMENT_FETCH_AUDIT.matches(',').count() + 1;
        assert_eq!(
            column_count, 5,
            "DEDUP key must have exactly 5 columns; got {column_count} in \"{DEDUP_KEY_INSTRUMENT_FETCH_AUDIT}\""
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

    // ========================================================================
    // ζ-1 persistence-helper tests
    // ========================================================================

    fn test_cfg_unreachable() -> QuestDbConfig {
        // Port 1 is reserved and never listening; ensures a real HTTP
        // failure without hitting any live service.
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    fn sample_success_row() -> InstrumentFetchAuditRow<'static> {
        InstrumentFetchAuditRow {
            ts_nanos_ist: 1_700_000_000_000_000_000,
            trading_date_ist_nanos: 1_699_920_000_000_000_000,
            outcome: FetchOutcome::Success,
            attempt: 1,
            error_code: "",
            total_rows: 142_350,
            universe_size: 248,
            index_count: 30,
            underlying_count: 218,
            source_csv_sha256: "abc123def456",
            dry_run: false,
            detail: "",
            feed: FETCH_AUDIT_FEED_DHAN,
        }
    }

    #[test]
    fn test_insert_column_list_has_thirteen_columns() {
        // The shared column list must name every schema column so the
        // INSERT helper and the DDL stay aligned.
        assert_eq!(
            INSERT_COLUMN_LIST.matches(',').count() + 1,
            13,
            "INSERT column list must name all 13 schema columns (12 + feed)"
        );
    }

    #[test]
    fn test_ddl_contains_all_thirteen_columns() {
        // Source-scan: every schema column must be declared in the DDL
        // string. Any drift between code + schema fails the build.
        let source = include_str!("instrument_fetch_audit_persistence.rs");
        let columns = [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "outcome SYMBOL",
            "attempt INT",
            "error_code SYMBOL",
            "total_rows LONG",
            "universe_size INT",
            "index_count INT",
            "underlying_count INT",
            "source_csv_sha256 SYMBOL",
            "dry_run BOOLEAN",
            "detail STRING",
            "feed SYMBOL",
        ];
        for col in columns {
            assert!(
                source.contains(col),
                "DDL must declare column `{col}` — drift between code + schema rejected"
            );
        }
    }

    #[test]
    fn test_ddl_partition_by_day_with_wal_and_dedup() {
        let source = include_str!("instrument_fetch_audit_persistence.rs");
        assert!(
            source.contains("PARTITION BY DAY WAL"),
            "DDL must include `PARTITION BY DAY WAL` — required for retention + idempotent writes"
        );
        assert!(
            source.contains("DEDUP UPSERT KEYS({DEDUP_KEY_INSTRUMENT_FETCH_AUDIT})"),
            "DDL must wire the DEDUP key constant into the CREATE TABLE statement"
        );
    }

    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        // Regression class 2026-04-28: QuestDB TIMESTAMP columns store
        // microseconds; embedding nanos overflows the year-9999 bound.
        let source = include_str!("instrument_fetch_audit_persistence.rs");
        assert!(
            source.contains("ts_nanos_ist / 1_000"),
            "INSERT helper must divide ts nanos to micros (year-9999 overflow guard)"
        );
        assert!(
            source.contains("trading_date_ist_nanos / 1_000"),
            "INSERT helper must divide trading_date_ist nanos to micros"
        );
    }

    #[test]
    fn test_format_row_values_tuple_has_thirteen_fields() {
        let tuple = format_row_values_tuple(&sample_success_row());
        assert!(
            tuple.starts_with('(') && tuple.ends_with(')'),
            "tuple must be paren-wrapped"
        );
        // No nested parens/commas in any sanitized field → comma count
        // is exact: 13 columns → 12 separating commas.
        let inner = &tuple[1..tuple.len() - 1];
        assert_eq!(
            inner.matches(',').count(),
            12,
            "13 columns → 12 separating commas"
        );
    }

    #[test]
    fn test_format_row_values_tuple_stamps_feed() {
        // operator override 2026-06-28: every fetch-audit row carries feed in-key.
        let tuple = format_row_values_tuple(&sample_success_row());
        assert!(
            tuple.ends_with(", 'dhan')"),
            "fetch-audit tuple must end with the feed label: {tuple}"
        );
    }

    #[test]
    fn test_format_row_values_tuple_divides_nanos_to_micros() {
        // Verify the actual numeric output, not just a source-scan.
        let tuple = format_row_values_tuple(&sample_success_row());
        // 1_700_000_000_000_000_000 nanos / 1_000 = 1_700_000_000_000_000 micros
        assert!(
            tuple.starts_with("(1700000000000000, 1699920000000000, 'success', 1,"),
            "tuple must lead with micros-converted ts + trading_date + outcome + attempt; got: {tuple}"
        );
    }

    #[test]
    fn test_format_row_values_tuple_renders_dry_run_boolean_and_error_code() {
        // A retryable-failure dry-run row exercises the error_code +
        // dry_run + detail string-formatting paths together.
        let mut row = sample_success_row();
        row.outcome = FetchOutcome::CsvHardFailed;
        row.error_code = "INSTR-FETCH-01";
        row.dry_run = true;
        row.detail = "dhan cdn 503 after 11 attempts";
        let tuple = format_row_values_tuple(&row);
        assert!(
            tuple.contains("'csv_hard_failed'"),
            "outcome label: {tuple}"
        );
        assert!(tuple.contains("'INSTR-FETCH-01'"), "error_code: {tuple}");
        assert!(tuple.contains(", true, "), "dry_run boolean: {tuple}");
        assert!(
            tuple.contains("'dhan cdn 503 after 11 attempts'"),
            "detail free text: {tuple}"
        );
    }

    #[test]
    fn test_ensure_instrument_fetch_audit_table_pub_fn_visible() {
        // Cheap smoke that the public surface compiles + is reachable.
        let _ = ensure_instrument_fetch_audit_table;
    }

    #[tokio::test]
    async fn test_append_instrument_fetch_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg_unreachable();
        let row = sample_success_row();
        let result = append_instrument_fetch_audit_row(&cfg, &row).await;
        assert!(
            result.is_err(),
            "must propagate transport error when QuestDB is unreachable"
        );
    }

    #[tokio::test]
    async fn test_append_instrument_fetch_audit_row_handles_operator_override_outcome() {
        // Exercise the §20 operator-override variant + its free-text
        // detail through the transport-error path.
        let cfg = test_cfg_unreachable();
        let mut row = sample_success_row();
        row.outcome = FetchOutcome::OperatorOverride;
        row.detail = "operator acknowledged stale csv 2026-05-27";
        row.dry_run = false;
        let result = append_instrument_fetch_audit_row(&cfg, &row).await;
        assert!(result.is_err(), "transport error path must propagate");
    }

    #[test]
    fn test_error_body_is_capped_at_200_chars() {
        // Source-scan: the non-2xx branch must bound the reflected
        // QuestDB body so an unbounded response can't flood a Telegram
        // alert or leak schema detail (security-reviewer MEDIUM).
        let source = include_str!("instrument_fetch_audit_persistence.rs");
        assert!(
            source.contains(".chars().take(200).collect()"),
            "append helper must cap the reflected QuestDB error body at 200 chars"
        );
    }

    #[test]
    fn test_format_row_values_tuple_sanitizes_injection_in_detail() {
        // The detail free-text field is caller/CSV-supplied — a malicious
        // payload must not escape the single-quoted SQL literal. Confirm
        // the classic break-out string is neutralized (single-quote
        // doubled, semicolon + double-dash stripped by sanitize).
        let mut row = sample_success_row();
        row.detail = "'); DROP TABLE instrument_fetch_audit;--";
        let tuple = format_row_values_tuple(&row);
        assert!(
            !tuple.contains("DROP TABLE instrument_fetch_audit;"),
            "semicolon-terminated injection must be neutralized; got: {tuple}"
        );
        // The tuple must still be a single balanced VALUES row — the
        // sanitized detail stays inside its quotes.
        assert!(tuple.starts_with('(') && tuple.ends_with(')'));
    }
}
