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
//!   WHERE index_name = 'Nifty Bank' AND status = 'active';
//! -- which indices is RELIANCE (sid 2885) in today?
//! SELECT index_name FROM index_constituency
//!   WHERE security_id = 2885 AND status = 'active';
//! ```
//!
//! ## `status = 'active'` is MANDATORY on every membership query
//!
//! The designated `ts` is pinned to a constant (see below), so it carries NO
//! date information and `WHERE ts = today()` is meaningless here — the earlier
//! version of this doc showed exactly that query, which is why the filter is
//! spelled out now rather than left implied.
//!
//! Rows are UPSERTed in place and **never deleted** (SEBI point-in-time, §25),
//! so a stock DROPPED from an index at a rebalance keeps its row forever. Read
//! without the `status` filter and you get the historical UNION of everything
//! that has ever been in that index — which reads exactly like a correct
//! answer, just with too many stocks in it.
//!
//! Since 2026-08-11 that is detectable: every row carries `status`,
//! `last_seen_date` and `expired_date` (the `instrument_lifecycle` I-P1-08
//! pattern), and [`mark_missing_index_constituents_expired`] runs after each
//! successful bulk write to flip rows that today's lists did not mention. A
//! constituent that returns to the index auto-reactivates on the next write —
//! ILP UPSERT replaces the whole row, so `status` goes back to `active` and
//! `expired_date` returns to NULL.
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

/// `status` value for a constituent present in TODAY's constituent list.
///
/// Wire-format SYMBOL — operator SQL filters on this literal, so it is stable
/// across releases (the `instrument_lifecycle` I-P1-08 precedent).
pub const INDEX_CONSTITUENCY_STATUS_ACTIVE: &str = "active";

/// `status` value for a constituent that was in this table but is ABSENT from
/// today's list — i.e. dropped at an index rebalance.
///
/// The row is NEVER deleted (SEBI point-in-time retention, §25); it is marked.
pub const INDEX_CONSTITUENCY_STATUS_EXPIRED: &str = "expired";

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
    "status",
    "last_seen_date",
    "expired_date",
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

impl IndexConstituencyRow<'_> {
    /// The `last_seen_date` this row stamps: the IST trading-date midnight it
    /// was built for.
    ///
    /// Deliberately derived from `trading_date_ist_nanos` rather than added as
    /// a fourth caller-supplied field. That field was previously vestigial —
    /// retained only "for caller compatibility" after the ts-pin took its
    /// original job away — so reusing it both keeps every call site unchanged
    /// and gives it a real purpose again. Two sources for one date could drift;
    /// one cannot.
    #[must_use]
    pub const fn last_seen_date_nanos(&self) -> i64 {
        self.trading_date_ist_nanos
    }
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
            feed SYMBOL, \
            status SYMBOL, \
            last_seen_date TIMESTAMP, \
            expired_date TIMESTAMP\
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
        // Rebalance-lifecycle self-heal (2026-08-11). Additive + idempotent.
        // These three columns are deliberately NOT in the DEDUP key: they are
        // MUTABLE per-row state, and an UPSERT must be able to flip `status`
        // back to active in place. Putting `status` in the key would make an
        // expired row and its reactivated twin two SEPARATE rows — the exact
        // duplicate-accumulation the ts-pin was introduced to kill.
        format!(
            "ALTER TABLE {QUESTDB_TABLE_INDEX_CONSTITUENCY} \
             ADD COLUMN IF NOT EXISTS status SYMBOL;"
        ),
        format!(
            "ALTER TABLE {QUESTDB_TABLE_INDEX_CONSTITUENCY} \
             ADD COLUMN IF NOT EXISTS last_seen_date TIMESTAMP;"
        ),
        format!(
            "ALTER TABLE {QUESTDB_TABLE_INDEX_CONSTITUENCY} \
             ADD COLUMN IF NOT EXISTS expired_date TIMESTAMP;"
        ),
        // Legacy rows (written before this change) carry a NULL status, and a
        // NULL never matches `status = 'active'` — so without this backfill
        // every pre-existing row would silently vanish from the very queries
        // the new filter is meant to make correct. Seeding them `active` is
        // the non-destructive direction: today's build then flips whichever
        // ones are genuinely absent, so the table converges after ONE run
        // instead of losing history.
        format!(
            "UPDATE {QUESTDB_TABLE_INDEX_CONSTITUENCY} \
             SET status = '{INDEX_CONSTITUENCY_STATUS_ACTIVE}' WHERE status IS NULL;"
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
    /// F15 hardening (2026-07-05): in-process EXACTLY-ONCE latch for the
    /// TRUNCATE body. The marker FILE alone is not enough — its post-truncate
    /// write is best-effort, and the migration wrapper is re-invoked on every
    /// Dhan lane restart in the SAME process (runtime enable / cold-start
    /// retry). Without this latch a marker-write failure (or the documented
    /// operator delete-marker re-run procedure) lets a later in-process
    /// TRUNCATE wipe rows behind a permanently-green gate. `OnceCell` also
    /// SERIALIZES concurrent invocations (boot-prefix task racing the
    /// Dhan-lane call): the second caller waits for the single run instead of
    /// firing a second TRUNCATE.
    run_once: tokio::sync::OnceCell<()>,
}

impl MigrationGate {
    /// Fresh, un-marked gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            done: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
            run_once: tokio::sync::OnceCell::new(),
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
    /// timed out (the caller proceeds degrade-safe). Since the F14 hardening
    /// (2026-07-05) the migration runs process-globally in the boot prefix
    /// regardless of feed flags, so a timeout no longer means "no truncate can
    /// run this process" — it means the boot-prefix migration (including its
    /// quiet QuestDB readiness probe) has not completed yet; the caller's
    /// append is best-effort and re-persists next boot if the truncate lands
    /// after it. Registration-before-recheck ordering makes the
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
/// F15 hardening (2026-07-05): the wrapper is now EXACTLY-ONCE per process
/// via the gate's `run_once` latch — see
/// [`migrate_index_constituency_truncate_once_with_gate`].
// WIRING-EXEMPT: boot wiring lives in crates/app/src/index_constituency_boot.rs (boot-prefix task + before the normal write).
// TEST-EXEMPT: thin delegation to the gate-parameterized fn below, which is unit-tested (skip / run-once / concurrent-single-run).
pub async fn migrate_index_constituency_truncate_once(questdb_config: &QuestDbConfig) {
    let _ = migrate_index_constituency_truncate_once_with_gate(
        questdb_config,
        index_constituency_migration_gate(),
    )
    .await;
}

/// Outcome of one wrapper invocation — observable so the exactly-once
/// semantics are unit-testable with a LOCAL gate (never shared global state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsPinMigrationOutcome {
    /// This invocation executed the inner migration body (marker check +
    /// TRUNCATE attempt). At most ONE invocation per gate ever returns this.
    Ran,
    /// The inner body was skipped: the gate was already complete, or another
    /// invocation ran (or is running — this caller waited for it) the body.
    Skipped,
}

/// Gate-parameterized migration wrapper (F15 hardening, 2026-07-05).
///
/// Semantics:
/// * The inner TRUNCATE body executes AT MOST ONCE per gate (per process for
///   the global gate) — `run_once.get_or_init` both latches and SERIALIZES:
///   a concurrent second caller WAITS for the single run to finish instead of
///   racing a second TRUNCATE (which could land after another feed's append).
/// * The gate is marked complete after the once-cell resolves, on every path.
/// * A marker-write failure (or operator marker delete) can therefore no
///   longer re-TRUNCATE in the SAME process behind a green gate; the re-run
///   happens at the NEXT process boot (fresh gate + once-cell), where the
///   boot-prefix ordering again puts the truncate before the feed appends.
pub async fn migrate_index_constituency_truncate_once_with_gate(
    questdb_config: &QuestDbConfig,
    gate: &MigrationGate,
) -> TsPinMigrationOutcome {
    if gate.is_complete() {
        tracing::debug!(
            "index_constituency ts-pin migration already complete in-process — skipping"
        );
        return TsPinMigrationOutcome::Skipped;
    }
    let mut ran = false;
    gate.run_once
        .get_or_init(|| async {
            migrate_index_constituency_truncate_once_inner(questdb_config).await;
            ran = true;
        })
        .await;
    gate.mark_complete();
    if ran {
        TsPinMigrationOutcome::Ran
    } else {
        TsPinMigrationOutcome::Skipped
    }
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
    // Every row this writer emits came from TODAY's constituent list, so it is
    // active BY CONSTRUCTION — there is no code path that ILP-writes an expired
    // row. Expiry is only ever applied afterwards, by the UPDATE in
    // `mark_missing_index_constituents_expired`, to rows today did NOT touch.
    buffer.symbol("status", INDEX_CONSTITUENCY_STATUS_ACTIVE)?;
    // Optional SYMBOL — skip when empty (→ NULL, not empty ILP symbol).
    if !row.isin.is_empty() {
        buffer.symbol("isin", sanitize_ilp_symbol(row.isin).as_ref())?;
    }
    // ── Fields. ──
    buffer.column_i64("security_id", row.security_id)?;
    buffer.column_bool("via_isin", row.via_isin)?;
    buffer.column_bool("dry_run", row.dry_run)?;
    buffer.column_ts(
        "last_seen_date",
        TimestampNanos::new(row.last_seen_date_nanos()),
    )?;
    // `expired_date` is deliberately NOT written. QuestDB's DEDUP UPSERT
    // replaces the WHOLE row, so an omitted column lands NULL — which is
    // precisely the reactivation semantics we want: a constituent that returns
    // to the index gets a clean `active` row with no stale expiry date on it.
    // Writing a sentinel here would leave that date to be misread later.
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

/// Build the rebalance-expiry `UPDATE` for one `(feed, dry_run)` build.
///
/// Pure + unit-tested; [`mark_missing_index_constituents_expired`] is the thin
/// network wrapper around it.
///
/// ## Why the `last_seen_date IS NULL` arm is load-bearing
///
/// `NULL < x` evaluates to NULL, not true — so a plain `last_seen_date <
/// today` silently skips every row written before this column existed, and
/// those are exactly the rows most likely to be stale. Because this UPDATE runs
/// AFTER the bulk write, a legacy row that is STILL a constituent has just been
/// UPSERTed with today's date and no longer matches; only the genuinely absent
/// legacy rows keep their NULL. So the NULL arm expires precisely the right set,
/// and the table converges after one run instead of carrying pre-2026-08-11
/// rows as permanently active.
///
/// ## Why it is scoped by `feed` AND `dry_run`
///
/// Absence-from-today's-list is only evidence about the build that just ran.
/// An unscoped UPDATE would let a Dhan build expire Groww's rows (a feed that
/// simply did not run today looks identical to a feed whose constituents all
/// vanished), and would let a `--dry-run-universe` boot expire real production
/// rows — breaking the §27 dry-run isolation. Both are fail-closed here: a
/// build can only ever expire rows it is itself responsible for.
#[must_use]
pub fn build_index_constituency_expiry_sql(
    feed: &str,
    dry_run: bool,
    today_ist_nanos: i64,
) -> String {
    let today_micros = today_ist_nanos / 1_000;
    let feed_symbol = sanitize_ilp_symbol(feed);
    format!(
        "UPDATE {QUESTDB_TABLE_INDEX_CONSTITUENCY} \
         SET status = '{INDEX_CONSTITUENCY_STATUS_EXPIRED}', expired_date = {today_micros} \
         WHERE status = '{INDEX_CONSTITUENCY_STATUS_ACTIVE}' \
         AND feed = '{feed}' AND dry_run = {dry_run} \
         AND (last_seen_date IS NULL OR last_seen_date < {today_micros});",
        feed = feed_symbol.as_ref()
    )
}

/// Flip constituents that today's lists did NOT mention to `expired`.
///
/// Call this ONLY after a SUCCESSFUL [`append_index_constituency_rows`] for the
/// same build. Calling it after a partial or failed write would read that
/// failure as "these stocks left their index" and expire live constituents —
/// the caller therefore gates on the write's `Ok`, and the gating is what makes
/// this safe rather than anything inside this function.
///
/// Never deletes (SEBI point-in-time, §25) and never touches another feed's or
/// the other `dry_run` half's rows. Idempotent: a second run matches nothing,
/// because the first already moved those rows out of `status = 'active'`.
///
/// Logs rather than returning `Err` — the mapping itself is already durably
/// written at this point, and a failed expiry pass degrades to the PREVIOUS
/// behaviour (a stale-but-present row) rather than losing anything. It is
/// still logged at `error!` because a silently-skipped pass means membership
/// queries quietly over-report, which is the defect this whole change exists
/// to close.
// TEST-EXEMPT: network I/O — the SQL is built by the unit-tested pure `build_index_constituency_expiry_sql`.
pub async fn mark_missing_index_constituents_expired(
    questdb_config: &QuestDbConfig,
    feed: &str,
    dry_run: bool,
    today_ist_nanos: i64,
) {
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
                "index_constituency rebalance-expiry: HTTP client build failed — \
                 dropped constituents stay marked active, so membership queries \
                 will over-report until the next successful run"
            );
            return;
        }
    };
    let sql = build_index_constituency_expiry_sql(feed, dry_run, today_ist_nanos);
    match client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                feed, dry_run, "index_constituency rebalance-expiry pass applied"
            );
        }
        Ok(resp) => {
            error!(
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                status = %resp.status(),
                feed,
                dry_run,
                "index_constituency rebalance-expiry non-2xx — dropped constituents \
                 stay marked active; membership queries will over-report"
            );
        }
        Err(err) => {
            error!(
                ?err,
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                feed,
                dry_run,
                "index_constituency rebalance-expiry request failed — dropped \
                 constituents stay marked active; membership queries will over-report"
            );
        }
    }
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

    /// The NULL arm is the whole reason a legacy table converges. `NULL < x` is
    /// NULL in SQL, so without it every row written before `last_seen_date`
    /// existed would stay `active` forever — and those are precisely the rows
    /// most likely to be stale. Dropping this arm is a silent half-fix, which
    /// is why it gets its own test rather than riding along in a broader one.
    #[test]
    fn test_expiry_sql_matches_null_last_seen_date_not_just_older_dates() {
        let sql = build_index_constituency_expiry_sql(INDEX_CONSTITUENCY_FEED_DHAN, false, 0);
        assert!(
            sql.contains("last_seen_date IS NULL"),
            "legacy rows carry a NULL last_seen_date and NULL < x is NULL, so \
             omitting this arm leaves every pre-migration row permanently active: {sql}"
        );
        assert!(
            sql.contains("last_seen_date <"),
            "the ordinary stale-date arm must survive alongside the NULL arm: {sql}"
        );
    }

    /// Absence from today's list is evidence about THIS build only. Without the
    /// feed scope a Dhan-only run would expire every Groww row (a feed that did
    /// not run looks identical to one whose constituents all vanished); without
    /// the dry_run scope a `--dry-run-universe` boot would expire real rows,
    /// breaking §27 isolation.
    #[test]
    fn test_expiry_sql_is_scoped_by_feed_and_dry_run() {
        let sql = build_index_constituency_expiry_sql(INDEX_CONSTITUENCY_FEED_DHAN, false, 0);
        assert!(
            sql.contains("feed = 'dhan'"),
            "must not expire another feed's rows: {sql}"
        );
        assert!(
            sql.contains("dry_run = false"),
            "must not let a dry-run build expire production rows: {sql}"
        );

        let dry = build_index_constituency_expiry_sql(INDEX_CONSTITUENCY_FEED_DHAN, true, 0);
        assert!(
            dry.contains("dry_run = true"),
            "and symmetrically, a real build must not expire dry-run rows: {dry}"
        );
    }

    /// Only `active` rows are candidates. Re-running must be a no-op rather
    /// than re-stamping `expired_date` and destroying the original drop date —
    /// which is the one fact the SEBI point-in-time record needs from this row.
    #[test]
    fn test_expiry_sql_is_idempotent_by_targeting_active_rows_only() {
        let sql = build_index_constituency_expiry_sql(INDEX_CONSTITUENCY_FEED_DHAN, false, 0);
        assert!(
            sql.contains("WHERE status = 'active'"),
            "a second pass must match nothing, preserving the first expiry date: {sql}"
        );
        assert!(
            !sql.contains("DELETE"),
            "rows are marked, never deleted — SEBI point-in-time §25: {sql}"
        );
    }

    /// QuestDB SQL takes MICROseconds while the row API speaks nanoseconds. A
    /// 1000× error here would compare an IST midnight against a timestamp in
    /// 1970 and expire the entire table on the first run.
    #[test]
    fn test_expiry_sql_converts_nanos_to_micros() {
        let nanos = 1_780_000_000_000_000_000_i64;
        let sql = build_index_constituency_expiry_sql(INDEX_CONSTITUENCY_FEED_DHAN, false, nanos);
        let micros = nanos / 1_000;
        assert!(
            sql.contains(&micros.to_string()),
            "expected micros {micros} in: {sql}"
        );
        assert!(
            !sql.contains(&nanos.to_string()),
            "raw nanos must never reach the SQL — QuestDB would read it as a \
             far-future timestamp and match every row: {sql}"
        );
    }

    /// A row this writer emits is active by construction, and `last_seen_date`
    /// must carry the build's own trading date — the value the expiry pass
    /// compares against. If the writer stamped nothing, every row would look
    /// legacy-NULL and the next run would expire the entire universe.
    #[test]
    fn test_written_row_is_active_and_stamps_last_seen_date() {
        let row = sample_row();
        assert_eq!(
            row.last_seen_date_nanos(),
            row.trading_date_ist_nanos,
            "last_seen_date is derived from the build's trading date"
        );

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_index_constituency_ilp_row(&mut buffer, &row).expect("row builds");
        let wire = std::str::from_utf8(buffer.as_bytes()).expect("utf8 ILP");
        assert!(
            wire.contains("status=active"),
            "every emitted row is from today's list, so it is active: {wire}"
        );
        assert!(
            wire.contains("last_seen_date="),
            "the expiry pass compares against this column: {wire}"
        );
        assert!(
            !wire.contains("expired_date="),
            "expired_date must be OMITTED so an UPSERT clears it on reactivation \
             — writing a sentinel would leave a stale date to be misread: {wire}"
        );
    }

    #[test]
    fn test_ddl_columns_constant_matches_schema() {
        // The 13 columns the DDL writes, pinned so the writer can't drift.
        assert_eq!(INDEX_CONSTITUENCY_COLUMNS.len(), 13);
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
            "status",
            "last_seen_date",
            "expired_date",
        ] {
            assert!(
                INDEX_CONSTITUENCY_COLUMNS.contains(&col),
                "missing column {col}"
            );
        }
    }

    /// The three lifecycle columns are mutable per-row STATE. Putting `status`
    /// in the DEDUP key would make an expired row and its reactivated twin two
    /// separate rows — re-creating exactly the duplicate accumulation the
    /// constant-ts pin was introduced to eliminate.
    #[test]
    fn test_lifecycle_columns_are_not_in_the_dedup_key() {
        for col in ["status", "last_seen_date", "expired_date"] {
            assert!(
                !DEDUP_KEY_INDEX_CONSTITUENCY.contains(col),
                "{col} is mutable state and must stay OUT of the DEDUP key, so an \
                 UPSERT can flip it in place: {DEDUP_KEY_INDEX_CONSTITUENCY}"
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

    // ── F15 hardening (2026-07-05): exactly-once wrapper semantics ──
    // All tests use a LOCAL gate + an unreachable QuestDB config, so the inner
    // body's TRUNCATE attempt fails fast (connect refused on 127.0.0.1:1) and
    // never touches a live database or writes the marker file.

    fn unreachable_questdb() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    #[tokio::test]
    async fn test_with_gate_skips_when_gate_already_complete() {
        let gate = MigrationGate::new();
        gate.mark_complete();
        let outcome =
            migrate_index_constituency_truncate_once_with_gate(&unreachable_questdb(), &gate).await;
        assert_eq!(
            outcome,
            TsPinMigrationOutcome::Skipped,
            "a pre-complete gate must short-circuit without running the inner body"
        );
    }

    #[tokio::test]
    async fn test_with_gate_runs_once_then_skips() {
        let gate = MigrationGate::new();
        let cfg = unreachable_questdb();
        let first = migrate_index_constituency_truncate_once_with_gate(&cfg, &gate).await;
        assert_eq!(
            first,
            TsPinMigrationOutcome::Ran,
            "first call runs the body"
        );
        assert!(gate.is_complete(), "gate marked after the single run");
        // F15 pin: even though the inner run FAILED (unreachable host — marker
        // never written), a second in-process invocation must NOT re-run the
        // TRUNCATE body behind the now-green gate.
        let second = migrate_index_constituency_truncate_once_with_gate(&cfg, &gate).await;
        assert_eq!(
            second,
            TsPinMigrationOutcome::Skipped,
            "F15 regression: in-process re-invocation re-ran the TRUNCATE body"
        );
    }

    #[tokio::test]
    async fn test_with_gate_concurrent_callers_single_run() {
        let gate = std::sync::Arc::new(MigrationGate::new());
        let cfg = std::sync::Arc::new(unreachable_questdb());
        let spawn = |gate: std::sync::Arc<MigrationGate>, cfg: std::sync::Arc<QuestDbConfig>| {
            tokio::spawn(async move {
                migrate_index_constituency_truncate_once_with_gate(&cfg, &gate).await
            })
        };
        let a = spawn(std::sync::Arc::clone(&gate), std::sync::Arc::clone(&cfg));
        let b = spawn(std::sync::Arc::clone(&gate), std::sync::Arc::clone(&cfg));
        let (ra, rb) = (a.await, b.await);
        let outcomes = [
            ra.unwrap_or(TsPinMigrationOutcome::Skipped),
            rb.unwrap_or(TsPinMigrationOutcome::Skipped),
        ];
        let ran = outcomes
            .iter()
            .filter(|o| **o == TsPinMigrationOutcome::Ran)
            .count();
        assert_eq!(
            ran, 1,
            "exactly ONE concurrent caller may execute the TRUNCATE body \
             (OnceCell serializes; the other waits then skips), got {outcomes:?}"
        );
        assert!(gate.is_complete(), "gate marked after the serialized run");
    }

    // ── pub-fn ratchet heal (2026-07-05): name-matched tests for the three
    // F13/F14/F15 pub fns (the behavioral tests above predate them but their
    // names don't embed the fn names, so the pub-fn-test guard cannot see
    // them). Each test below adds a NEW assertion angle, not a duplicate. ──

    #[test]
    fn test_index_constituency_migration_gate_mark_visible_through_singleton() {
        // The static accessor must hand out ONE process-wide gate: a
        // mark_complete through one reference is observable through a second
        // accessor call (Release store / Acquire load through the singleton).
        // Marking the global gate here is safe: no other test asserts its
        // pre-mark state (the accessor-stability test checks pointers only).
        let first = index_constituency_migration_gate();
        first.mark_complete();
        assert!(
            index_constituency_migration_gate().is_complete(),
            "completion marked via one accessor call must be visible via another"
        );
    }

    #[tokio::test]
    async fn test_migration_gate_mark_complete_wakes_all_waiters() {
        // mark_complete uses notify_waiters, so EVERY registered waiter must
        // wake on a single mark — not just one (notify_one would deadlock the
        // second Groww-writer-class waiter).
        let gate = std::sync::Arc::new(MigrationGate::new());
        let spawn_waiter = |gate: std::sync::Arc<MigrationGate>| {
            tokio::spawn(async move { gate.wait(std::time::Duration::from_secs(5)).await })
        };
        let w1 = spawn_waiter(std::sync::Arc::clone(&gate));
        let w2 = spawn_waiter(std::sync::Arc::clone(&gate));
        let w3 = spawn_waiter(std::sync::Arc::clone(&gate));
        tokio::task::yield_now().await;
        gate.mark_complete();
        for (i, w) in [w1, w2, w3].into_iter().enumerate() {
            assert!(
                w.await.unwrap_or(false),
                "waiter {i} must be woken by a single mark_complete"
            );
        }
    }

    #[tokio::test]
    async fn test_migrate_index_constituency_truncate_once_with_gate_opens_gate_for_waiters() {
        // The FIX 13a contract: the wrapper opens the gate on EVERY exit path
        // — including an inner-body FAILURE (unreachable QuestDB here) — so a
        // waiting other-feed writer's wait() returns true immediately after,
        // instead of burning its full bounded timeout.
        let gate = MigrationGate::new();
        let outcome =
            migrate_index_constituency_truncate_once_with_gate(&unreachable_questdb(), &gate).await;
        assert_eq!(
            outcome,
            TsPinMigrationOutcome::Ran,
            "fresh gate runs the body"
        );
        assert!(
            gate.wait(std::time::Duration::from_millis(1)).await,
            "gate must be open for waiters immediately after the wrapper returns, \
             even when the inner TRUNCATE failed"
        );
    }
}
