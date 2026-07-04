//! `index_constituency` persistence — the §31-item-2 full index→constituents
//! mapping, queryable from QuestDB.
//!
//! NTM Sub-PR "Map-A" (operator 2026-06-06 — "full per-index mapping … I need
//! guarantee and assurance"). Stores one row per `(trading_date, index_name,
//! security_id, exchange_segment)` so BOTH directions are answerable in SQL:
//!
//! ```sql
//! -- which stocks are in NIFTY BANK today?
//! SELECT symbol_name FROM index_constituency
//!   WHERE ts = today() AND index_name = 'Nifty Bank';
//! -- which indices is RELIANCE (sid 2885) in today?
//! SELECT index_name FROM index_constituency
//!   WHERE ts = today() AND security_id = 2885;
//! ```
//!
//! This is the MAP-ONLY surface (§31 item 2). It does NOT change the live
//! WebSocket subscription, which stays the NTM-union-only set (the 2-WS lock +
//! `MAX_DAILY_UNIVERSE_SIZE`). Boot wiring (fetch all ~46 lists → resolve
//! per-index → write here) lands in the companion "Map-B" PR; this PR ships the
//! self-contained, fully-tested persistence layer first.
//!
//! ## Schema invariants (mirrors the canonical `instrument_lifecycle` template)
//! * **Composite DEDUP** `(ts, index_name, security_id, exchange_segment, feed)`
//!   — I-P1-11 (Dhan reuses `security_id` across segments, so segment is
//!   mandatory) + `index_name` (a stock is in many indices) + `feed`
//!   (operator override 2026-06-28) + `ts`. The designated `ts` is PINNED to a
//!   CONSTANT epoch 0 ([`index_constituency_designated_ts_nanos`]) — exactly
//!   like `instrument_lifecycle` (`lifecycle_designated_ts_nanos() -> 0`,
//!   I-P1-08). QuestDB requires the designated `ts` in the DEDUP key, so we pin
//!   it to 0 and let the business key
//!   `(index_name, security_id, exchange_segment, feed)` do the deduplication.
//!   This is a CURRENT-STATE map (one row per (index, stock) per feed,
//!   overwritten in place each boot) — NOT per-day history. Pinning `ts`
//!   eliminates the cross-day duplicate accumulation (~742 rows/day) the
//!   previous day-floored `trading_date_ist` caused.
//! * Idempotent `CREATE TABLE IF NOT EXISTS` + `ALTER ADD COLUMN IF NOT EXISTS`
//!   (schema self-heal — `observability-architecture.md`).
//! * ILP bulk ingest (port 9009) — the per-index mapping is large (~46 indices ×
//!   constituents, heavily overlapping → thousands of rows), so the `/exec` URL
//!   door would overflow; ILP is the real pipe.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21.

#![cfg(feature = "daily_universe_fetcher")]

use std::time::Duration;

use anyhow::Context;
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_ilp_symbol;

/// `/exec` HTTP timeout for DDL. Matches the other `*_persistence` modules.
const QUESTDB_EXEC_TIMEOUT_SECS: u64 = 10;

/// Broker-source label stamped on every constituency row today (operator
/// override 2026-06-28 — `feed` is in the DEDUP key on every persisted table).
/// The index→constituents map is built ONLY from Dhan-resolved SIDs today.
/// Replay-stable `&'static str` from the canonical `Feed` enum.
pub const INDEX_CONSTITUENCY_FEED_DHAN: &str = tickvault_common::feed::Feed::Dhan.as_str();

/// Wire-format table name. Stable across releases — operator SQL + any future
/// S3 archive job depend on the exact string.
pub const QUESTDB_TABLE_INDEX_CONSTITUENCY: &str = "index_constituency";

/// Composite DEDUP key — one CURRENT-STATE row per (index, stock, feed).
///
/// `ts` is the designated timestamp, satisfying QuestDB's
/// designated-ts-in-DEDUP requirement; it is PINNED to a constant epoch 0
/// ([`index_constituency_designated_ts_nanos`]) so DEDUP fires on the business
/// key alone and the table never accumulates a fresh row-set per trading day.
/// `index_name` distinguishes the same stock across its many indices.
/// `security_id` + `exchange_segment` is the I-P1-11 composite identity.
/// `feed` is in-key per the 2026-06-28 operator override.
pub const DEDUP_KEY_INDEX_CONSTITUENCY: &str =
    "ts, index_name, security_id, exchange_segment, feed";

/// The pinned constant designated timestamp (epoch 0) for the
/// UPSERT-in-place `index_constituency` current-state master table.
///
/// Mirrors `instrument_lifecycle_persistence::lifecycle_designated_ts_nanos()`
/// — the I-P1-08 rationale: QuestDB requires the designated `ts` in the DEDUP
/// key, so we pin it to 0 and let the business key
/// `(index_name, security_id, exchange_segment, feed)` do the deduplication.
/// Prevents the cross-day duplicate accumulation (~742 rows/day) the
/// day-floored `trading_date_ist` caused.
#[must_use]
pub const fn index_constituency_designated_ts_nanos() -> i64 {
    0
}

/// Column list shared by the DDL — one source so the writer + table cannot
/// drift. (The ILP writer names each column explicitly; this constant pins the
/// DDL ordering + the `test_ddl_contains_expected_columns` ratchet.)
///
/// Test-only: referenced exclusively by the DDL-column ratchet tests, so it is
/// `#[cfg(test)]`-gated to avoid a dead-code lint on the production lib build
/// (`cargo clippy --all-targets -- -D warnings`).
#[cfg(test)]
const INDEX_CONSTITUENCY_COLUMNS: &[&str] = &[
    "ts",
    "index_name",
    "security_id",
    "exchange_segment",
    "symbol_name",
    "isin",
    "via_isin",
    "source",
    "dry_run",
    "feed",
];

/// One `index_constituency` row — borrows so the bulk writer allocates nothing
/// per row beyond the ILP buffer.
#[derive(Debug, Clone, Copy)]
pub struct IndexConstituencyRow<'a> {
    /// IST trading-date midnight, nanoseconds. RETAINED for caller
    /// compatibility (both feed call sites still pass it); it is NO LONGER the
    /// stamped designated `ts` — the builder pins `ts` to the constant
    /// [`index_constituency_designated_ts_nanos`] (epoch 0) so cross-day
    /// duplicates collapse. Kept on the struct for a stable ABI + zero churn at
    /// the call sites.
    pub trading_date_ist_nanos: i64,
    /// Index display name (e.g. `"Nifty Bank"`).
    pub index_name: &'a str,
    /// Resolved Dhan `security_id` of the constituent.
    pub security_id: i64,
    /// Exchange segment (always `"NSE_EQ"` for cash constituents).
    pub exchange_segment: &'a str,
    /// NSE ticker (e.g. `"RELIANCE"`).
    pub symbol_name: &'a str,
    /// ISIN used for the match (empty if symbol-fallback).
    pub isin: &'a str,
    /// `true` if matched via the ISIN primary key, `false` via symbol fallback.
    pub via_isin: bool,
    /// Provenance — the constituency source (e.g. `"niftyindices"`).
    pub source: &'a str,
    /// `true` for a `--dry-run-universe` boot (isolation per §27).
    pub dry_run: bool,
    /// Broker-source label (`dhan`/`groww`). Part of the DEDUP key (operator
    /// override 2026-06-28). Always non-empty (stamped
    /// [`INDEX_CONSTITUENCY_FEED_DHAN`] today).
    pub feed: &'a str,
}

/// Idempotent `CREATE TABLE IF NOT EXISTS` for `index_constituency`.
///
/// Cold path — called once at boot before the bulk write. Logs (does not
/// return `Err`) so a transient DDL hiccup mirrors the other lifecycle DDLs;
/// the bulk write that follows surfaces a hard failure.
// TEST-EXEMPT: network DDL (live QuestDB) — mirrors the canonical ensure_*_table template; boot integration exercises it.
pub async fn ensure_index_constituency_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "index_constituency DDL: HTTP client build failed");
            return;
        }
    };
    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_INDEX_CONSTITUENCY} (\
            ts TIMESTAMP, \
            index_name SYMBOL, \
            security_id LONG, \
            exchange_segment SYMBOL, \
            symbol_name SYMBOL, \
            isin SYMBOL, \
            via_isin BOOLEAN, \
            source SYMBOL, \
            dry_run BOOLEAN, \
            feed SYMBOL\
        ) timestamp(ts) PARTITION BY DAY WAL \
        DEDUP UPSERT KEYS({DEDUP_KEY_INDEX_CONSTITUENCY});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                "index_constituency table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                status = %resp.status(),
                "index_constituency DDL non-2xx"
            );
        }
        Err(err) => {
            warn!(?err, "index_constituency DDL request failed");
        }
    }

    // Feed-in-key self-heal (operator override 2026-06-28 — supersedes the
    // 2026-06-19 NON-key label). Additive + idempotent, IN THIS ORDER:
    // ADD COLUMN → backfill NULL→'dhan' → re-enable DEDUP with `feed` in the
    // key. The CREATE above already ships the new column+key on greenfield;
    // these run for tables created before this change. The backfill MUST precede
    // the DEDUP-ENABLE so a new `feed='dhan'` row upserts over a legacy NULL-feed
    // row instead of duplicating it. Never drops the table (SEBI retention).
    let self_heal = [
        format!(
            "ALTER TABLE {QUESTDB_TABLE_INDEX_CONSTITUENCY} ADD COLUMN IF NOT EXISTS feed SYMBOL;"
        ),
        format!(
            "UPDATE {QUESTDB_TABLE_INDEX_CONSTITUENCY} \
             SET feed = '{INDEX_CONSTITUENCY_FEED_DHAN}' WHERE feed IS NULL;"
        ),
        format!(
            "ALTER TABLE {QUESTDB_TABLE_INDEX_CONSTITUENCY} \
             DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_INDEX_CONSTITUENCY});"
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
                    table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                    status = %resp.status(),
                    ddl = ddl.as_str(),
                    "index_constituency feed self-heal non-2xx"
                );
            }
            Err(err) => {
                warn!(
                    ?err,
                    ddl = ddl.as_str(),
                    "index_constituency feed self-heal request failed"
                );
            }
        }
    }
}

/// One-shot marker file path for the ts-pin migration. After the legacy
/// day-floored `index_constituency` rows are cleared ONCE on a deployment, this
/// file records the fact so subsequent boots skip the TRUNCATE entirely. To
/// force a re-run (e.g. after restoring an older QuestDB backup), delete it.
/// Mirrors the `shadow_persistence` marker-gate convention (`data/state/…`).
pub const INDEX_CONSTITUENCY_TS_PIN_MARKER_PATH: &str =
    "data/state/index_constituency_ts_pin_v1.done";

/// PURE marker-gate predicate: should the one-shot ts-pin migration run?
///
/// `true` when the marker file is ABSENT (migration not yet done on this
/// deployment); `false` when PRESENT (already done → skip). Split out so the
/// one-shot semantics are unit-testable without live QuestDB.
#[must_use]
pub fn index_constituency_migration_should_run(marker_path: &std::path::Path) -> bool {
    !marker_path.exists()
}

/// Ordering gate for the one-shot ts-pin TRUNCATE migration (FIX 13a,
/// 2026-07-04).
///
/// `TRUNCATE TABLE index_constituency` wipes ALL rows — QuestDB has no
/// row-level `DELETE ... WHERE feed='dhan'`, so the migration cannot be
/// feed-scoped. The Groww shared-master writer persists `feed='groww'` rows
/// into the SAME table from an unordered fire-and-forget boot spawn, so
/// without ordering the migration could wipe another feed's just-written
/// rows. Writers of OTHER feeds await this gate (bounded) before their
/// `index_constituency` append; the migration marks it complete on EVERY
/// exit path (ran / skipped-via-marker / failed — the failure case is safe
/// because the marker is not written and the NEXT boot both re-truncates
/// and re-persists, DEDUP-idempotent).
///
/// Instance-testable struct + one process-wide static accessor
/// ([`index_constituency_migration_gate`]) so unit tests never share global
/// state.
pub struct MigrationGate {
    done: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl MigrationGate {
    /// Fresh, un-marked gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            done: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Mark the migration COMPLETE (ran, skipped, or failed for this boot)
    /// and wake every waiter. Idempotent.
    pub fn mark_complete(&self) {
        self.done.store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Whether the gate has been marked complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Await the gate with a bounded timeout. `true` = gate opened; `false` =
    /// timed out (the caller proceeds degrade-safe — e.g. a Groww-only boot
    /// never runs the Dhan-lane migration, so the truncate cannot fire and
    /// proceeding is CORRECT). Registration-before-recheck ordering makes the
    /// `notify_waiters` wake race-free.
    pub async fn wait(&self, timeout: Duration) -> bool {
        if self.is_complete() {
            return true;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            if self.is_complete() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.is_complete();
            }
        }
    }
}

impl Default for MigrationGate {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide ts-pin migration gate instance (`OnceLock` accessor
/// convention, mirroring `http_client::shared_probe_client`).
#[must_use]
pub fn index_constituency_migration_gate() -> &'static MigrationGate {
    static GATE: std::sync::OnceLock<MigrationGate> = std::sync::OnceLock::new();
    GATE.get_or_init(MigrationGate::new)
}

/// One-time, marker-gated `TRUNCATE TABLE index_constituency` to clear the
/// legacy day-floored rows accumulated before the `ts`-pin fix.
///
/// Before this fix the designated `ts` was the day-floored IST trading-date
/// midnight, so each trading day wrote a fresh ~742-row set that DEDUP never
/// collapsed (the day value was in the key). With `ts` now pinned to epoch 0
/// the NEW writes UPSERT in place, but the legacy rows (at the old per-day `ts`
/// values) never get overwritten — they must be cleared ONCE. After the
/// truncate, the same boot's normal `index_constituency` write rewrites the
/// current ~742-row set at `ts=0`.
///
/// **Degrade-safe** (operator-charter Rule 6 — this is a cold-path master
/// migration, NOT the ticks/order path):
/// * marker PRESENT → skip (one-shot).
/// * TRUNCATE fails / QuestDB down → `error!` (Telegram-routable), the marker
///   is NOT written so a later healthy boot retries, and boot is NEVER blocked.
/// * TRUNCATE succeeds → `info!` + write the marker so subsequent boots skip.
///
/// MUST be awaited BEFORE the table's normal boot write so the order is
/// truncate → write. `index_constituency` is fully re-derivable from the
/// niftyindices/Dhan CSV on every boot, so clearing it loses no record that
/// was not already reproducible (SEBI-safe, same current-state model as
/// `instrument_lifecycle`).
/// FIX 13a (2026-07-04): outer wrapper — the inner body carries the real
/// logic; the wrapper marks the [`index_constituency_migration_gate`]
/// complete on EVERY exit path (a future early return in the body can never
/// forget to open the gate for the waiting Groww writer).
// WIRING-EXEMPT: boot wiring lives in crates/app/src/index_constituency_boot.rs before the normal write.
// TEST-EXEMPT: network I/O orchestration (live QuestDB TRUNCATE) — the pure marker-gate predicate `index_constituency_migration_should_run` + the `MigrationGate` primitives are unit-tested.
pub async fn migrate_index_constituency_truncate_once(questdb_config: &QuestDbConfig) {
    migrate_index_constituency_truncate_once_inner(questdb_config).await;
    index_constituency_migration_gate().mark_complete();
}

// TEST-EXEMPT: network I/O orchestration (live QuestDB TRUNCATE) — see the public wrapper above.
async fn migrate_index_constituency_truncate_once_inner(questdb_config: &QuestDbConfig) {
    let marker_path = std::path::Path::new(INDEX_CONSTITUENCY_TS_PIN_MARKER_PATH);
    if !index_constituency_migration_should_run(marker_path) {
        tracing::debug!(
            marker = INDEX_CONSTITUENCY_TS_PIN_MARKER_PATH,
            "index_constituency ts-pin migration already done on this deployment — skipping"
        );
        return;
    }

    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                ?err,
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                "index_constituency ts-pin migration: HTTP client build failed — \
                 marker NOT written, will retry next boot"
            );
            return;
        }
    };

    let truncate_ddl = format!("TRUNCATE TABLE {QUESTDB_TABLE_INDEX_CONSTITUENCY};");
    match client
        .get(&base_url)
        .query(&[("query", truncate_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                "one-time ts-pin migration: truncated legacy day-floored rows"
            );
        }
        Ok(resp) => {
            error!(
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                status = %resp.status(),
                "index_constituency ts-pin migration: TRUNCATE non-2xx — \
                 marker NOT written, will retry next boot"
            );
            return;
        }
        Err(err) => {
            error!(
                ?err,
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                "index_constituency ts-pin migration: TRUNCATE request failed — \
                 marker NOT written, will retry next boot"
            );
            return;
        }
    }

    // Best-effort marker write so subsequent boots skip the TRUNCATE. If it
    // can't be written, the migration just repeats next boot (harmless — the
    // table is immediately rewritten from CSV).
    if let Some(parent) = marker_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        warn!(
            ?err,
            path = %parent.display(),
            "index_constituency ts-pin migration: failed to create data/state/ dir for marker"
        );
        return;
    }
    if let Err(err) = std::fs::write(
        marker_path,
        "index_constituency ts-pin migration — legacy day-floored rows cleared on this deployment.\n",
    ) {
        warn!(
            ?err,
            path = INDEX_CONSTITUENCY_TS_PIN_MARKER_PATH,
            "index_constituency ts-pin migration: failed to write marker (will repeat next boot)"
        );
    } else {
        info!(
            path = INDEX_CONSTITUENCY_TS_PIN_MARKER_PATH,
            "index_constituency ts-pin migration complete — marker written"
        );
    }
}

/// Write one `index_constituency` row into an ILP [`Buffer`]. PURE builder
/// (no network) — deterministically unit-tested via `buffer.as_bytes()`.
///
/// ILP rules: all symbol tags before any field; an empty optional symbol tag
/// (`isin`) is SKIPPED (→ NULL, not the empty symbol `''`). The designated `ts` is
/// PINNED to constant epoch 0 ([`index_constituency_designated_ts_nanos`]) so
/// DEDUP fires on the business key `(index_name, security_id, exchange_segment,
/// feed)` alone — a current-state map, never per-day history. `row.trading_date_ist_nanos`
/// is intentionally NOT used as the designated ts (retained for caller ABI).
fn build_index_constituency_ilp_row(
    buffer: &mut Buffer,
    row: &IndexConstituencyRow<'_>,
) -> anyhow::Result<()> {
    buffer.table(QUESTDB_TABLE_INDEX_CONSTITUENCY)?;
    // ── symbol tags first (ILP requires all tags before any field). ──
    buffer.symbol("index_name", sanitize_ilp_symbol(row.index_name).as_ref())?;
    buffer.symbol(
        "exchange_segment",
        sanitize_ilp_symbol(row.exchange_segment).as_ref(),
    )?;
    buffer.symbol("symbol_name", sanitize_ilp_symbol(row.symbol_name).as_ref())?;
    buffer.symbol("source", sanitize_ilp_symbol(row.source).as_ref())?;
    // `feed` is a DEDUP-key SYMBOL — ALWAYS written (operator override 2026-06-28).
    buffer.symbol("feed", sanitize_ilp_symbol(row.feed).as_ref())?;
    // Optional SYMBOL — skip when empty (→ NULL, not empty ILP symbol).
    if !row.isin.is_empty() {
        buffer.symbol("isin", sanitize_ilp_symbol(row.isin).as_ref())?;
    }
    // ── Fields. ──
    buffer.column_i64("security_id", row.security_id)?;
    buffer.column_bool("via_isin", row.via_isin)?;
    buffer.column_bool("dry_run", row.dry_run)?;
    // Pin the designated ts to constant epoch 0 (NOT row.trading_date_ist_nanos)
    // so DEDUP collapses cross-day duplicates — mirrors lifecycle_designated_ts_nanos().
    buffer.at(TimestampNanos::new(index_constituency_designated_ts_nanos()))?;
    Ok(())
}

/// Bulk-ingest many `index_constituency` rows via QuestDB **ILP** (port 9009).
///
/// The whole [`Buffer`] is built on the async task (CPU-only, borrows `rows`),
/// then the blocking ILP connect + flush runs on `spawn_blocking`. Idempotent
/// via the table's DEDUP UPSERT KEYS — a re-run after a degraded boot is safe.
///
/// # Errors
/// Returns `Err` if the ILP `Sender` cannot connect or the flush fails; the
/// caller (Map-B boot wiring) logs + the next idempotent boot re-runs.
// TEST-EXEMPT: network I/O — pure builder `build_index_constituency_ilp_row` is unit-tested + boot integration exercises the flush.
pub async fn append_index_constituency_rows(
    questdb_config: &QuestDbConfig,
    rows: &[IndexConstituencyRow<'_>],
) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let conf = questdb_config.build_ilp_conf_string();
    let mut buffer = Buffer::new(ProtocolVersion::V1);
    for row in rows {
        build_index_constituency_ilp_row(&mut buffer, row)
            .context("building index_constituency ILP row")?;
    }
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut sender =
            Sender::from_conf(&conf).context("connect QuestDB ILP (index_constituency)")?;
        sender
            .flush(&mut buffer)
            .context("flush index_constituency rows via ILP")?;
        Ok(())
    })
    .await
    .context("index_constituency ILP flush task join")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_name_constant_is_stable() {
        assert_eq!(QUESTDB_TABLE_INDEX_CONSTITUENCY, "index_constituency");
    }

    #[test]
    fn test_dedup_key_includes_segment_and_index_and_designated_ts() {
        // I-P1-11: segment mandatory. + index_name (stock in many indices) + ts.
        assert!(DEDUP_KEY_INDEX_CONSTITUENCY.contains("exchange_segment"));
        assert!(DEDUP_KEY_INDEX_CONSTITUENCY.contains("index_name"));
        assert!(DEDUP_KEY_INDEX_CONSTITUENCY.contains("security_id"));
        assert!(
            DEDUP_KEY_INDEX_CONSTITUENCY.starts_with("ts"),
            "designated ts must be in the DEDUP key (QuestDB requirement)"
        );
        assert!(
            DEDUP_KEY_INDEX_CONSTITUENCY
                .split([',', ' '])
                .map(str::trim)
                .any(|t| t == "feed"),
            "operator override 2026-06-28: feed must be in the DEDUP key"
        );
        assert_eq!(
            DEDUP_KEY_INDEX_CONSTITUENCY.matches(',').count() + 1,
            5,
            "DEDUP key has exactly 5 columns (ts, index_name, security_id, exchange_segment, feed)"
        );
    }

    #[test]
    fn test_ddl_columns_constant_matches_schema() {
        // The 10 columns the DDL writes, pinned so the writer can't drift.
        assert_eq!(INDEX_CONSTITUENCY_COLUMNS.len(), 10);
        for col in [
            "ts",
            "index_name",
            "security_id",
            "exchange_segment",
            "symbol_name",
            "isin",
            "via_isin",
            "source",
            "dry_run",
            "feed",
        ] {
            assert!(
                INDEX_CONSTITUENCY_COLUMNS.contains(&col),
                "missing column {col}"
            );
        }
    }

    fn sample_row<'a>() -> IndexConstituencyRow<'a> {
        IndexConstituencyRow {
            trading_date_ist_nanos: 1_780_000_000_000_000_000,
            index_name: "Nifty Bank",
            security_id: 2885,
            exchange_segment: "NSE_EQ",
            symbol_name: "RELIANCE",
            isin: "INE002A01018",
            via_isin: true,
            source: "niftyindices",
            dry_run: false,
            feed: INDEX_CONSTITUENCY_FEED_DHAN,
        }
    }

    #[test]
    fn test_build_ilp_row_writes_feed_symbol() {
        // operator override 2026-06-28: every row carries feed in-key.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_index_constituency_ilp_row(&mut buffer, &sample_row()).expect("build");
        let line = String::from_utf8(buffer.as_bytes().to_vec()).expect("utf8");
        assert!(
            line.contains(",feed=dhan"),
            "ILP must write feed tag: {line}"
        );
    }

    #[test]
    fn test_build_ilp_row_symbols_before_fields_and_contains_values() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_index_constituency_ilp_row(&mut buffer, &sample_row()).expect("build");
        let line = String::from_utf8(buffer.as_bytes().to_vec()).expect("utf8");
        // table name first
        assert!(line.starts_with("index_constituency"), "got: {line}");
        // Values present (ILP escapes the space in "Nifty Bank" as "Nifty\ Bank").
        assert!(line.contains("index_name=Nifty\\ Bank"), "got: {line}");
        assert!(line.contains("exchange_segment=NSE_EQ"), "got: {line}");
        assert!(line.contains(",isin=INE002A01018"), "got: {line}");
        assert!(line.contains("security_id=2885i"), "got: {line}");
        assert!(line.contains("via_isin=t"), "got: {line}");
        // Symbols (tags) precede fields: the first field key appears AFTER the
        // last tag key. (Don't split on ' ' — ILP escapes spaces inside tags.)
        let tag_pos = line.find("index_name=").expect("tag present");
        let field_pos = line.find("security_id=").expect("field present");
        assert!(tag_pos < field_pos, "symbols must precede fields: {line}");
    }

    #[test]
    fn test_index_constituency_designated_ts_nanos_is_epoch_zero() {
        // ts-pin fix (2026-06-28): mirrors lifecycle_designated_ts_nanos() — the
        // designated ts is pinned to constant epoch 0 so DEDUP fires on the
        // business key alone and cross-day duplicates collapse.
        assert_eq!(index_constituency_designated_ts_nanos(), 0);
    }

    #[test]
    fn test_builder_stamps_constant_ts_regardless_of_trading_date() {
        // THE HEADLINE REGRESSION: two DIFFERENT trading_date_ist_nanos values
        // (June28 vs June29 IST-midnight) for the SAME (index, stock, feed) must
        // produce ILP rows that stamp the SAME designated ts (epoch 0) — proving
        // the day value no longer reaches the DEDUP key, so the table can no
        // longer accumulate 742 rows per day (1484 after two days).
        const JUNE28_IST_MIDNIGHT_NANOS: i64 = 1_782_950_400_000_000_000; // 2026-06-28 00:00 IST
        const JUNE29_IST_MIDNIGHT_NANOS: i64 = 1_783_036_800_000_000_000; // 2026-06-29 00:00 IST
        assert_ne!(
            JUNE28_IST_MIDNIGHT_NANOS, JUNE29_IST_MIDNIGHT_NANOS,
            "test setup: the two trading dates must differ"
        );

        let mut day28_row = sample_row();
        day28_row.trading_date_ist_nanos = JUNE28_IST_MIDNIGHT_NANOS;
        let mut day29_row = sample_row();
        day29_row.trading_date_ist_nanos = JUNE29_IST_MIDNIGHT_NANOS;

        let mut buf28 = Buffer::new(ProtocolVersion::V1);
        build_index_constituency_ilp_row(&mut buf28, &day28_row).expect("build day28");
        let mut buf29 = Buffer::new(ProtocolVersion::V1);
        build_index_constituency_ilp_row(&mut buf29, &day29_row).expect("build day29");

        let line28 = String::from_utf8(buf28.as_bytes().to_vec()).expect("utf8");
        let line29 = String::from_utf8(buf29.as_bytes().to_vec()).expect("utf8");

        // ILP V1 line format: `... <fields> <designated_ts_nanos>\n`. The
        // trailing nanos token is the designated ts — it MUST be `0` for both
        // days (same full DEDUP key → cross-day collapse), and identical to each
        // other regardless of the differing trading_date_ist_nanos input.
        let ts28 = line28.trim_end().rsplit(' ').next().expect("ts token 28");
        let ts29 = line29.trim_end().rsplit(' ').next().expect("ts token 29");
        assert_eq!(
            ts28, "0",
            "day28 designated ts must be epoch 0, got: {line28}"
        );
        assert_eq!(
            ts29, "0",
            "day29 designated ts must be epoch 0, got: {line29}"
        );
        assert_eq!(
            ts28, ts29,
            "different trading dates must stamp the SAME designated ts (cross-day collapse)"
        );
    }

    #[test]
    fn test_index_constituency_migration_should_run_only_when_marker_absent() {
        // Pure marker-gate predicate: absent → run; present → skip.
        let dir = std::env::temp_dir().join(format!(
            "tv_idxconst_mig_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mk tmp dir");
        let marker = dir.join("index_constituency_ts_pin_v1.done");

        // Absent → should run.
        assert!(
            index_constituency_migration_should_run(&marker),
            "migration must run when marker is absent"
        );

        // Present → should skip.
        std::fs::write(&marker, b"done").expect("write marker");
        assert!(
            !index_constituency_migration_should_run(&marker),
            "migration must skip when marker is present"
        );

        // Cleanup (best-effort).
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_marker_path_is_under_data_state() {
        // Mirrors the shadow_persistence marker convention.
        assert_eq!(
            INDEX_CONSTITUENCY_TS_PIN_MARKER_PATH,
            "data/state/index_constituency_ts_pin_v1.done"
        );
    }

    #[test]
    fn test_build_ilp_row_skips_empty_isin() {
        let mut row = sample_row();
        row.isin = ""; // symbol-fallback resolved → NULL, not empty symbol
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_index_constituency_ilp_row(&mut buffer, &row).expect("build");
        let line = String::from_utf8(buffer.as_bytes().to_vec()).expect("utf8");
        // The isin TAG appears as ",isin=" — must be absent when empty. (Note
        // the `via_isin=` FIELD contains the substring "isin=", so assert on the
        // comma-prefixed tag form, not bare "isin=".)
        assert!(
            !line.contains(",isin="),
            "empty isin must be skipped (NULL), got: {line}"
        );
        // The via_isin field still carries the resolution flag.
        assert!(line.contains("via_isin=t"), "got: {line}");
    }

    // ── FIX 13a: MigrationGate primitives (instance-scoped — no global state) ──

    #[tokio::test]
    async fn test_migration_gate_wait_returns_after_mark() {
        let gate = MigrationGate::new();
        gate.mark_complete();
        assert!(gate.wait(std::time::Duration::from_millis(10)).await);
    }

    #[tokio::test]
    async fn test_migration_gate_times_out_when_not_marked() {
        let gate = MigrationGate::new();
        // Short real timeout (50ms) — bounded, deterministic false.
        assert!(!gate.wait(std::time::Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn test_migration_gate_wakes_concurrent_waiter() {
        let gate = std::sync::Arc::new(MigrationGate::new());
        let waiter = {
            let gate = std::sync::Arc::clone(&gate);
            tokio::spawn(async move { gate.wait(std::time::Duration::from_secs(5)).await })
        };
        tokio::task::yield_now().await;
        gate.mark_complete();
        assert!(waiter.await.unwrap_or(false), "waiter must see the mark");
    }

    #[test]
    fn test_migration_gate_is_complete_flag() {
        let gate = MigrationGate::default();
        assert!(!gate.is_complete());
        gate.mark_complete();
        assert!(gate.is_complete());
        // Idempotent re-mark.
        gate.mark_complete();
        assert!(gate.is_complete());
    }

    #[test]
    fn test_migration_gate_static_accessor_is_stable() {
        let a = index_constituency_migration_gate() as *const MigrationGate;
        let b = index_constituency_migration_gate() as *const MigrationGate;
        assert_eq!(a, b, "accessor must return the same process-wide instance");
    }
}
