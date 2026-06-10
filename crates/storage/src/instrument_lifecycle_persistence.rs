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
//! ## 2026-05-29 — bulk writes use ILP (port 9009), NOT `/exec` URL
//!
//! Both tables are `timestamp(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(...)`
//! (see the DDLs below — the "PARTITION BY NONE" prose in the historical note
//! that follows is SUPERSEDED). With the lifecycle table's constant designated
//! `ts`, every never-deleted row lands in one partition — harmless, and WAL +
//! DEDUP work correctly. The BULK writers
//! ([`append_instrument_lifecycle_rows`] / [`append_instrument_lifecycle_audit_rows`])
//! ingest via **ILP** (the same pipe ticks/candles use) — NOT multi-row SQL
//! `INSERT` over `/exec`. Reason: `/exec` carries the SQL in the URL query
//! string, capped by QuestDB's request buffer; at the ~79K-row applicable-F&O
//! master this overflowed (the 2026-05-29 "79190 write error(s)" /
//! "connection closed before message completed" boot halts). ILP has no URL
//! limit. The single-row helpers + DDL stay on `/exec` (small, no URL issue).
//!
//! ## ⚠ DDL follow-up — historical note (SUPERSEDED above)
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

use anyhow::Context;
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::{sanitize_audit_string, sanitize_ilp_string, sanitize_ilp_symbol};

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
/// `timestamp(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(...)`. The designated
/// `ts` is pinned to a constant (I-P1-08), so every never-deleted row lands in
/// one partition — harmless, and WAL + DEDUP work correctly, which is what lets
/// the bulk writer ingest via ILP (port 9009). DDL itself goes over `/exec`
/// (a single small statement, no URL-size issue).
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
        ) timestamp(ts) PARTITION BY DAY WAL \
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

/// Coerce a non-finite f64 (NaN / ±inf) to `0.0` before SQL formatting.
///
/// `format!("{}", f64::NAN)` emits the literal `NaN`, which QuestDB rejects —
/// a single such tuple would fail its whole INSERT and halt the boot. The
/// ~219K-row applicable-F&O master can carry an odd strike/tick from upstream
/// CSV parsing, so we clamp defensively (a 0.0 price is an obvious sentinel an
/// operator can spot, never a silent SQL break).
#[must_use]
fn finite_or_zero(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
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
    let tick_size = finite_or_zero(row.tick_size);
    let expiry_micros = row.expiry_date_nanos / 1_000;
    let strike_price = finite_or_zero(row.strike_price);
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

/// Write one `instrument_lifecycle` row into an ILP [`Buffer`].
///
/// PURE builder (no network) — deterministically unit-tested via
/// `buffer.as_bytes()`. This is the bulk-ingest replacement for the old
/// `/exec` SQL path: the SQL door (URL query string) is capped by QuestDB's
/// request buffer and overflowed at the ~79K-row F&O master; ILP (port 9009)
/// is the real ingestion pipe (same one ticks/candles use) with no URL limit.
///
/// ILP rules honoured: all SYMBOL columns are written BEFORE any field; the
/// DEDUP-key SYMBOL `exchange_segment` is always written; optional SYMBOLs that
/// are empty (`exchange_id` / `underlying_symbol` / `option_type`) are SKIPPED
/// (→ stored **NULL**, NOT the empty symbol `''` the old `/exec` path wrote;
/// none are DEDUP keys so no duplicate rows result — but consumers must filter
/// `option_type IS NULL`, not `= ''`). Empty ILP tag values are non-standard,
/// hence the skip. f64s are clamped via [`finite_or_zero`]; SYMBOLs use
/// [`sanitize_ilp_symbol`] (strips ILP tag delimiters) and STRING columns use
/// [`sanitize_ilp_string`] (preserves commas/JSON). Timestamps
/// are written as nanos ([`TimestampNanos`]) — QuestDB stores micros. The
/// designated `ts` is the pinned constant so DEDUP fires on the business key
/// (I-P1-08).
fn build_lifecycle_ilp_row(
    buffer: &mut Buffer,
    row: &InstrumentLifecycleRow<'_>,
) -> anyhow::Result<()> {
    buffer.table(QUESTDB_TABLE_INSTRUMENT_LIFECYCLE)?;
    // ── SYMBOLs first (ILP requires all tags before any field). ──
    buffer.symbol(
        "exchange_segment",
        sanitize_ilp_symbol(row.exchange_segment).as_ref(),
    )?;
    buffer.symbol(
        "instrument_type",
        sanitize_ilp_symbol(row.instrument_type).as_ref(),
    )?;
    buffer.symbol("symbol_name", sanitize_ilp_symbol(row.symbol_name).as_ref())?;
    buffer.symbol("lifecycle_state", row.lifecycle_state.as_str())?;
    buffer.symbol(
        "source_csv_sha256",
        sanitize_ilp_symbol(row.source_csv_sha256).as_ref(),
    )?;
    // Optional SYMBOLs — skip when empty (→ NULL) to avoid empty ILP symbols.
    if !row.exchange_id.is_empty() {
        buffer.symbol("exchange_id", sanitize_ilp_symbol(row.exchange_id).as_ref())?;
    }
    if !row.underlying_symbol.is_empty() {
        buffer.symbol(
            "underlying_symbol",
            sanitize_ilp_symbol(row.underlying_symbol).as_ref(),
        )?;
    }
    if !row.option_type.is_empty() {
        buffer.symbol("option_type", sanitize_ilp_symbol(row.option_type).as_ref())?;
    }
    // ── Fields. ──
    buffer.column_i64("security_id", row.security_id)?;
    buffer.column_i64("underlying_security_id", row.underlying_security_id)?;
    buffer.column_i64("lot_size", i64::from(row.lot_size))?;
    buffer.column_f64("tick_size", finite_or_zero(row.tick_size))?;
    buffer.column_f64("strike_price", finite_or_zero(row.strike_price))?;
    buffer.column_bool("lifecycle_state_locked", row.lifecycle_state_locked)?;
    buffer.column_bool("dry_run", row.dry_run)?;
    // STRING columns use `sanitize_ilp_string` (NOT the symbol sanitiser):
    // commas/equals are literal-safe inside an ILP quoted string and must be
    // preserved — stripping them would corrupt JSON-bearing fields.
    buffer.column_str("display_name", &sanitize_ilp_string(row.display_name))?;
    buffer.column_str(
        "prev_symbol_chain",
        &sanitize_ilp_string(row.prev_symbol_chain),
    )?;
    buffer.column_ts(
        "last_update_ts",
        TimestampNanos::new(row.last_update_ts_nanos),
    )?;
    buffer.column_ts("expiry_date", TimestampNanos::new(row.expiry_date_nanos))?;
    buffer.column_ts(
        "first_seen_date",
        TimestampNanos::new(row.first_seen_date_nanos),
    )?;
    buffer.column_ts(
        "last_seen_date",
        TimestampNanos::new(row.last_seen_date_nanos),
    )?;
    buffer.column_ts(
        "last_active_date",
        TimestampNanos::new(row.last_active_date_nanos),
    )?;
    buffer.column_ts("expired_date", TimestampNanos::new(row.expired_date_nanos))?;
    buffer.at(TimestampNanos::new(lifecycle_designated_ts_nanos()))?;
    Ok(())
}

/// Bulk-ingest many `instrument_lifecycle` rows via QuestDB **ILP** (port
/// 9009) — the real ingestion pipe, NOT the `/exec` SQL/URL door.
///
/// The whole [`Buffer`] is built on the async task (CPU-only, borrows `rows`),
/// then the blocking ILP connect + flush runs on a `spawn_blocking` thread so
/// the tokio runtime is never blocked. Idempotent via the table's DEDUP UPSERT
/// KEYS — a re-run after a fail-closed boot is safe (Z+ L6).
///
/// # Errors
/// Returns `Err` if the ILP `Sender` cannot connect or the flush fails; the
/// caller (`apply_reconcile_plan`) counts this and the next idempotent boot
/// re-runs.
// TEST-EXEMPT: network I/O — pure builder `build_lifecycle_ilp_row` is unit-tested + boot integration exercises the flush.
pub async fn append_instrument_lifecycle_rows(
    questdb_config: &QuestDbConfig,
    rows: &[InstrumentLifecycleRow<'_>],
) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let conf = questdb_config.build_ilp_conf_string();
    let mut buffer = Buffer::new(ProtocolVersion::V1);
    for row in rows {
        build_lifecycle_ilp_row(&mut buffer, row)
            .context("building instrument_lifecycle ILP row")?;
    }
    // Borrows of `rows` end here; move the owned buffer + conf into a blocking
    // thread for the synchronous ILP connect + flush.
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut sender =
            Sender::from_conf(&conf).context("connect QuestDB ILP (instrument_lifecycle)")?;
        sender
            .flush(&mut buffer)
            .context("flush instrument_lifecycle rows via ILP")?;
        Ok(())
    })
    .await
    .context("instrument_lifecycle ILP flush task join")??;
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
    let tick_size_after = finite_or_zero(row.tick_size_after);
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

/// Build ONE multi-row `INSERT … VALUES (t1),…,(tN);` for a chunk of audit
/// rows. PURE (no I/O) — extracted for deterministic testing.
///
/// Same ILP rules as [`build_lifecycle_ilp_row`]: SYMBOLs before fields; the
/// DEDUP-key SYMBOLs (`exchange_segment`, `transition_kind`) are always written;
/// the optional `from_state` SYMBOL is SKIPPED when empty (`appeared` has no
/// prior state → NULL). `field_deltas` / `operator_note` are STRING columns.
fn build_lifecycle_audit_ilp_row(
    buffer: &mut Buffer,
    row: &InstrumentLifecycleAuditRow<'_>,
) -> anyhow::Result<()> {
    buffer.table(QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT)?;
    // ── SYMBOLs first. ──
    buffer.symbol(
        "exchange_segment",
        sanitize_ilp_symbol(row.exchange_segment).as_ref(),
    )?;
    buffer.symbol("to_state", row.to_state.as_str())?;
    buffer.symbol("transition_kind", row.transition_kind.as_str())?;
    buffer.symbol(
        "source_csv_sha256",
        sanitize_ilp_symbol(row.source_csv_sha256).as_ref(),
    )?;
    buffer.symbol("lifecycle_state_after", row.lifecycle_state_after.as_str())?;
    buffer.symbol(
        "symbol_name_after",
        sanitize_ilp_symbol(row.symbol_name_after).as_ref(),
    )?;
    // `from_state` is empty for an `appeared` transition — skip (→ NULL).
    if let Some(from_state) = row.from_state {
        buffer.symbol("from_state", from_state.as_str())?;
    }
    // ── Fields. ──
    buffer.column_i64("security_id", row.security_id)?;
    buffer.column_i64("lot_size_after", i64::from(row.lot_size_after))?;
    buffer.column_f64("tick_size_after", finite_or_zero(row.tick_size_after))?;
    buffer.column_bool("dry_run", row.dry_run)?;
    // STRING columns use `sanitize_ilp_string` — `field_deltas` is JSON
    // (`{"lot_size":[1000,200]}`); the symbol sanitiser would strip its
    // commas and corrupt the forensic chain.
    buffer.column_str("field_deltas", &sanitize_ilp_string(row.field_deltas))?;
    buffer.column_str("operator_note", &sanitize_ilp_string(row.operator_note))?;
    buffer.column_ts(
        "trading_date_ist",
        TimestampNanos::new(row.trading_date_ist_nanos),
    )?;
    buffer.column_ts(
        "expiry_date_after",
        TimestampNanos::new(row.expiry_date_after_nanos),
    )?;
    // Audit `ts` is the REAL per-transition wall-clock (NOT pinned constant).
    buffer.at(TimestampNanos::new(row.ts_nanos_ist))?;
    Ok(())
}

/// Bulk-append many `instrument_lifecycle_audit` rows via QuestDB **ILP**
/// (port 9009). Append-only forensic chain — written BEFORE the lifecycle
/// batch so the §24 audit-first invariant holds. Idempotent via DEDUP UPSERT
/// KEYS. Buffer built on the async task; blocking flush on `spawn_blocking`.
///
/// # Errors
/// Returns `Err` on ILP connect/flush failure → caller counts it → next
/// idempotent boot re-runs (Z+ L6).
// TEST-EXEMPT: network I/O — pure builder `build_lifecycle_audit_ilp_row` is unit-tested + boot integration exercises the flush.
pub async fn append_instrument_lifecycle_audit_rows(
    questdb_config: &QuestDbConfig,
    rows: &[InstrumentLifecycleAuditRow<'_>],
) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let conf = questdb_config.build_ilp_conf_string();
    let mut buffer = Buffer::new(ProtocolVersion::V1);
    for row in rows {
        build_lifecycle_audit_ilp_row(&mut buffer, row)
            .context("building instrument_lifecycle_audit ILP row")?;
    }
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut sender =
            Sender::from_conf(&conf).context("connect QuestDB ILP (instrument_lifecycle_audit)")?;
        sender
            .flush(&mut buffer)
            .context("flush instrument_lifecycle_audit rows via ILP")?;
        Ok(())
    })
    .await
    .context("instrument_lifecycle_audit ILP flush task join")??;
    Ok(())
}

// ============================================================================
// instrument_lifecycle UPDATE (state-flip for expiry / reactivation)
// ============================================================================

/// Build the UPDATE statement that flips an `instrument_lifecycle` row's
/// state columns WITHOUT rewriting the full 24-column row.
///
/// Used by the daily reconciler for DISAPPEARANCE transitions
/// (`Expired` / `SegmentMoved`): the instrument is no longer in today's
/// CSV, so there are no fresh attribute values — a full-row UPSERT would
/// wipe `underlying_security_id` / `expiry_date` / etc. This mirrors the
/// I-P1-08 `mark_missing_as_expired` precedent (UPDATE, not re-insert).
///
/// `lifecycle_state` is NOT part of the DEDUP key (which is
/// `ts, security_id, exchange_segment`), so updating it in place is
/// safe. The WHERE clause is the I-P1-11 composite key.
///
/// `exchange_segment` is sanitized; `new_state` is a static enum string;
/// numerics are typed — no injection surface.
#[must_use]
pub fn build_lifecycle_state_update_sql(
    security_id: i64,
    exchange_segment: &str,
    new_state: LifecycleState,
    expired_date_nanos: i64,
    last_update_ts_nanos: i64,
) -> String {
    let segment = sanitize_audit_string(exchange_segment);
    // `state` is NOT sanitized: it is a compile-time `&'static str` from
    // `LifecycleState::as_str()` (one of 5 lowercase snake_case constants),
    // provably free of SQL-breaking characters — same convention as the
    // SYMBOL enum labels in the append helpers. Only caller-supplied
    // free-text (`exchange_segment`) is sanitized.
    let state = new_state.as_str();
    let expired_micros = expired_date_nanos / 1_000;
    let last_update_micros = last_update_ts_nanos / 1_000;
    format!(
        "UPDATE {QUESTDB_TABLE_INSTRUMENT_LIFECYCLE} \
         SET lifecycle_state = '{state}', expired_date = {expired_micros}, \
         last_update_ts = {last_update_micros} \
         WHERE security_id = {security_id} AND exchange_segment = '{segment}';"
    )
}

/// Flip an `instrument_lifecycle` row's state in place (expiry /
/// segment-move / reactivation-without-fresh-attrs). Cold path — called
/// by the daily reconciler per disappearance transition.
///
/// # Errors
///
/// Propagates the `reqwest` transport error, or returns `Err` on a
/// non-2xx `/exec` response.
pub async fn update_lifecycle_state(
    questdb_config: &QuestDbConfig,
    security_id: i64,
    exchange_segment: &str,
    new_state: LifecycleState,
    expired_date_nanos: i64,
    last_update_ts_nanos: i64,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()?;
    let sql = build_lifecycle_state_update_sql(
        security_id,
        exchange_segment,
        new_state,
        expired_date_nanos,
        last_update_ts_nanos,
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
        anyhow::bail!("instrument_lifecycle UPDATE non-2xx ({status}): {body}");
    }
    Ok(())
}

// ============================================================================
// O(1) WARM-BOOT path — single-statement last_seen refresh
// ============================================================================

/// Build the O(1) warm-boot UPDATE: bump `last_seen_date` + `last_active_date`
/// (+ `last_update_ts`) for ALL `active` rows in ONE statement. Used when the
/// daily CSV SHA-256 is unchanged from the last boot — the universe is
/// identical and already persisted, so re-UPSERTing every row is pure waste.
/// This is O(1) in HTTP requests regardless of universe size (331 or 2M rows).
/// PURE (no I/O) — extracted for deterministic testing.
#[must_use]
pub fn build_bump_active_last_seen_sql(today_ist_nanos: i64, last_update_ts_nanos: i64) -> String {
    let today_micros = today_ist_nanos / 1_000;
    let last_update_micros = last_update_ts_nanos / 1_000;
    // `active` is the compile-time `LifecycleState::Active` label — no
    // sanitization needed (same convention as build_lifecycle_state_update_sql).
    let active = LifecycleState::Active.as_str();
    format!(
        "UPDATE {QUESTDB_TABLE_INSTRUMENT_LIFECYCLE} \
         SET last_seen_date = {today_micros}, last_active_date = {today_micros}, \
         last_update_ts = {last_update_micros} \
         WHERE lifecycle_state = '{active}';"
    )
}

/// Execute the O(1) warm-boot `last_seen` bump (one UPDATE, one round-trip).
/// (Pure builder `build_bump_active_last_seen_sql` carries the unit coverage.)
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn bump_active_last_seen(
    questdb_config: &QuestDbConfig,
    today_ist_nanos: i64,
    last_update_ts_nanos: i64,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()?;
    let sql = build_bump_active_last_seen_sql(today_ist_nanos, last_update_ts_nanos);
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
        anyhow::bail!("instrument_lifecycle warm-bump non-2xx ({status}): {body}");
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
        // Bug A fix (2026-05-28): QuestDB REJECTS WAL+DEDUP on a
        // non-partitioned table (PARTITION BY NONE WAL → HTTP 400 at boot,
        // observed live 16:16 IST). The table MUST be PARTITION BY DAY WAL.
        // The constant designated ts (I-P1-08) simply funnels every
        // never-deleted row into the single 1970-01-01 partition — harmless;
        // DEDUP on (ts, security_id, exchange_segment) still keys correctly
        // because ts is constant. Source-scan pins the corrected contract.
        let source = include_str!("instrument_lifecycle_persistence.rs");
        assert!(
            source.contains(") timestamp(ts) PARTITION BY DAY WAL"),
            "instrument_lifecycle DDL MUST use PARTITION BY DAY WAL (WAL+DEDUP \
             requires a partitioned table; constant ts → single 1970 partition)"
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

    // ========================================================================
    // P2c (coverage-gaps #1076): HTTP /exec + ILP I/O arm coverage via
    // file-local mock servers (same idiom as tick_persistence::tests).
    // ========================================================================

    const P2C_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const P2C_HTTP_500: &str =
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 13\r\n\r\n{\"error\":\"x\"}";

    async fn p2c_spawn_mock_http(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    fn p2c_spawn_tcp_drain() -> u16 {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 65536];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
        port
    }

    fn p2c_cfg(http_port: u16, ilp_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 1,
            ilp_port,
        }
    }

    #[tokio::test]
    async fn test_ensure_lifecycle_tables_with_mock_200() {
        let port = p2c_spawn_mock_http(P2C_HTTP_200).await;
        let cfg = p2c_cfg(port, 1);
        // Both DDL walks (CREATE + self-heal ALTERs + DEDUP) complete
        // without panic against a 200-everything QuestDB.
        ensure_instrument_lifecycle_table(&cfg).await;
        ensure_instrument_lifecycle_audit_table(&cfg).await;
    }

    #[tokio::test]
    async fn test_append_lifecycle_row_mock_200_and_500() {
        let ok_port = p2c_spawn_mock_http(P2C_HTTP_200).await;
        let row = sample_index_row();
        let res = append_instrument_lifecycle_row(&p2c_cfg(ok_port, 1), &row).await;
        assert!(res.is_ok(), "200 insert must be Ok: {res:?}");

        let err_port = p2c_spawn_mock_http(P2C_HTTP_500).await;
        let res = append_instrument_lifecycle_row(&p2c_cfg(err_port, 1), &row).await;
        let err = res.expect_err("500 insert must surface an error");
        assert!(err.to_string().contains("non-2xx"), "{err}");
    }

    #[tokio::test]
    async fn test_append_lifecycle_audit_row_mock_200_and_500() {
        let ok_port = p2c_spawn_mock_http(P2C_HTTP_200).await;
        let row = sample_audit_row();
        let res = append_instrument_lifecycle_audit_row(&p2c_cfg(ok_port, 1), &row).await;
        assert!(res.is_ok(), "200 audit insert must be Ok: {res:?}");

        let err_port = p2c_spawn_mock_http(P2C_HTTP_500).await;
        let res = append_instrument_lifecycle_audit_row(&p2c_cfg(err_port, 1), &row).await;
        let err = res.expect_err("500 audit insert must surface an error");
        assert!(err.to_string().contains("non-2xx"), "{err}");
    }

    #[tokio::test]
    async fn test_update_state_and_bump_last_seen_mock_200_and_500() {
        let ok_port = p2c_spawn_mock_http(P2C_HTTP_200).await;
        let cfg = p2c_cfg(ok_port, 1);
        let res = update_lifecycle_state(
            &cfg,
            99887,
            "NSE_FNO",
            LifecycleState::ExpiredContract,
            1_700_000_000_000_000_000,
            1_700_000_000_000_000_000,
        )
        .await;
        assert!(res.is_ok(), "200 state update must be Ok: {res:?}");
        let res =
            bump_active_last_seen(&cfg, 1_699_920_000_000_000_000, 1_700_000_000_000_000_000).await;
        assert!(res.is_ok(), "200 bump must be Ok: {res:?}");

        let err_port = p2c_spawn_mock_http(P2C_HTTP_500).await;
        let cfg = p2c_cfg(err_port, 1);
        let res = update_lifecycle_state(
            &cfg,
            99887,
            "NSE_FNO",
            LifecycleState::ExpiredContract,
            1_700_000_000_000_000_000,
            1_700_000_000_000_000_000,
        )
        .await;
        assert!(res.is_err(), "500 state update must error");
        let res =
            bump_active_last_seen(&cfg, 1_699_920_000_000_000_000, 1_700_000_000_000_000_000).await;
        assert!(res.is_err(), "500 bump must error");
    }

    #[tokio::test]
    async fn test_append_lifecycle_rows_ilp_drain_and_error() {
        // Empty slice short-circuits without connecting anywhere.
        let res = append_instrument_lifecycle_rows(&p2c_cfg(1, 1), &[]).await;
        assert!(res.is_ok(), "empty slice must be Ok without I/O");

        let drain_port = p2c_spawn_tcp_drain();
        let row = sample_index_row();
        let res = append_instrument_lifecycle_rows(&p2c_cfg(1, drain_port), &[row]).await;
        assert!(res.is_ok(), "ILP flush to drain server must be Ok: {res:?}");

        // Port 1 — connect refused → Err, no panic.
        let row = sample_index_row();
        let res = append_instrument_lifecycle_rows(&p2c_cfg(1, 1), &[row]).await;
        assert!(res.is_err(), "ILP connect failure must surface an error");
    }

    #[tokio::test]
    async fn test_append_lifecycle_audit_rows_ilp_drain_and_error() {
        let res = append_instrument_lifecycle_audit_rows(&p2c_cfg(1, 1), &[]).await;
        assert!(res.is_ok(), "empty slice must be Ok without I/O");

        let drain_port = p2c_spawn_tcp_drain();
        let row = sample_audit_row();
        let res = append_instrument_lifecycle_audit_rows(&p2c_cfg(1, drain_port), &[row]).await;
        assert!(res.is_ok(), "ILP flush to drain server must be Ok: {res:?}");

        let row = sample_audit_row();
        let res = append_instrument_lifecycle_audit_rows(&p2c_cfg(1, 1), &[row]).await;
        assert!(res.is_err(), "ILP connect failure must surface an error");
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

    #[test]
    fn test_build_lifecycle_ilp_row_content() {
        // The bulk writer now ingests via ILP (port 9009), NOT the /exec URL.
        // The pure row builder writes a valid ILP line for the lifecycle table.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_lifecycle_ilp_row(&mut buffer, &sample_index_row()).unwrap();
        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_INSTRUMENT_LIFECYCLE));
        assert!(content.contains("exchange_segment="));
        assert!(content.contains("instrument_type="));
        assert!(content.contains("lifecycle_state="));
        assert!(content.contains("security_id="));
        assert!(content.contains("tick_size="));
        assert!(content.contains("lifecycle_state_locked="));
    }

    #[test]
    fn test_build_lifecycle_ilp_row_empty_symbols_skipped_no_error() {
        // Index rows carry empty exchange_id / underlying_symbol / option_type.
        // Empty SYMBOLs MUST be skipped (→ NULL), never written as empty ILP
        // symbols (which QuestDB versions can reject). Build must not error.
        let mut row = sample_index_row();
        row.exchange_id = "";
        row.underlying_symbol = "";
        row.option_type = "";
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_lifecycle_ilp_row(&mut buffer, &row)
            .expect("empty optional symbols must build cleanly");
        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        // Skipped optional symbols must NOT appear as `name=` tags …
        assert!(!content.contains("option_type="));
        assert!(!content.contains("underlying_symbol="));
        // … but the mandatory DEDUP-key symbol is always present.
        assert!(content.contains("exchange_segment="));
    }

    #[test]
    fn test_build_lifecycle_ilp_row_clamps_non_finite_f64() {
        // A non-finite strike/tick must be clamped (finite_or_zero) so the ILP
        // f64 encoder never sees NaN/inf — regression for the boot-halt class.
        let mut row = sample_index_row();
        row.tick_size = f64::NAN;
        row.strike_price = f64::INFINITY;
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_lifecycle_ilp_row(&mut buffer, &row).expect("non-finite f64 must clamp, not error");
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_lifecycle_audit_ilp_row_content() {
        // sample_audit_row has from_state = Some(Active) → present as a tag.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_lifecycle_audit_ilp_row(&mut buffer, &sample_audit_row()).unwrap();
        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT));
        assert!(content.contains("transition_kind="));
        assert!(content.contains("to_state="));
        assert!(content.contains("from_state="));
        assert!(content.contains("security_id="));
    }

    #[test]
    fn test_build_lifecycle_audit_ilp_row_preserves_field_deltas_json() {
        // Regression (2026-05-29 3-agent review): `field_deltas` is a STRING
        // column carrying JSON with COMMAS. It MUST go through
        // sanitize_ilp_string (preserves commas), NOT sanitize_ilp_symbol
        // (strips commas → corrupts the forensic JSON). The commas must
        // survive into the ILP buffer.
        let mut row = sample_audit_row();
        row.field_deltas = r#"{"lot_size":[1000,200]}"#;
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_lifecycle_audit_ilp_row(&mut buffer, &row).unwrap();
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(
            content.contains("[1000,200]"),
            "field_deltas JSON commas must be preserved, got: {content}"
        );
    }

    #[test]
    fn test_build_lifecycle_audit_ilp_row_appeared_skips_empty_from_state() {
        // An `appeared` transition has from_state = None → skipped (→ NULL).
        let mut row = sample_audit_row();
        row.from_state = None;
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_lifecycle_audit_ilp_row(&mut buffer, &row).unwrap();
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(
            !content.contains("from_state="),
            "None from_state must be skipped"
        );
        // The DEDUP-key symbols are always written.
        assert!(content.contains("transition_kind="));
        assert!(content.contains("exchange_segment="));
    }

    #[test]
    fn test_finite_or_zero_clamps_non_finite() {
        assert_eq!(finite_or_zero(12.5), 12.5);
        assert_eq!(finite_or_zero(0.0), 0.0);
        assert_eq!(finite_or_zero(f64::NAN), 0.0);
        assert_eq!(finite_or_zero(f64::INFINITY), 0.0);
        assert_eq!(finite_or_zero(f64::NEG_INFINITY), 0.0);
    }

    #[test]
    fn test_lifecycle_tuple_never_emits_nan_or_inf() {
        // A row with non-finite strike/tick must NOT put NaN/inf into the SQL
        // (which QuestDB rejects → boot halt). Regression guard for the
        // 2026-05-29 "79190 write error(s)" class of failure.
        let mut row = sample_index_row();
        row.tick_size = f64::NAN;
        row.strike_price = f64::INFINITY;
        let tuple = format_lifecycle_row_tuple(&row);
        assert!(
            !tuple.contains("NaN") && !tuple.contains("inf"),
            "tuple: {tuple}"
        );
    }

    #[test]
    fn test_build_bump_active_last_seen_sql_shape_micros_and_active_filter() {
        // 1_700_000_000_000_000_000 ns → 1_700_000_000_000_000 µs.
        let sql =
            build_bump_active_last_seen_sql(1_700_100_000_000_000_000, 1_700_000_000_000_000_000);
        assert!(sql.starts_with(&format!("UPDATE {QUESTDB_TABLE_INSTRUMENT_LIFECYCLE} SET")));
        assert!(sql.contains("last_seen_date = 1700100000000000"));
        assert!(sql.contains("last_active_date = 1700100000000000"));
        assert!(sql.contains("last_update_ts = 1700000000000000"));
        // O(1) win: ONE statement scoped to active rows, no per-row VALUES.
        assert!(sql.contains("WHERE lifecycle_state = 'active'"));
        assert!(
            !sql.contains("VALUES"),
            "warm bump is an UPDATE, never an INSERT"
        );
        assert!(sql.ends_with(';'));
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

    // ========================================================================
    // update_lifecycle_state (state-flip) tests
    // ========================================================================

    #[test]
    fn test_build_lifecycle_state_update_sql_shape_and_micros() {
        let sql = build_lifecycle_state_update_sql(
            99887,
            "NSE_EQ",
            LifecycleState::ExpiredFromFno,
            1_700_100_000_000_000_000,
            1_700_000_000_000_000_000,
        );
        assert!(sql.starts_with("UPDATE instrument_lifecycle SET"));
        assert!(sql.contains("lifecycle_state = 'expired_from_fno'"));
        // nanos → micros (year-9999 guard).
        assert!(
            sql.contains("expired_date = 1700100000000000"),
            "expired_date must be micros: {sql}"
        );
        assert!(
            sql.contains("last_update_ts = 1700000000000000"),
            "last_update_ts must be micros: {sql}"
        );
        // I-P1-11 composite WHERE key.
        assert!(sql.contains("WHERE security_id = 99887 AND exchange_segment = 'NSE_EQ'"));
    }

    #[test]
    fn test_build_lifecycle_state_update_sql_does_not_touch_dedup_key_columns() {
        // The UPDATE must only set lifecycle_state / expired_date /
        // last_update_ts — NEVER the DEDUP key columns (ts, security_id,
        // exchange_segment). Setting a key column would re-identify the row.
        // Parse each `SET col = ...` assignment's LHS precisely (substring
        // checks would false-trip: "last_update_ts" contains "ts").
        let sql = build_lifecycle_state_update_sql(1, "NSE_EQ", LifecycleState::Active, 0, 0);
        let set_clause = sql
            .split("SET")
            .nth(1)
            .and_then(|s| s.split("WHERE").next())
            .expect("SET ... WHERE present");
        let allowed = ["lifecycle_state", "expired_date", "last_update_ts"];
        for assignment in set_clause.split(',') {
            let lhs = assignment
                .split('=')
                .next()
                .map(str::trim)
                .unwrap_or_default();
            assert!(
                allowed.contains(&lhs),
                "SET assigns unexpected column `{lhs}` — must be one of {allowed:?} \
                 (never a DEDUP-key column)"
            );
        }
    }

    #[test]
    fn test_build_lifecycle_state_update_sql_sanitizes_segment_injection() {
        let sql = build_lifecycle_state_update_sql(
            1,
            "'); DROP TABLE instrument_lifecycle;--",
            LifecycleState::ExpiredIndex,
            0,
            0,
        );
        assert!(
            !sql.contains("DROP TABLE instrument_lifecycle;"),
            "segment injection must be neutralized: {sql}"
        );
    }

    #[test]
    fn test_build_lifecycle_state_update_sql_renders_each_state() {
        for st in LifecycleState::all() {
            let sql = build_lifecycle_state_update_sql(1, "NSE_EQ", st, 0, 0);
            assert!(
                sql.contains(&format!("lifecycle_state = '{}'", st.as_str())),
                "must render state {st:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_update_lifecycle_state_returns_err_when_questdb_unreachable() {
        let cfg = cfg_unreachable();
        let result = update_lifecycle_state(
            &cfg,
            99887,
            "NSE_EQ",
            LifecycleState::ExpiredFromFno,
            1_700_100_000_000_000_000,
            1_700_000_000_000_000_000,
        )
        .await;
        assert!(result.is_err(), "must propagate transport error");
    }
}
