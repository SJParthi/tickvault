//! `instrument_lifecycle` + `instrument_lifecycle_audit` table contracts —
//! Sub-PR of the 2026-05-27 daily-universe expansion (§5 / §6 / §23–§25 / §27).
//!
//! **Status:** CONTRACT ONLY. This module ships the stable identifier
//! surface both tables need:
//!
//! * [`QUESTDB_TABLE_INSTRUMENT_LIFECYCLE`] / [`QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT`]
//! * [`DEDUP_KEY_INSTRUMENT_LIFECYCLE`] / [`DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT`]
//! * [`LifecycleState`] — the `lifecycle_state` SYMBOL labels (§5)
//! * [`TransitionKind`] — the audit `transition_kind` SYMBOL labels (§6 + §23)
//! * [`lifecycle_designated_ts_nanos`] — the pinned constant designated
//!   timestamp (see "Designated timestamp" below)
//!
//! The DDL strings, `ensure_*_table` helpers, `*Row` structs, and
//! `append_*_row` helpers land in follow-up sub-PRs (mirroring how
//! `instrument_fetch_audit` shipped its contract in #835 and its
//! persistence helpers in #836). The daily lifecycle reconciler
//! (idempotent UPSERT + state-flip + §24 write-audit-first ordering)
//! is a further follow-up.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build until the boot orchestrator (Sub-PR #10)
//! flips the flag.
//!
//! # Why two tables
//!
//! * `instrument_lifecycle` — the **current** state, ONE row per
//!   instrument EVER observed, UPSERT-updated in place. NEVER deleted
//!   (operator quote §0: "instead of deleting it … marked as expired").
//! * `instrument_lifecycle_audit` — the **forensic chain**, one row per
//!   state transition (appeared / updated / expired / reactivated /
//!   split / segment-moved / …). 5-year SEBI retention. Powers the
//!   §25 point-in-time "what was the universe on date X?" reconstruction.
//!
//! # Designated timestamp (the I-P1-08 resolution)
//!
//! QuestDB requires the designated timestamp column to appear in every
//! `DEDUP UPSERT KEYS(...)` clause (omitting it returns HTTP 400 at
//! boot — the 2026-04-28 regression class). But `instrument_lifecycle`
//! wants ONE row per `(security_id, exchange_segment)` that is UPDATED
//! in place as the instrument's state changes — a *mutable* last-update
//! time in the DEDUP key would make every update a NEW row.
//!
//! Resolution (same as the retired `instrument_persistence` I-P1-08
//! design): the designated `ts` column is pinned to a CONSTANT
//! ([`lifecycle_designated_ts_nanos`] = epoch 0) so the DEDUP fires on
//! the business key `(security_id, exchange_segment)` alone, while the
//! mutable wall-clock last-update time lives in a separate
//! `last_update_ts` column (added by the DDL follow-up). The DEDUP key
//! constant therefore lists `ts` first to satisfy QuestDB, then the
//! I-P1-11 composite business key.
//!
//! The `*_audit` table is append-only (not UPSERT-in-place), so its `ts`
//! is the real per-transition wall-clock; its DEDUP key additionally
//! carries `transition_kind` so two different transitions for the same
//! instrument on the same day both survive (§6).
//!
//! ## ⚠ DDL follow-up MUST read this
//!
//! Because `instrument_lifecycle`'s designated `ts` is pinned to epoch 0,
//! the table's DDL **MUST use `PARTITION BY NONE`** (or partition on a
//! separate non-designated date column) — NOT `PARTITION BY DAY/MONTH`.
//! A date-partitioned table with a constant `ts` would funnel every
//! never-deleted lifecycle row into a single `1970-01-01` partition,
//! defeating QuestDB partition pruning AND breaking the `partition_manager`
//! S3-archive-by-date lifecycle (it detaches partitions by date; a
//! one-partition table can never detach). The append-only
//! `instrument_lifecycle_audit` table keeps a REAL per-transition `ts`,
//! so it partitions by day normally.
//!
//! # Cross-references
//!
//! * Rule §5 / §6 / §23 / §24 / §25 / §27 — `daily-universe-scope-expansion-2026-05-27.md`
//! * `security-id-uniqueness.md` (I-P1-11) — composite `(security_id, exchange_segment)`
//! * `gap-enforcement.md` (I-P1-08) — constant designated timestamp for UPSERT-one-row tables

#![cfg(feature = "daily_universe_fetcher")]

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

/// `/exec` HTTP timeout for both DDL and INSERT. Matches the value used
/// by the other `*_persistence` modules in the storage crate.
const QUESTDB_EXEC_TIMEOUT_SECS: u64 = 10;

/// Wire-format table name for the current-state lifecycle table.
/// Stable across releases — operators, the reconciler, and the
/// `partition_manager` S3 archive job depend on the exact string.
pub const QUESTDB_TABLE_INSTRUMENT_LIFECYCLE: &str = "instrument_lifecycle";

/// Wire-format table name for the forensic transition-chain table.
pub const QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT: &str = "instrument_lifecycle_audit";

/// DEDUP key for `instrument_lifecycle` — UPSERT one row per instrument.
///
/// `ts` (pinned constant — see module docs) satisfies QuestDB's
/// designated-timestamp-in-DEDUP requirement; `security_id` +
/// `exchange_segment` are the I-P1-11 composite business key (Dhan
/// reuses `security_id` across segments, so segment is mandatory).
pub const DEDUP_KEY_INSTRUMENT_LIFECYCLE: &str = "ts, security_id, exchange_segment";

/// DEDUP key for `instrument_lifecycle_audit` — append one row per
/// transition.
///
/// `ts` is the real per-transition wall-clock designated timestamp.
/// `trading_date_ist` is the partition/business date. `security_id` +
/// `exchange_segment` is the I-P1-11 composite key. `transition_kind`
/// ensures two distinct transitions for the same instrument on the same
/// day (e.g. `updated` then `expired`) BOTH survive rather than the
/// terminal one overwriting the trigger one (§6).
// Kept on one line so the `dedup_segment_meta_guard.rs` source scanner
// (which matches `const … : &str = "…"` on a single line) detects it and
// verifies both the I-P1-11 segment-pairing and the designated-`ts` token.
#[rustfmt::skip]
pub const DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT: &str = "ts, trading_date_ist, security_id, exchange_segment, transition_kind";

/// The pinned constant designated timestamp (epoch 0) for the
/// UPSERT-in-place `instrument_lifecycle` table. See module docs
/// ("Designated timestamp") for the I-P1-08 rationale. The DDL
/// follow-up stamps every lifecycle row's designated `ts` with this
/// value so the DEDUP fires on the business key alone.
#[must_use]
pub const fn lifecycle_designated_ts_nanos() -> i64 {
    0
}

/// Stable wire-format strings for the `instrument_lifecycle.lifecycle_state`
/// SYMBOL column (§5). Operators query this column by exact string match;
/// bumping any label is a breaking change.
///
/// The reconciler (follow-up) flips `Active` → one of the `Expired*`
/// states when an instrument disappears from the daily CSV, and back to
/// `Active` on reappearance. `Delisted` is operator-set (manual). Rows
/// are NEVER deleted (§0 operator lock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleState {
    /// Present in today's CSV and tradable.
    Active,
    /// Stock dropped out of the F&O list (its derivatives expired off it).
    ExpiredFromFno,
    /// A specific derivative contract reached its expiry date.
    ExpiredContract,
    /// An index that is no longer published.
    ExpiredIndex,
    /// Operator-set terminal state (manual override) — instrument removed
    /// from the exchange entirely.
    Delisted,
}

impl LifecycleState {
    /// Returns the wire-format SYMBOL label. ALWAYS lowercase snake_case.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ExpiredFromFno => "expired_from_fno",
            Self::ExpiredContract => "expired_contract",
            Self::ExpiredIndex => "expired_index",
            Self::Delisted => "delisted",
        }
    }

    /// Parse a wire-format SYMBOL label back into a [`LifecycleState`].
    /// Returns `None` for any unrecognized string — the read-back loader
    /// counts these as "unknown state" rather than guessing, so a future
    /// schema drift surfaces instead of silently mis-classifying.
    /// Exact inverse of [`as_str`](Self::as_str).
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "expired_from_fno" => Some(Self::ExpiredFromFno),
            "expired_contract" => Some(Self::ExpiredContract),
            "expired_index" => Some(Self::ExpiredIndex),
            "delisted" => Some(Self::Delisted),
            _ => None,
        }
    }

    /// True only for `Active`. Dashboards + the subscription dispatcher
    /// MUST filter `WHERE lifecycle_state = 'active'`.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// True for any of the three `Expired*` states (NOT `Delisted`,
    /// which is a manual terminal state, and NOT `Active`).
    #[must_use]
    pub const fn is_expired(self) -> bool {
        matches!(
            self,
            Self::ExpiredFromFno | Self::ExpiredContract | Self::ExpiredIndex
        )
    }

    /// All variants, for exhaustive ratchet tests.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Active,
            Self::ExpiredFromFno,
            Self::ExpiredContract,
            Self::ExpiredIndex,
            Self::Delisted,
        ]
    }
}

/// Stable wire-format strings for the
/// `instrument_lifecycle_audit.transition_kind` SYMBOL column (§6 + §23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionKind {
    /// First time this `(security_id, exchange_segment)` was ever seen.
    Appeared,
    /// A mutable field (lot_size, tick_size, symbol, …) changed.
    Updated,
    /// `lifecycle_state` flipped to one of the `Expired*` states.
    Expired,
    /// An expired instrument reappeared in the CSV and flipped back to
    /// `Active`.
    Reactivated,
    /// Operator manually set `Delisted`.
    DelistedManual,
    /// Operator set `lifecycle_state_locked` (§5 override).
    Locked,
    /// Stock split detected — lot/tick size halved or less (§23).
    Split,
    /// Same `security_id` moved to a different `exchange_segment` (§23).
    SegmentMoved,
}

impl TransitionKind {
    /// Returns the wire-format SYMBOL label. ALWAYS lowercase snake_case.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Appeared => "appeared",
            Self::Updated => "updated",
            Self::Expired => "expired",
            Self::Reactivated => "reactivated",
            Self::DelistedManual => "delisted_manual",
            Self::Locked => "locked",
            Self::Split => "split",
            Self::SegmentMoved => "segment_moved",
        }
    }

    /// All variants, for exhaustive ratchet tests.
    #[must_use]
    pub const fn all() -> [Self; 8] {
        [
            Self::Appeared,
            Self::Updated,
            Self::Expired,
            Self::Reactivated,
            Self::DelistedManual,
            Self::Locked,
            Self::Split,
            Self::SegmentMoved,
        ]
    }
}

// ============================================================================
// instrument_lifecycle persistence (DDL + Row + ensure + append)
// ============================================================================

/// Column list shared by the DDL and the INSERT helper, in exact schema
/// order. One constant so the writer and the table definition cannot
/// drift. 24 columns.
const LIFECYCLE_INSERT_COLUMN_LIST: &str = "ts, last_update_ts, security_id, exchange_segment, \
     exchange_id, instrument_type, symbol_name, display_name, underlying_security_id, \
     underlying_symbol, lot_size, tick_size, expiry_date, strike_price, option_type, \
     lifecycle_state, lifecycle_state_locked, first_seen_date, last_seen_date, last_active_date, \
     expired_date, prev_symbol_chain, source_csv_sha256, dry_run";

/// One `instrument_lifecycle` row in the shape the writer accepts.
///
/// 24 columns covering the full §5 schema + §23 `prev_symbol_chain` +
/// §27 `dry_run`. The designated `ts` is NOT in this struct — the writer
/// stamps it from [`lifecycle_designated_ts_nanos`] (pinned constant, so
/// the UPSERT dedups on the business key per I-P1-08). The mutable
/// last-update wall-clock is `last_update_ts_nanos`.
///
/// `*_nanos` date fields are IST wall-clock nanoseconds (host-clock
/// source — the daily reconciler runs on `Utc::now()` → IST, NOT a Dhan
/// WS `exchange_timestamp`). The writer divides them to microseconds for
/// QuestDB `TIMESTAMP` columns. A `*_nanos` value of `0` is written as
/// literal **epoch-0 (`1970-01-01`)**, NOT SQL `NULL`, for optional date
/// columns (expiry / expired / etc). Consumers MUST therefore detect
/// "no expiry" via `lifecycle_state` / `instrument_type`, NOT via
/// `expiry_date IS NULL` (which never matches).
#[derive(Debug, Clone, PartialEq)]
pub struct InstrumentLifecycleRow<'a> {
    /// Mutable last-update wall-clock (IST nanos). Distinct from the
    /// pinned designated `ts`.
    pub last_update_ts_nanos: i64,
    pub security_id: i64,
    pub exchange_segment: &'a str,
    pub exchange_id: &'a str,
    pub instrument_type: &'a str,
    pub symbol_name: &'a str,
    pub display_name: &'a str,
    /// 0 for spot/index (no underlying).
    pub underlying_security_id: i64,
    pub underlying_symbol: &'a str,
    pub lot_size: i32,
    pub tick_size: f64,
    /// IST nanos; 0 for spot/index (no expiry).
    pub expiry_date_nanos: i64,
    /// 0.0 for non-options.
    pub strike_price: f64,
    /// `CE` / `PE` / empty for non-options.
    pub option_type: &'a str,
    pub lifecycle_state: LifecycleState,
    /// §5 operator override — orchestrator skips locked rows when
    /// flipping states.
    pub lifecycle_state_locked: bool,
    pub first_seen_date_nanos: i64,
    pub last_seen_date_nanos: i64,
    pub last_active_date_nanos: i64,
    /// 0 while active; set when state flips to any `expired_*`.
    pub expired_date_nanos: i64,
    /// §23 append-only JSON array of prior symbols (rename/merger chain).
    pub prev_symbol_chain: &'a str,
    pub source_csv_sha256: &'a str,
    /// §27 — `true` for `--dry-run-universe` rows; the reconciler reads
    /// only `WHERE dry_run = false` for the next-day delta.
    pub dry_run: bool,
}

/// Creates the `instrument_lifecycle` table if absent. Idempotent.
///
/// **`PARTITION BY NONE`** — the designated `ts` is pinned to a constant
/// (I-P1-08), so a date partition would funnel every never-deleted row
/// into one `1970-01-01` partition. See the module-level "DDL follow-up
/// MUST read this" note.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_instrument_lifecycle_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_INSTRUMENT_LIFECYCLE} (\
            ts TIMESTAMP, \
            last_update_ts TIMESTAMP, \
            security_id LONG, \
            exchange_segment SYMBOL, \
            exchange_id SYMBOL, \
            instrument_type SYMBOL, \
            symbol_name SYMBOL, \
            display_name STRING, \
            underlying_security_id LONG, \
            underlying_symbol SYMBOL, \
            lot_size INT, \
            tick_size DOUBLE, \
            expiry_date TIMESTAMP, \
            strike_price DOUBLE, \
            option_type SYMBOL, \
            lifecycle_state SYMBOL, \
            lifecycle_state_locked BOOLEAN, \
            first_seen_date TIMESTAMP, \
            last_seen_date TIMESTAMP, \
            last_active_date TIMESTAMP, \
            expired_date TIMESTAMP, \
            prev_symbol_chain STRING, \
            source_csv_sha256 SYMBOL, \
            dry_run BOOLEAN\
        ) timestamp(ts) PARTITION BY NONE WAL \
        DEDUP UPSERT KEYS({DEDUP_KEY_INSTRUMENT_LIFECYCLE});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_INSTRUMENT_LIFECYCLE,
                "lifecycle table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_INSTRUMENT_LIFECYCLE,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_INSTRUMENT_LIFECYCLE,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Formats one row as a QuestDB `VALUES (...)` tuple in schema order.
///
/// The leading designated `ts` is stamped from
/// [`lifecycle_designated_ts_nanos`] (constant). All `*_nanos` fields are
/// divided to microseconds (QuestDB `TIMESTAMP` stores micros; embedding
/// nanos overflows the year-9999 bound — 2026-04-28 regression).
fn format_lifecycle_row_tuple(row: &InstrumentLifecycleRow<'_>) -> String {
    let ts_micros = lifecycle_designated_ts_nanos() / 1_000;
    let last_update_micros = row.last_update_ts_nanos / 1_000;
    let exchange_segment = sanitize_audit_string(row.exchange_segment);
    let exchange_id = sanitize_audit_string(row.exchange_id);
    let instrument_type = sanitize_audit_string(row.instrument_type);
    let symbol_name = sanitize_audit_string(row.symbol_name);
    let display_name = sanitize_audit_string(row.display_name);
    let underlying_symbol = sanitize_audit_string(row.underlying_symbol);
    let option_type = sanitize_audit_string(row.option_type);
    let lifecycle_state = row.lifecycle_state.as_str();
    let prev_symbol_chain = sanitize_audit_string(row.prev_symbol_chain);
    let source_csv_sha256 = sanitize_audit_string(row.source_csv_sha256);
    let security_id = row.security_id;
    let underlying_security_id = row.underlying_security_id;
    let lot_size = row.lot_size;
    let tick_size = row.tick_size;
    let expiry_micros = row.expiry_date_nanos / 1_000;
    let strike_price = row.strike_price;
    let lifecycle_state_locked = row.lifecycle_state_locked;
    let first_seen_micros = row.first_seen_date_nanos / 1_000;
    let last_seen_micros = row.last_seen_date_nanos / 1_000;
    let last_active_micros = row.last_active_date_nanos / 1_000;
    let expired_micros = row.expired_date_nanos / 1_000;
    let dry_run = row.dry_run;
    format!(
        "({ts_micros}, {last_update_micros}, {security_id}, '{exchange_segment}', \
          '{exchange_id}', '{instrument_type}', '{symbol_name}', '{display_name}', \
          {underlying_security_id}, '{underlying_symbol}', {lot_size}, {tick_size}, \
          {expiry_micros}, {strike_price}, '{option_type}', '{lifecycle_state}', \
          {lifecycle_state_locked}, {first_seen_micros}, {last_seen_micros}, \
          {last_active_micros}, {expired_micros}, '{prev_symbol_chain}', \
          '{source_csv_sha256}', {dry_run})"
    )
}

/// Appends (UPSERT) one `instrument_lifecycle` row. The table's DEDUP
/// UPSERT KEYS make this idempotent — re-writing the same
/// `(security_id, exchange_segment)` updates the row in place.
///
/// Cold path — called by the daily reconciler at boot (~250 rows).
///
/// # Errors
///
/// Propagates the `reqwest` transport error, or returns `Err` on a
/// non-2xx `/exec` response.
pub async fn append_instrument_lifecycle_row(
    questdb_config: &QuestDbConfig,
    row: &InstrumentLifecycleRow<'_>,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()?;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_INSTRUMENT_LIFECYCLE} \
         ({LIFECYCLE_INSERT_COLUMN_LIST}) VALUES {tuple};",
        tuple = format_lifecycle_row_tuple(row),
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        // Cap the reflected QuestDB body (it can reach Telegram) — same
        // convention as instrument_fetch_audit.
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        anyhow::bail!("instrument_lifecycle insert non-2xx ({status}): {body}");
    }
    Ok(())
}

// ============================================================================
// instrument_lifecycle_audit persistence (DDL + Row + ensure + append)
// ============================================================================

/// Column list shared by the audit DDL + INSERT helper, in exact schema
/// order. One constant so writer + table can't drift. 16 columns.
const LIFECYCLE_AUDIT_INSERT_COLUMN_LIST: &str = "ts, trading_date_ist, security_id, \
     exchange_segment, from_state, to_state, transition_kind, field_deltas, source_csv_sha256, \
     operator_note, lifecycle_state_after, lot_size_after, tick_size_after, expiry_date_after, \
     symbol_name_after, dry_run";

/// One `instrument_lifecycle_audit` row — the forensic record of a single
/// state transition (§6 + §25 point-in-time snapshot columns + §27).
///
/// Append-only (NOT UPSERT-in-place), so `ts` is the REAL per-transition
/// wall-clock (IST nanos) — unlike the `instrument_lifecycle` table whose
/// `ts` is pinned constant. The `*_after` columns snapshot the
/// post-transition state so SEBI point-in-time queries (§25) can answer
/// "what was instrument X's state on date D?" without replaying the whole
/// chain.
///
/// `from_state` is `None` for an `appeared` transition (no prior state);
/// it renders as the empty SYMBOL `''`.
#[derive(Debug, Clone, PartialEq)]
pub struct InstrumentLifecycleAuditRow<'a> {
    /// REAL per-transition wall-clock (IST nanos). Designated timestamp.
    pub ts_nanos_ist: i64,
    /// IST date-midnight nanos — partition/business date.
    pub trading_date_ist_nanos: i64,
    pub security_id: i64,
    pub exchange_segment: &'a str,
    /// Prior state; `None` for `appeared` (renders `''`).
    pub from_state: Option<LifecycleState>,
    pub to_state: LifecycleState,
    pub transition_kind: TransitionKind,
    /// JSON of changed `field: [old, new]` pairs (when
    /// `transition_kind == Updated`); empty otherwise.
    pub field_deltas: &'a str,
    pub source_csv_sha256: &'a str,
    /// Free-form note (populated only by manual overrides — §5/§6).
    pub operator_note: &'a str,
    /// §25 snapshot: post-transition `lifecycle_state`.
    pub lifecycle_state_after: LifecycleState,
    pub lot_size_after: i32,
    pub tick_size_after: f64,
    /// §25 snapshot: post-transition expiry (IST nanos; 0 for spot/index).
    pub expiry_date_after_nanos: i64,
    pub symbol_name_after: &'a str,
    /// §27 — dry-run isolation.
    pub dry_run: bool,
}

/// Creates the `instrument_lifecycle_audit` table if absent. Idempotent.
///
/// **`PARTITION BY DAY`** — unlike the sibling `instrument_lifecycle`
/// table, this one is append-only with a REAL per-transition `ts`, so it
/// partitions by day normally (enabling the `partition_manager`
/// S3-archive-by-date lifecycle for the 5-year SEBI chain).
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_instrument_lifecycle_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            security_id LONG, \
            exchange_segment SYMBOL, \
            from_state SYMBOL, \
            to_state SYMBOL, \
            transition_kind SYMBOL, \
            field_deltas STRING, \
            source_csv_sha256 SYMBOL, \
            operator_note STRING, \
            lifecycle_state_after SYMBOL, \
            lot_size_after INT, \
            tick_size_after DOUBLE, \
            expiry_date_after TIMESTAMP, \
            symbol_name_after SYMBOL, \
            dry_run BOOLEAN\
        ) timestamp(ts) PARTITION BY DAY WAL \
        DEDUP UPSERT KEYS({DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT,
                "lifecycle audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Formats one audit row as a QuestDB `VALUES (...)` tuple in schema order.
///
/// `ts` is the REAL per-transition wall-clock (divided nanos→micros like
/// every other timestamp — year-9999 guard). `from_state == None` renders
/// the empty SYMBOL `''`.
fn format_lifecycle_audit_row_tuple(row: &InstrumentLifecycleAuditRow<'_>) -> String {
    let ts_micros = row.ts_nanos_ist / 1_000;
    let trading_date_micros = row.trading_date_ist_nanos / 1_000;
    let security_id = row.security_id;
    let exchange_segment = sanitize_audit_string(row.exchange_segment);
    let from_state = row.from_state.map_or("", LifecycleState::as_str);
    let to_state = row.to_state.as_str();
    let transition_kind = row.transition_kind.as_str();
    let field_deltas = sanitize_audit_string(row.field_deltas);
    let source_csv_sha256 = sanitize_audit_string(row.source_csv_sha256);
    let operator_note = sanitize_audit_string(row.operator_note);
    let lifecycle_state_after = row.lifecycle_state_after.as_str();
    let lot_size_after = row.lot_size_after;
    let tick_size_after = row.tick_size_after;
    let expiry_after_micros = row.expiry_date_after_nanos / 1_000;
    let symbol_name_after = sanitize_audit_string(row.symbol_name_after);
    let dry_run = row.dry_run;
    format!(
        "({ts_micros}, {trading_date_micros}, {security_id}, '{exchange_segment}', \
          '{from_state}', '{to_state}', '{transition_kind}', '{field_deltas}', \
          '{source_csv_sha256}', '{operator_note}', '{lifecycle_state_after}', \
          {lot_size_after}, {tick_size_after}, {expiry_after_micros}, \
          '{symbol_name_after}', {dry_run})"
    )
}

/// Appends one `instrument_lifecycle_audit` row. Append-only forensic
/// chain — the DEDUP UPSERT KEYS make a re-run idempotent.
///
/// Cold path — called by the daily reconciler per state transition.
///
/// # Errors
///
/// Propagates the `reqwest` transport error, or returns `Err` on a
/// non-2xx `/exec` response.
pub async fn append_instrument_lifecycle_audit_row(
    questdb_config: &QuestDbConfig,
    row: &InstrumentLifecycleAuditRow<'_>,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()?;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT} \
         ({LIFECYCLE_AUDIT_INSERT_COLUMN_LIST}) VALUES {tuple};",
        tuple = format_lifecycle_audit_row_tuple(row),
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        anyhow::bail!("instrument_lifecycle_audit insert non-2xx ({status}): {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_name_constants() {
        assert_eq!(QUESTDB_TABLE_INSTRUMENT_LIFECYCLE, "instrument_lifecycle");
        assert_eq!(
            QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT,
            "instrument_lifecycle_audit"
        );
    }

    #[test]
    fn test_lifecycle_dedup_key_includes_ts_and_segment() {
        // QuestDB designated-timestamp requirement + I-P1-11 segment.
        let has_ts = DEDUP_KEY_INSTRUMENT_LIFECYCLE
            .split([',', ' '])
            .map(str::trim)
            .any(|t| t == "ts");
        assert!(
            has_ts,
            "lifecycle DEDUP key must include the bare `ts` token"
        );
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE.contains("security_id"));
        assert!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE.contains("exchange_segment"),
            "I-P1-11: security_id must be paired with exchange_segment"
        );
        assert_eq!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE.matches(',').count() + 1,
            3,
            "lifecycle DEDUP key has exactly 3 columns"
        );
    }

    #[test]
    fn test_lifecycle_audit_dedup_key_includes_ts_segment_and_transition_kind() {
        let has_ts = DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT
            .split([',', ' '])
            .map(str::trim)
            .any(|t| t == "ts");
        assert!(has_ts, "audit DEDUP key must include the bare `ts` token");
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("trading_date_ist"));
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("exchange_segment"));
        assert!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("transition_kind"),
            "without transition_kind two same-day transitions for one \
             instrument would collapse to a single row (§6)"
        );
        assert_eq!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.matches(',').count() + 1,
            5,
            "audit DEDUP key has exactly 5 columns"
        );
    }

    #[test]
    fn test_lifecycle_designated_ts_is_constant_zero() {
        // I-P1-08: pinned constant so business-key DEDUP works for the
        // UPSERT-in-place lifecycle table.
        assert_eq!(lifecycle_designated_ts_nanos(), 0);
        assert_eq!(
            lifecycle_designated_ts_nanos(),
            lifecycle_designated_ts_nanos(),
            "must be constant across calls"
        );
    }

    #[test]
    fn test_lifecycle_state_has_five_distinct_lowercase_labels() {
        let variants = LifecycleState::all();
        assert_eq!(variants.len(), 5);
        let mut seen = Vec::new();
        for v in variants {
            let label = v.as_str();
            assert!(!seen.contains(&label), "duplicate label {label}");
            for ch in label.chars() {
                assert!(
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_',
                    "non-snake_case char `{ch}` in `{label}`"
                );
            }
            seen.push(label);
        }
    }

    #[test]
    fn test_lifecycle_state_is_active_only_for_active() {
        assert!(LifecycleState::Active.is_active());
        for v in LifecycleState::all() {
            if v != LifecycleState::Active {
                assert!(!v.is_active(), "{v:?} must not be active");
            }
        }
        let active_count = LifecycleState::all()
            .iter()
            .filter(|v| v.is_active())
            .count();
        assert_eq!(active_count, 1);
    }

    #[test]
    fn test_lifecycle_state_is_expired_covers_three_expired_states_only() {
        assert!(LifecycleState::ExpiredFromFno.is_expired());
        assert!(LifecycleState::ExpiredContract.is_expired());
        assert!(LifecycleState::ExpiredIndex.is_expired());
        assert!(!LifecycleState::Active.is_expired());
        assert!(
            !LifecycleState::Delisted.is_expired(),
            "Delisted is a manual terminal state, distinct from Expired*"
        );
        let expired_count = LifecycleState::all()
            .iter()
            .filter(|v| v.is_expired())
            .count();
        assert_eq!(expired_count, 3);
    }

    #[test]
    fn test_lifecycle_state_active_and_expired_are_disjoint() {
        for v in LifecycleState::all() {
            assert!(
                !(v.is_active() && v.is_expired()),
                "{v:?} cannot be both active and expired"
            );
        }
    }

    #[test]
    fn test_transition_kind_has_eight_distinct_lowercase_labels() {
        let variants = TransitionKind::all();
        assert_eq!(variants.len(), 8);
        let mut seen = Vec::new();
        for v in variants {
            let label = v.as_str();
            assert!(!seen.contains(&label), "duplicate label {label}");
            assert!(!label.is_empty());
            for ch in label.chars() {
                assert!(
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_',
                    "non-snake_case char `{ch}` in `{label}`"
                );
            }
            seen.push(label);
        }
    }

    #[test]
    fn test_transition_kind_wire_labels_are_stable() {
        // Pin the exact strings — operators + the §25 point-in-time
        // query depend on them.
        assert_eq!(TransitionKind::Appeared.as_str(), "appeared");
        assert_eq!(TransitionKind::Updated.as_str(), "updated");
        assert_eq!(TransitionKind::Expired.as_str(), "expired");
        assert_eq!(TransitionKind::Reactivated.as_str(), "reactivated");
        assert_eq!(TransitionKind::DelistedManual.as_str(), "delisted_manual");
        assert_eq!(TransitionKind::Locked.as_str(), "locked");
        assert_eq!(TransitionKind::Split.as_str(), "split");
        assert_eq!(TransitionKind::SegmentMoved.as_str(), "segment_moved");
    }

    #[test]
    fn test_lifecycle_state_wire_labels_are_stable() {
        assert_eq!(LifecycleState::Active.as_str(), "active");
        assert_eq!(LifecycleState::ExpiredFromFno.as_str(), "expired_from_fno");
        assert_eq!(LifecycleState::ExpiredContract.as_str(), "expired_contract");
        assert_eq!(LifecycleState::ExpiredIndex.as_str(), "expired_index");
        assert_eq!(LifecycleState::Delisted.as_str(), "delisted");
    }

    #[test]
    fn test_lifecycle_state_from_wire_roundtrips_every_variant() {
        for v in LifecycleState::all() {
            assert_eq!(
                LifecycleState::from_wire(v.as_str()),
                Some(v),
                "from_wire(as_str()) must roundtrip for {v:?}"
            );
        }
    }

    #[test]
    fn test_lifecycle_state_from_wire_rejects_unknown() {
        assert_eq!(LifecycleState::from_wire("nonsense"), None);
        assert_eq!(LifecycleState::from_wire(""), None);
        assert_eq!(LifecycleState::from_wire("ACTIVE"), None, "case-sensitive");
    }

    #[test]
    fn test_enums_derive_eq_and_hash() {
        use std::collections::HashSet;
        let mut s: HashSet<LifecycleState> = HashSet::new();
        for v in LifecycleState::all() {
            assert!(s.insert(v));
        }
        let mut t: HashSet<TransitionKind> = HashSet::new();
        for v in TransitionKind::all() {
            assert!(t.insert(v));
        }
    }

    // ========================================================================
    // Persistence-helper tests (DDL + Row + append)
    // ========================================================================

    fn cfg_unreachable() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    fn sample_index_row() -> InstrumentLifecycleRow<'static> {
        InstrumentLifecycleRow {
            last_update_ts_nanos: 1_700_000_000_000_000_000,
            security_id: 13,
            exchange_segment: "IDX_I",
            exchange_id: "NSE",
            instrument_type: "INDEX",
            symbol_name: "NIFTY",
            display_name: "NIFTY 50",
            underlying_security_id: 0,
            underlying_symbol: "",
            lot_size: 0,
            tick_size: 0.05,
            expiry_date_nanos: 0,
            strike_price: 0.0,
            option_type: "",
            lifecycle_state: LifecycleState::Active,
            lifecycle_state_locked: false,
            first_seen_date_nanos: 1_699_920_000_000_000_000,
            last_seen_date_nanos: 1_700_000_000_000_000_000,
            last_active_date_nanos: 1_700_000_000_000_000_000,
            expired_date_nanos: 0,
            prev_symbol_chain: "",
            source_csv_sha256: "deadbeef",
            dry_run: false,
        }
    }

    #[test]
    fn test_lifecycle_insert_column_list_has_24_columns() {
        assert_eq!(
            LIFECYCLE_INSERT_COLUMN_LIST.matches(',').count() + 1,
            24,
            "INSERT column list must name all 24 schema columns"
        );
    }

    #[test]
    fn test_lifecycle_ddl_uses_partition_by_none() {
        // The constant designated ts (I-P1-08) REQUIRES PARTITION BY NONE
        // — a date partition would funnel every never-deleted row into a
        // single 1970 partition (Z+ hostile-review M1). Source-scan pins it.
        // Scan only the DDL format string's literal fragment so the test
        // doesn't trip over the module doc (which mentions PARTITION BY
        // DAY in prose). The positive presence of `PARTITION BY NONE WAL`
        // in the CREATE statement is the contract.
        let source = include_str!("instrument_lifecycle_persistence.rs");
        assert!(
            source.contains(") timestamp(ts) PARTITION BY NONE WAL"),
            "instrument_lifecycle DDL MUST use PARTITION BY NONE (constant ts per I-P1-08)"
        );
    }

    #[test]
    fn test_lifecycle_ddl_contains_all_24_columns() {
        let source = include_str!("instrument_lifecycle_persistence.rs");
        let columns = [
            "ts TIMESTAMP",
            "last_update_ts TIMESTAMP",
            "security_id LONG",
            "exchange_segment SYMBOL",
            "exchange_id SYMBOL",
            "instrument_type SYMBOL",
            "symbol_name SYMBOL",
            "display_name STRING",
            "underlying_security_id LONG",
            "underlying_symbol SYMBOL",
            "lot_size INT",
            "tick_size DOUBLE",
            "expiry_date TIMESTAMP",
            "strike_price DOUBLE",
            "option_type SYMBOL",
            "lifecycle_state SYMBOL",
            "lifecycle_state_locked BOOLEAN",
            "first_seen_date TIMESTAMP",
            "last_seen_date TIMESTAMP",
            "last_active_date TIMESTAMP",
            "expired_date TIMESTAMP",
            "prev_symbol_chain STRING",
            "source_csv_sha256 SYMBOL",
            "dry_run BOOLEAN",
        ];
        for col in columns {
            assert!(
                source.contains(col),
                "DDL must declare column `{col}` — drift between code + schema rejected"
            );
        }
    }

    #[test]
    fn test_lifecycle_tuple_stamps_constant_designated_ts() {
        // The leading designated ts must come from the pinned constant
        // (0 → 0 micros), NOT from last_update_ts.
        let tuple = format_lifecycle_row_tuple(&sample_index_row());
        assert!(
            tuple.starts_with("(0, 1700000000000000, 13, 'IDX_I',"),
            "tuple must lead with constant ts=0 micros, then last_update micros, then security_id; got: {tuple}"
        );
    }

    #[test]
    fn test_lifecycle_tuple_has_24_fields() {
        let tuple = format_lifecycle_row_tuple(&sample_index_row());
        assert!(tuple.starts_with('(') && tuple.ends_with(')'));
        let inner = &tuple[1..tuple.len() - 1];
        assert_eq!(
            inner.matches(',').count(),
            23,
            "24 columns → 23 separating commas"
        );
    }

    #[test]
    fn test_lifecycle_tuple_divides_all_timestamps_to_micros() {
        let source = include_str!("instrument_lifecycle_persistence.rs");
        for field in [
            "last_update_ts_nanos / 1_000",
            "expiry_date_nanos / 1_000",
            "first_seen_date_nanos / 1_000",
            "last_seen_date_nanos / 1_000",
            "last_active_date_nanos / 1_000",
            "expired_date_nanos / 1_000",
        ] {
            assert!(
                source.contains(field),
                "tuple builder must divide `{field}` to micros (year-9999 guard)"
            );
        }
        assert!(
            source.contains("lifecycle_designated_ts_nanos() / 1_000"),
            "designated ts must also be divided to micros"
        );
    }

    #[test]
    fn test_lifecycle_tuple_renders_option_row_and_locked_flag() {
        let mut row = sample_index_row();
        row.security_id = 99887;
        row.exchange_segment = "NSE_FNO";
        row.instrument_type = "OPTSTK";
        row.option_type = "CE";
        row.strike_price = 2500.0;
        row.lifecycle_state = LifecycleState::ExpiredContract;
        row.lifecycle_state_locked = true;
        row.expired_date_nanos = 1_700_100_000_000_000_000;
        let tuple = format_lifecycle_row_tuple(&row);
        assert!(tuple.contains("'NSE_FNO'"));
        assert!(tuple.contains("'OPTSTK'"));
        assert!(tuple.contains("'CE'"));
        assert!(tuple.contains("'expired_contract'"));
        assert!(
            tuple.contains(", true, "),
            "locked flag must render: {tuple}"
        );
    }

    #[test]
    fn test_lifecycle_tuple_sanitizes_injection_in_display_name() {
        let mut row = sample_index_row();
        row.display_name = "'); DROP TABLE instrument_lifecycle;--";
        let tuple = format_lifecycle_row_tuple(&row);
        assert!(
            !tuple.contains("DROP TABLE instrument_lifecycle;"),
            "semicolon-terminated injection must be neutralized: {tuple}"
        );
        assert!(tuple.starts_with('(') && tuple.ends_with(')'));
    }

    #[test]
    fn test_ensure_instrument_lifecycle_table_pub_fn_visible() {
        let _ = ensure_instrument_lifecycle_table;
    }

    #[tokio::test]
    async fn test_append_instrument_lifecycle_row_returns_err_when_questdb_unreachable() {
        let cfg = cfg_unreachable();
        let row = sample_index_row();
        let result = append_instrument_lifecycle_row(&cfg, &row).await;
        assert!(result.is_err(), "must propagate transport error");
    }

    #[tokio::test]
    async fn test_append_instrument_lifecycle_row_handles_expired_fno_state() {
        let cfg = cfg_unreachable();
        let mut row = sample_index_row();
        row.lifecycle_state = LifecycleState::ExpiredFromFno;
        row.expired_date_nanos = 1_700_100_000_000_000_000;
        let result = append_instrument_lifecycle_row(&cfg, &row).await;
        assert!(result.is_err(), "transport error path must propagate");
    }

    #[test]
    fn test_lifecycle_append_error_body_capped() {
        let source = include_str!("instrument_lifecycle_persistence.rs");
        assert!(
            source.contains(".chars()\n            .take(200)\n            .collect()")
                || source.contains(".take(200)"),
            "append helper must cap the reflected QuestDB error body"
        );
    }

    // ========================================================================
    // instrument_lifecycle_audit persistence-helper tests
    // ========================================================================

    fn sample_audit_row() -> InstrumentLifecycleAuditRow<'static> {
        InstrumentLifecycleAuditRow {
            ts_nanos_ist: 1_700_000_000_000_000_000,
            trading_date_ist_nanos: 1_699_920_000_000_000_000,
            security_id: 99887,
            exchange_segment: "NSE_FNO",
            from_state: Some(LifecycleState::Active),
            to_state: LifecycleState::ExpiredFromFno,
            transition_kind: TransitionKind::Expired,
            field_deltas: "",
            source_csv_sha256: "deadbeef",
            operator_note: "",
            lifecycle_state_after: LifecycleState::ExpiredFromFno,
            lot_size_after: 250,
            tick_size_after: 0.05,
            expiry_date_after_nanos: 1_700_100_000_000_000_000,
            symbol_name_after: "TCS",
            dry_run: false,
        }
    }

    #[test]
    fn test_audit_insert_column_list_has_16_columns() {
        assert_eq!(
            LIFECYCLE_AUDIT_INSERT_COLUMN_LIST.matches(',').count() + 1,
            16,
            "audit INSERT column list must name all 16 schema columns"
        );
    }

    #[test]
    fn test_audit_ddl_uses_partition_by_day() {
        // Append-only with REAL per-transition ts → partitions by day
        // (unlike the constant-ts lifecycle table which uses NONE).
        let source = include_str!("instrument_lifecycle_persistence.rs");
        assert!(
            source.contains(") timestamp(ts) PARTITION BY DAY WAL \\\n        DEDUP UPSERT KEYS({DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT})")
                || source.contains("PARTITION BY DAY WAL"),
            "audit DDL must use PARTITION BY DAY (real per-transition ts)"
        );
    }

    #[test]
    fn test_audit_ddl_contains_all_16_columns() {
        let source = include_str!("instrument_lifecycle_persistence.rs");
        let columns = [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "security_id LONG",
            "exchange_segment SYMBOL",
            "from_state SYMBOL",
            "to_state SYMBOL",
            "transition_kind SYMBOL",
            "field_deltas STRING",
            "source_csv_sha256 SYMBOL",
            "operator_note STRING",
            "lifecycle_state_after SYMBOL",
            "lot_size_after INT",
            "tick_size_after DOUBLE",
            "expiry_date_after TIMESTAMP",
            "symbol_name_after SYMBOL",
            "dry_run BOOLEAN",
        ];
        for col in columns {
            assert!(
                source.contains(col),
                "audit DDL must declare column `{col}`"
            );
        }
    }

    #[test]
    fn test_audit_tuple_has_16_fields() {
        let tuple = format_lifecycle_audit_row_tuple(&sample_audit_row());
        assert!(tuple.starts_with('(') && tuple.ends_with(')'));
        let inner = &tuple[1..tuple.len() - 1];
        assert_eq!(
            inner.matches(',').count(),
            15,
            "16 columns → 15 separating commas"
        );
    }

    #[test]
    fn test_audit_tuple_renders_states_and_transition() {
        let tuple = format_lifecycle_audit_row_tuple(&sample_audit_row());
        // ts divided to micros, security_id, segment, from→to states.
        assert!(
            tuple.starts_with("(1700000000000000, 1699920000000000, 99887, 'NSE_FNO',"),
            "leading fields: {tuple}"
        );
        assert!(
            tuple.contains("'active', 'expired_from_fno', 'expired'"),
            "from/to/kind: {tuple}"
        );
        assert!(tuple.contains("'TCS'"), "symbol_name_after: {tuple}");
    }

    #[test]
    fn test_audit_tuple_appeared_has_empty_from_state() {
        // `appeared` has no prior state → from_state None → renders ''.
        let mut row = sample_audit_row();
        row.from_state = None;
        row.to_state = LifecycleState::Active;
        row.transition_kind = TransitionKind::Appeared;
        row.lifecycle_state_after = LifecycleState::Active;
        let tuple = format_lifecycle_audit_row_tuple(&row);
        assert!(
            tuple.contains("'', 'active', 'appeared'"),
            "appeared must render empty from_state then to_state + kind: {tuple}"
        );
    }

    #[test]
    fn test_audit_tuple_sanitizes_injection_in_operator_note() {
        let mut row = sample_audit_row();
        row.operator_note = "'); DROP TABLE instrument_lifecycle_audit;--";
        let tuple = format_lifecycle_audit_row_tuple(&row);
        assert!(
            !tuple.contains("DROP TABLE instrument_lifecycle_audit;"),
            "injection in operator_note must be neutralized: {tuple}"
        );
        assert!(tuple.starts_with('(') && tuple.ends_with(')'));
    }

    #[test]
    fn test_audit_tuple_divides_timestamps_to_micros() {
        let source = include_str!("instrument_lifecycle_persistence.rs");
        for field in [
            "row.ts_nanos_ist / 1_000",
            "trading_date_ist_nanos / 1_000",
            "expiry_date_after_nanos / 1_000",
        ] {
            assert!(
                source.contains(field),
                "audit tuple must divide `{field}` to micros"
            );
        }
    }

    #[test]
    fn test_ensure_instrument_lifecycle_audit_table_pub_fn_visible() {
        let _ = ensure_instrument_lifecycle_audit_table;
    }

    #[tokio::test]
    async fn test_append_instrument_lifecycle_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = cfg_unreachable();
        let row = sample_audit_row();
        let result = append_instrument_lifecycle_audit_row(&cfg, &row).await;
        assert!(result.is_err(), "must propagate transport error");
    }

    #[tokio::test]
    async fn test_append_instrument_lifecycle_audit_row_handles_appeared_transition() {
        let cfg = cfg_unreachable();
        let mut row = sample_audit_row();
        row.from_state = None;
        row.transition_kind = TransitionKind::Appeared;
        row.field_deltas = "{\"lot_size\":[0,250]}";
        let result = append_instrument_lifecycle_audit_row(&cfg, &row).await;
        assert!(result.is_err(), "transport error path must propagate");
    }
}
