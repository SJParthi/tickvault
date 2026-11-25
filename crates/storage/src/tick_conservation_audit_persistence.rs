//! `tick_conservation_audit` — one row per daily tick conservation check
//! (plan item K4, 2026-10-26).
//!
//! ## Why it exists again
//!
//! The writer was deleted on 2026-07-18 because nothing wrote `ticks` then.
//! The Dhan live lane writes `ticks` again, and the question "did every row we
//! accepted reach the table?" had no daily answer. The TABLE was never dropped
//! (SEBI retention); this module writes new rows into it. Columns the retired
//! writer used stay in the table and read null on new rows.
//!
//! ## Table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS tick_conservation_audit (
//!     ts                TIMESTAMP,  -- the check's instant (IST nanos)
//!     trading_date_ist  TIMESTAMP,  -- IST midnight of the checked day
//!     feed              SYMBOL,     -- dhan / ...
//!     verdict           SYMBOL,     -- match / mismatch / unverified / partial
//!     reason            SYMBOL,     -- why (fixed set, see the app module)
//!     accepted_rows     LONG,       -- rows this process accepted for the day
//!     db_tick_rows      LONG,       -- rows QuestDB holds for the day, -1 unknown
//!     not_landed_rows   LONG,       -- rows counted dropped/spilled/deferred
//!     shortfall         LONG,       -- accepted minus db (negative = extra rows)
//!     count_reads       LONG,       -- database count attempts made
//!     process_start     TIMESTAMP   -- this process's boot instant (IST nanos)
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed);
//! ```
//!
//! DEDUP carries the designated `ts`, the day and `feed` (operator override
//! 2026-06-28: feed in every persisted key). Per feed and day, not per
//! instrument, so I-P1-11's `(security_id, exchange_segment)` pair is N/A.

use crate::audit_spill::{AuditSpill, AuditSpillTable};
use crate::off_worker::off_worker;
use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;

/// QuestDB table name.
pub const TICK_CONSERVATION_AUDIT_TABLE: &str = "tick_conservation_audit";

/// DEDUP UPSERT key — `ts` first, then the day and the feed.
pub const DEDUP_KEY_TICK_CONSERVATION_AUDIT: &str = "ts, trading_date_ist, feed";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// `(column, type)` for every column, in DDL order. Single source for the
/// `CREATE` and the per-column `ADD COLUMN IF NOT EXISTS` self-heal.
pub const TICK_CONSERVATION_AUDIT_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("verdict", "SYMBOL"),
    ("reason", "SYMBOL"),
    ("accepted_rows", "LONG"),
    ("db_tick_rows", "LONG"),
    ("not_landed_rows", "LONG"),
    ("shortfall", "LONG"),
    ("count_reads", "LONG"),
    ("process_start", "TIMESTAMP"),
];

/// One daily check, ready for ILP write. `Copy`; building one allocates
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickConservationRow {
    /// The check's instant (IST epoch nanos). The designated timestamp.
    pub checked_at_ist_nanos: i64,
    /// Any instant on the checked IST day (IST epoch nanos).
    pub day_ist_nanos: i64,
    /// Which feed.
    pub feed: Feed,
    /// `match` / `mismatch` / `unverified` / `partial`.
    pub verdict: &'static str,
    /// Why (fixed set, never free text).
    pub reason: &'static str,
    /// Rows this process accepted for the day.
    pub accepted_rows: i64,
    /// Rows QuestDB holds for the day, `-1` when the count was not read.
    pub db_tick_rows: i64,
    /// Rows counted as dropped, spilled or deferred to the WAL replay.
    pub not_landed_rows: i64,
    /// Count attempts made.
    pub count_reads: i64,
    /// This process's boot instant (IST epoch nanos).
    pub process_start_ist_nanos: i64,
}

impl TickConservationRow {
    /// IST midnight of the checked day.
    #[must_use]
    pub const fn trading_date_ist_nanos(&self) -> i64 {
        self.day_ist_nanos - self.day_ist_nanos.rem_euclid(NANOS_PER_DAY)
    }

    /// Accepted minus database rows; `0` when the count is unknown. Negative
    /// means the database holds rows this process did not accept.
    #[must_use]
    pub const fn shortfall(&self) -> i64 {
        if self.db_tick_rows < 0 {
            0
        } else {
            self.accepted_rows.saturating_sub(self.db_tick_rows)
        }
    }
}

/// The idempotent `CREATE TABLE` DDL. Pure.
#[must_use]
pub fn tick_conservation_audit_create_ddl() -> String {
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed 11-column list
    let cols = TICK_CONSERVATION_AUDIT_COLUMNS
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    // O(1) EXEMPT: end
    format!(
        "CREATE TABLE IF NOT EXISTS {TICK_CONSERVATION_AUDIT_TABLE} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({DEDUP_KEY_TICK_CONSERVATION_AUDIT});"
    )
}

/// `CREATE` → per-column `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`.
/// Never a drop. Pure. The ALTERs heal the table the retired writer created.
#[must_use]
pub fn tick_conservation_audit_ddl_statements() -> Vec<String> {
    let mut statements = vec![tick_conservation_audit_create_ddl()];
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed 11-column list
    for (col, ty) in TICK_CONSERVATION_AUDIT_COLUMNS {
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {TICK_CONSERVATION_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    // O(1) EXEMPT: end
    statements.push(format!(
        "ALTER TABLE {TICK_CONSERVATION_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_TICK_CONSERVATION_AUDIT});"
    ));
    statements
}

/// Confirms the current table identity, repairing a missing or legacy schema first.
/// The SQL catalog check and the later ILP POST are separate database operations.
/// Out-of-band table replacement must still be coordinated with the writer.
/// A failed ensure leaves the new receipt on disk until a later ensure succeeds.
pub async fn ensure_tick_conservation_audit_table(questdb_config: &QuestDbConfig) -> bool {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "tick_conservation_ensure_client_build",
                ?err,
                "tick_conservation_audit: HTTP client build failed — table not ensured"
            );
            return false;
        }
    };
    ensure_schema(&client, &base_url).await
}

/// Verifies the key on the same database that will receive this file.
pub(crate) async fn ensure_schema_before_replay(client: &reqwest::Client, write_url: &str) -> bool {
    let Ok(exec) = reqwest::Url::parse(write_url).and_then(|url| url.join("exec")) else {
        return false;
    };
    ensure_schema(client, exec.as_str()).await
}

// The catalog reflects applied WAL transactions. A prior DDL can be committed
// but temporarily invisible to both tables() and table_columns().
const CATALOG_APPLY_POLLS: usize = 12;
const CATALOG_INITIAL_WAIT_MS: u64 = 2;
const CATALOG_MAX_WAIT_MS: u64 = 32;
const CATALOG_RESPONSE_LIMIT: usize = 32 * 1024;

static CATALOG_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    // O(1) EXEMPT: build once from the fixed production receipt schema.
    let names = TICK_CONSERVATION_AUDIT_COLUMNS
        .iter()
        .map(|(name, _)| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(",");
    let limit = TICK_CONSERVATION_AUDIT_COLUMNS.len() + 1;
    // The extra row detects an unexpected key or designated column. Legacy
    // non-key columns are deliberately excluded because the retired writer used them.
    format!(
        r#"SELECT c."column", c.type, c.designated, c.upsertKey, t.dedup,
        t.walEnabled, t.partitionBy, t.designatedTimestamp, t.table_txn,
        w.writerTxn, w.sequencerTxn, w.suspended
        FROM table_columns('{TICK_CONSERVATION_AUDIT_TABLE}') c CROSS JOIN
        (SELECT table_name, dedup, walEnabled, partitionBy, designatedTimestamp, table_txn
         FROM tables() WHERE table_name='{TICK_CONSERVATION_AUDIT_TABLE}') t
        LEFT JOIN wal_tables() w ON t.table_name=w.name
        WHERE c.designated OR c.upsertKey OR c."column" IN ({names}) LIMIT {limit}"#
    )
});

#[derive(serde::Deserialize)]
struct CatalogColumn {
    name: String,
    kind: String,
    designated: bool,
    key: bool,
    dedup: bool,
    wal: bool,
    partition: String,
    timestamp: Option<String>,
    table_txn: Option<i64>,
    writer_txn: Option<i64>,
    sequencer_txn: Option<i64>,
    suspended: Option<bool>,
}

#[derive(PartialEq, Eq, Debug)]
enum SchemaState {
    Ready,
    NeedsRepair,
    Applying,
}

fn catalog_state(rows: &[CatalogColumn]) -> Result<SchemaState> {
    let first_txn = rows.first().and_then(|first| first.table_txn);
    for column in rows {
        if column.wal {
            anyhow::ensure!(
                column.suspended == Some(false),
                "audit WAL suspended or unconfirmed"
            );
            let writer = column
                .writer_txn
                .context("missing applied WAL transaction")?;
            let sequencer = column
                .sequencer_txn
                .context("missing sequencer transaction")?;
            // A newly created empty QuestDB 9.3.5 table has no table_txn yet.
            // Only an initial WAL position on *both* sides establishes that case.
            let table = column
                .table_txn
                .or_else(|| (writer == 0 && sequencer == 0).then_some(0));
            anyhow::ensure!(
                table.is_none_or(|txn| txn >= 0) && writer >= 0 && sequencer >= 0,
                "invalid WAL transaction"
            );
            if column.table_txn != first_txn || table != Some(writer) || writer != sequencer {
                return Ok(SchemaState::Applying);
            }
        }
    }
    let exact = rows.len() == TICK_CONSERVATION_AUDIT_COLUMNS.len()
        && TICK_CONSERVATION_AUDIT_COLUMNS.iter().all(|&(name, kind)| {
            rows.iter().any(|column| {
                column.name == name
                    && column.kind == kind
                    && column.designated == (name == "ts")
                    && column.key
                        == DEDUP_KEY_TICK_CONSERVATION_AUDIT
                            .split(", ")
                            .any(|key| key == name)
                    && column.wal
                    && column.dedup
                    && column.partition == "DAY"
                    && column.timestamp.as_deref() == Some("ts")
            })
        });
    Ok(if exact {
        SchemaState::Ready
    } else {
        SchemaState::NeedsRepair
    })
}

async fn read_schema(client: &reqwest::Client, base_url: &str) -> Result<SchemaState> {
    let mut response = client
        .get(base_url)
        .query(&[("query", CATALOG_SQL.as_str())])
        .send()
        .await?;
    let status = response.status();
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|n| n <= CATALOG_RESPONSE_LIMIT as u64),
        "oversize catalog reply"
    );
    let mut body = Vec::new();
    // A chunked response can omit Content-Length. Refuse excess bytes before copying them.
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            chunk.len() <= CATALOG_RESPONSE_LIMIT.saturating_sub(body.len()),
            "oversize catalog reply"
        );
        body.extend_from_slice(&chunk);
    }
    let value: serde_json::Value = serde_json::from_slice(&body)?;
    if status == reqwest::StatusCode::BAD_REQUEST
        && value["error"].as_str() == Some("table does not exist [table=tick_conservation_audit]")
    {
        return Ok(SchemaState::NeedsRepair);
    }
    anyhow::ensure!(
        status.is_success() && value.get("error").is_none(),
        "audit catalog request refused ({status})"
    );
    #[derive(serde::Deserialize)]
    struct Reply {
        dataset: Vec<CatalogColumn>,
    }
    let reply: Reply = serde_json::from_value(value)?;
    catalog_state(&reply.dataset)
}

async fn settled_schema(client: &reqwest::Client, base_url: &str) -> Result<SchemaState> {
    let mut wait_ms = CATALOG_INITIAL_WAIT_MS;
    for poll in 0..=CATALOG_APPLY_POLLS {
        let state = read_schema(client, base_url).await?;
        if state != SchemaState::Applying {
            return Ok(state);
        }
        if poll < CATALOG_APPLY_POLLS {
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            wait_ms = (wait_ms * 2).min(CATALOG_MAX_WAIT_MS);
        }
    }
    anyhow::bail!("audit WAL catalog has not caught up")
}

async fn ensure_schema(client: &reqwest::Client, base_url: &str) -> bool {
    let state = settled_schema(client, base_url).await;
    let verified = match state {
        Ok(SchemaState::Ready) => return true,
        Ok(SchemaState::NeedsRepair) if repair_schema(client, base_url).await => {
            settled_schema(client, base_url).await
        }
        state => state,
    };
    if matches!(verified, Ok(SchemaState::Ready)) {
        return true;
    }
    error!(
        code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
        stage = "tick_conservation_ensure_schema",
        ?verified,
        "QuestDB catalog did not confirm the daily audit receipt schema"
    );
    false
}

async fn repair_schema(client: &reqwest::Client, base_url: &str) -> bool {
    for ddl in &tick_conservation_audit_ddl_statements() {
        match client
            .get(base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.json::<serde_json::Value>().await.ok();
                if !body.as_ref().is_some_and(|reply| {
                    reply.get("error").is_none()
                        && reply.get("ddl").and_then(serde_json::Value::as_str) == Some("OK")
                }) {
                    error!(
                        code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                        stage = "tick_conservation_ensure_ddl",
                        ddl = ddl.as_str(),
                        "QuestDB did not acknowledge the audit schema statement"
                    );
                    return false;
                }
            }
            Ok(resp) => {
                let status = resp.status();
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "tick_conservation_ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    "tick_conservation_audit: DDL returned non-2xx"
                );
                return false;
            }
            Err(err) => {
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "tick_conservation_ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "tick_conservation_audit: DDL request failed"
                );
                return false;
            }
        }
    }
    true
}

/// Completion state for a single scheduled audit receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditReceiptWrite {
    /// The immediate QuestDB request was acknowledged.
    Acknowledged,
    /// The original row was published to the local audit replay directory.
    /// A process restart can drain it. A directory-sync failure is logged by
    /// the spool and leaves survival of a subsequent power loss uncertain.
    QueuedForReplay,
}

/// The journal and its drain share ownership while a file is in flight.
/// The daily writer acquires the lock before publication, and a replay checks
/// the file under this lock, so a healthy send and its drain cannot race.
pub(crate) static RECEIPT_DELIVERY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Journals one complete receipt before attempting its keyed database write.
///
/// # Errors
/// Returns an error if row encoding fails or neither the journal nor the
/// database acknowledges the receipt.
pub async fn write_or_queue_tick_conservation_row(
    config: &QuestDbConfig,
    row: &TickConservationRow,
) -> Result<AuditReceiptWrite> {
    let mut writer = TickConservationAuditWriter::new(config);
    writer.append_row(row)?;
    let spool = AuditSpill::for_table(AuditSpillTable::TickConservationAudit);
    writer.deliver_receipt(config, &spool).await
}

/// Lazy ILP-over-HTTP writer. An unreachable QuestDB still builds
/// (`sender = None`); `flush` returns `Err` until it is reachable.
pub struct TickConservationAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl TickConservationAuditWriter {
    /// Production constructor.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        crate::ilp_overflow::register_overflow_baseline(TICK_CONSERVATION_AUDIT_TABLE);
        let conf = format!(
            "http::addr={}:{};protocol_version=1;",
            config.host, config.http_port
        );
        match Sender::from_conf(conf) {
            Ok(s) => {
                let buffer = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer,
                    pending: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "tick_conservation_audit writer: QuestDB unreachable — buffering locally"
                );
                Self::for_test()
            }
        }
    }

    /// Disconnected writer with an empty buffer.
    #[must_use]
    // TEST-EXEMPT: test helper and the unreachable-QuestDB fallback of `new`; exercised by the append tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Appends one row. Cold path: once a day.
    ///
    /// # Errors
    /// Propagates ILP buffer errors.
    pub fn append_row(&mut self, r: &TickConservationRow) -> Result<()> {
        self.buffer
            .table(TICK_CONSERVATION_AUDIT_TABLE)
            .context("table")?
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("verdict", r.verdict)
            .context("verdict")?
            .symbol("reason", r.reason)
            .context("reason")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos()),
            )
            .context("trading_date_ist")?
            .column_i64("accepted_rows", r.accepted_rows)
            .context("accepted_rows")?
            .column_i64("db_tick_rows", r.db_tick_rows)
            .context("db_tick_rows")?
            .column_i64("not_landed_rows", r.not_landed_rows)
            .context("not_landed_rows")?
            .column_i64("shortfall", r.shortfall())
            .context("shortfall")?
            .column_i64("count_reads", r.count_reads)
            .context("count_reads")?
            .column_ts(
                "process_start",
                TimestampNanos::new(r.process_start_ist_nanos),
            )
            .context("process_start")?
            .at(TimestampNanos::new(r.checked_at_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    // One complete receipt goes into one immutable file with its original identity.
    async fn deliver_receipt(
        mut self,
        config: &QuestDbConfig,
        spool: &AuditSpill,
    ) -> Result<AuditReceiptWrite> {
        let _delivery = RECEIPT_DELIVERY.lock().await;
        let journal = off_worker(|| spool.spill(self.buffer.as_bytes(), self.pending));
        let result = if ensure_tick_conservation_audit_table(config).await {
            off_worker(|| self.flush())
        } else {
            Err(anyhow::anyhow!(
                "QuestDB has not confirmed the audit table's deduplication key"
            ))
        };

        match (result, journal) {
            (Ok(()), Ok(path)) => {
                if let Err(err) = tokio::fs::remove_file(&path).await {
                    warn!(code = ErrorCode::StorageGap03AuditWriteFailed.code_str(), ?err,
                        path = %path.display(), "QuestDB acknowledged the receipt but journal removal failed; a keyed replay is safe");
                }
                Ok(AuditReceiptWrite::Acknowledged)
            }
            (Ok(()), Err(err)) => {
                warn!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    ?err,
                    "QuestDB acknowledged the receipt after the local journal refused it"
                );
                Ok(AuditReceiptWrite::Acknowledged)
            }
            (Err(err), Ok(path)) => {
                metrics::counter!("tv_audit_spill_rows_total", "table" => TICK_CONSERVATION_AUDIT_TABLE).increment(1);
                tracing::debug!(?err, path = %path.display(), "receipt remains in its local journal for replay");
                Ok(AuditReceiptWrite::QueuedForReplay)
            }
            (Err(err), Err(disk)) => {
                metrics::counter!("tv_audit_spill_refused_rows_total", "table" => TICK_CONSERVATION_AUDIT_TABLE).increment(1);
                Err(err.context(format!("local receipt journal refused the row ({disk})")))
            }
        }
    }

    /// Flushes buffered rows over ILP-HTTP.
    ///
    /// # Errors
    /// `Err` when disconnected or the flush fails.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("tick_conservation_audit: no ILP sender (QuestDB unreachable)");
        };
        if let Err(err) = sender.flush(&mut self.buffer) {
            let dropped = crate::ilp_overflow::discard_if_overflowing(
                &mut self.buffer,
                &mut self.pending,
                TICK_CONSERVATION_AUDIT_TABLE,
            );
            return Err(anyhow::Error::new(err).context(
                crate::ilp_overflow::flush_failure_context(
                    "tick_conservation_audit ILP flush",
                    dropped,
                ),
            ));
        }
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    mod catalog_regression_tests;

    use super::*;

    const DAY_START: i64 = 1_792_972_800_000_000_000; // 2026-10-26 00:00 IST-epoch

    fn sample() -> TickConservationRow {
        TickConservationRow {
            checked_at_ist_nanos: DAY_START + 56_700 * 1_000_000_000,
            day_ist_nanos: DAY_START + 56_700 * 1_000_000_000,
            feed: Feed::Dhan,
            verdict: "match",
            reason: "reconciled",
            accepted_rows: 1_000,
            db_tick_rows: 998,
            not_landed_rows: 2,
            count_reads: 1,
            process_start_ist_nanos: DAY_START + 30_000 * 1_000_000_000,
        }
    }

    #[test]
    fn test_dedup_key_includes_ts_day_and_feed() {
        assert_eq!(
            DEDUP_KEY_TICK_CONSERVATION_AUDIT,
            "ts, trading_date_ist, feed"
        );
    }

    #[test]
    fn test_tick_conservation_audit_create_ddl_carries_dedup_with_feed_and_day_partition() {
        let ddl = tick_conservation_audit_create_ddl();
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS tick_conservation_audit ("));
        assert!(ddl.contains("DEDUP UPSERT KEYS(ts, trading_date_ist, feed)"));
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
        for (col, ty) in TICK_CONSERVATION_AUDIT_COLUMNS {
            assert!(ddl.contains(&format!("{col} {ty}")), "{col} missing");
        }
    }

    #[test]
    fn test_tick_conservation_audit_ddl_statements_self_heal_every_column_and_never_drop() {
        let stmts = tick_conservation_audit_ddl_statements();
        assert_eq!(
            stmts.len(),
            1 + (TICK_CONSERVATION_AUDIT_COLUMNS.len() - 1) + 1
        );
        for (col, ty) in TICK_CONSERVATION_AUDIT_COLUMNS.iter().skip(1) {
            let alter =
                format!("ALTER TABLE tick_conservation_audit ADD COLUMN IF NOT EXISTS {col} {ty};");
            assert!(stmts.contains(&alter), "missing self-heal for {col}");
        }
        assert!(stmts.last().is_some_and(|s| s.contains("DEDUP ENABLE UPSERT KEYS(ts, trading_date_ist, feed)")));
        assert!(
            stmts
                .iter()
                .all(|s| !s.to_ascii_uppercase().contains("DROP"))
        );
    }

    #[test]
    fn test_row_trading_date_and_shortfall() {
        let r = sample();
        assert_eq!(r.trading_date_ist_nanos(), DAY_START);
        assert_eq!(r.shortfall(), 2);
        let unknown = TickConservationRow {
            db_tick_rows: -1,
            ..r
        };
        assert_eq!(unknown.shortfall(), 0);
        let extra = TickConservationRow {
            db_tick_rows: 1_005,
            ..r
        };
        assert_eq!(extra.shortfall(), -5);
    }

    #[test]
    fn test_tick_conservation_append_row_writes_every_column() {
        let mut w = TickConservationAuditWriter::for_test();
        w.append_row(&sample()).expect("append");
        assert_eq!(w.pending(), 1);
        let line = String::from_utf8(w.buffer.as_bytes().to_vec()).unwrap_or_default();
        for needle in [
            "tick_conservation_audit,feed=dhan,verdict=match,reason=reconciled",
            "accepted_rows=1000i",
            "db_tick_rows=998i",
            "not_landed_rows=2i",
            "shortfall=2i",
            "count_reads=1i",
        ] {
            assert!(line.contains(needle), "{needle} missing in {line}");
        }
    }

    #[test]
    fn test_for_test_flush_and_pending_without_sender_keep_rows() {
        let mut w = TickConservationAuditWriter::for_test();
        assert!(w.flush().is_ok(), "nothing pending is a no-op");
        w.append_row(&sample()).expect("append");
        assert!(w.flush().is_err());
        assert_eq!(w.pending(), 1);
    }

    const CATALOG_BODY: &str = include_str!("../tests/fixtures/tick_conservation_catalog.json");
    const CATALOG_REPLY: &str = concat!(
        "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n",
        include_str!("../tests/fixtures/tick_conservation_catalog.json")
    );
    const MISSING_TABLE: &str = "HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n{\"error\":\"table does not exist [table=tick_conservation_audit]\"}";

    #[test]
    fn catalog_requires_every_receipt_column_and_exact_applied_wal_identity() {
        let data: serde_json::Value = serde_json::from_str(CATALOG_BODY).unwrap();
        let rows = &data["dataset"];
        let state = |v: serde_json::Value| {
            catalog_state(&serde_json::from_value::<Vec<CatalogColumn>>(v).unwrap())
        };
        assert_eq!(state(rows.clone()).unwrap(), SchemaState::Ready);
        for (index, field, value) in [
            (0, 2, serde_json::json!(false)),
            (3, 2, serde_json::json!(true)),
            (1, 3, serde_json::json!(false)),
            (4, 3, serde_json::json!(true)),
            (0, 4, serde_json::json!(false)),
            (0, 5, serde_json::json!(false)),
            (0, 6, serde_json::json!("MONTH")),
            (0, 7, serde_json::json!("another_ts")),
            (2, 1, serde_json::json!("STRING")),
            (9, 1, serde_json::json!("INT")),
            (10, 0, serde_json::json!("unexpected_extra")),
        ] {
            let mut changed = rows.clone();
            changed[index][field] = value;
            assert_eq!(state(changed).unwrap(), SchemaState::NeedsRepair);
        }
        let mut empty = rows.clone();
        for row in empty.as_array_mut().unwrap() {
            row[8] = serde_json::Value::Null;
            row[9] = serde_json::json!(0);
            row[10] = serde_json::json!(0);
        }
        assert_eq!(state(empty.clone()).unwrap(), SchemaState::Ready);
        empty[0][10] = serde_json::json!(1);
        assert_eq!(state(empty).unwrap(), SchemaState::Applying);
        let mut extra = rows.clone();
        extra.as_array_mut().unwrap().push(rows[0].clone());
        assert_eq!(state(extra).unwrap(), SchemaState::NeedsRepair);
        for index in [8, 9, 10] {
            let mut lagging = rows.clone();
            lagging[0][index] = serde_json::json!(42);
            assert_eq!(state(lagging).unwrap(), SchemaState::Applying);
        }
        for (field, value) in [
            (9, serde_json::Value::Null),
            (10, serde_json::json!(-1)),
            (11, serde_json::json!(true)),
        ] {
            let mut invalid = rows.clone();
            invalid[0][field] = value;
            assert!(state(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn oversized_chunked_catalog_never_confirms_a_receipt() {
        use crate::audit_spill::test_support::status_server;
        static RESPONSE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            let body = format!("{}{}", CATALOG_BODY, " ".repeat(CATALOG_RESPONSE_LIMIT));
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            )
        });
        let (url, server) = status_server(&RESPONSE, 1);
        let err = read_schema(&crate::http_client::build_probe_client(3).unwrap(), &url)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("oversize catalog reply"), "{err}");
        assert_eq!(server.join().unwrap(), 1);
    }

    fn schema_server(ready: bool) -> (QuestDbConfig, std::thread::JoinHandle<usize>) {
        use crate::audit_spill::test_support::status_server;
        let reply = if ready {
            CATALOG_REPLY
        } else {
            "HTTP/1.1 503 Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        };
        let (url, server) = status_server(reply, 1);
        (fixture_config(&url), server)
    }

    fn fixture_config(url: &str) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".into(),
            http_port: reqwest::Url::parse(url)
                .expect("fixture URL")
                .port()
                .expect("port"),
            pg_port: 1,
            ilp_port: 1,
        }
    }

    #[tokio::test]
    async fn test_write_or_queue_tick_conservation_row_retains_exact_bytes_for_both_uncertain_outcomes()
     {
        use crate::audit_spill::test_support::TestDir;

        for ready in [false, true] {
            let (config, server) = schema_server(ready);
            let dir = TestDir::new();
            let spool =
                AuditSpill::in_dir(AuditSpillTable::TickConservationAudit, dir.path().into());
            let mut writer = TickConservationAuditWriter::for_test();
            writer.append_row(&sample()).expect("append");
            let original = writer.buffer.as_bytes().to_vec();
            let result = writer
                .deliver_receipt(&config, &spool)
                .await
                .expect("enqueue receipt");
            assert_eq!(result, AuditReceiptWrite::QueuedForReplay);
            let files = crate::tick_spill_replay::list_spill_files(dir.path());
            assert_eq!(files.len(), 1);
            assert_eq!(
                std::fs::read(&files[0]).expect("published receipt"),
                original
            );
            server.join().expect("schema server");
        }
    }

    #[tokio::test]
    async fn test_refused_receipt_spill_returns_an_error() {
        use crate::audit_spill::test_support::TestDir;

        let (config, server) = schema_server(false);
        let dir = TestDir::new();
        let obstructed = dir.path().join("blocked");
        std::fs::write(&obstructed, b"keep").expect("obstruct spill directory");
        let spool = AuditSpill::in_dir(AuditSpillTable::TickConservationAudit, obstructed.clone());
        let mut writer = TickConservationAuditWriter::for_test();
        writer.append_row(&sample()).expect("append");
        assert!(writer.deliver_receipt(&config, &spool).await.is_err());
        assert_eq!(std::fs::read(obstructed).expect("original file"), b"keep");
        server.join().expect("schema server");
    }

    const DDL_ACK: &str = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"ddl\":\"OK\"}";

    fn reply_to_questdb_request(request: &str) -> &'static str {
        if request.starts_with("GET /exec?") {
            assert!(request.contains("table_columns"));
            CATALOG_REPLY
        } else {
            assert!(request.starts_with("POST /write?"));
            crate::audit_spill::test_support::OK_204
        }
    }

    #[tokio::test]
    async fn test_confirmed_receipt_with_and_without_a_writable_journal() {
        use crate::audit_spill::test_support::{TestDir, response_server};

        for disk_available in [false, true] {
            let dir = TestDir::new();
            let spool_dir = dir.path().join("journal");
            if !disk_available {
                std::fs::write(&spool_dir, b"blocked").expect("obstruct journal");
            }
            let n = 2; // one catalog query and one ILP POST
            let (url, server) = response_server(reply_to_questdb_request, n);
            let config = fixture_config(&url);
            let mut writer = TickConservationAuditWriter::new(&config);
            writer.append_row(&sample()).expect("append");
            let spool =
                AuditSpill::in_dir(AuditSpillTable::TickConservationAudit, spool_dir.clone());
            let result = writer
                .deliver_receipt(&config, &spool)
                .await
                .expect("direct write");
            assert_eq!(result, AuditReceiptWrite::Acknowledged);
            assert_eq!(server.join().expect("schema and write server"), n);
            if disk_available {
                assert_eq!(
                    std::fs::read_dir(spool_dir)
                        .expect("journal directory")
                        .count(),
                    0
                );
            } else {
                assert_eq!(std::fs::read(spool_dir).expect("obstruction"), b"blocked");
            }
        }
    }

    #[tokio::test]
    async fn test_ensure_schema_before_replay_requires_final_ddl_ack_and_retries_on_next_drain() {
        use crate::audit_spill::{
            drain_audit_spill_dir,
            test_support::{TestDir, response_server},
        };

        let dir = TestDir::new();
        let table = AuditSpillTable::TickConservationAudit;
        let spool = AuditSpill::in_dir(table, dir.path().into());
        let mut writer = TickConservationAuditWriter::for_test();
        writer.append_row(&sample()).expect("append");
        let original = writer.buffer.as_bytes().to_vec();
        let path = spool.spill(&original, 1).expect("publish");
        let client = crate::http_client::build_probe_client(3).expect("client");
        let ddls = tick_conservation_audit_ddl_statements().len();
        let (refused, server) = response_server(
            |request| {
                assert!(request.starts_with("GET /exec?"));
                if request.contains("table_columns") {
                    MISSING_TABLE
                } else if request.contains("DEDUP+ENABLE") {
                    "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"ddl\":\"OK\",\"error\":\"schema refused\"}"
                } else {
                    DDL_ACK
                }
            },
            ddls + 2,
        );
        let waiting = drain_audit_spill_dir(
            dir.path(),
            table,
            &format!("{refused}?precision=n"),
            &client,
        )
        .await;
        assert_eq!(server.join().expect("DDL server"), ddls + 1);
        assert!(waiting.failed);
        assert_eq!(waiting.files_replayed, 0);
        assert_eq!(std::fs::read(&path).expect("still waiting"), original);

        let (ready, server) = response_server(reply_to_questdb_request, 2);
        let landed =
            drain_audit_spill_dir(dir.path(), table, &format!("{ready}?precision=n"), &client)
                .await;
        assert_eq!(server.join().expect("catalog and ILP server"), 2);
        assert!(!landed.failed);
        assert_eq!(landed.rows_replayed, 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn repair_ddl_acknowledgement_needs_current_schema_readback() {
        use crate::audit_spill::test_support::response_server;
        // Empty/shadow catalog results remain unusable after all the DDLs returned OK.
        let ddl_count = tick_conservation_audit_ddl_statements().len();
        let (url, server) = response_server(
            |request| {
                if request.contains("table_columns") {
                    "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"dataset\":[]}"
                } else {
                    DDL_ACK
                }
            },
            ddl_count + 2,
        );
        assert!(!ensure_tick_conservation_audit_table(&fixture_config(&url)).await);
        assert_eq!(server.join().unwrap(), ddl_count + 2);
    }

    #[tokio::test]
    async fn test_ensure_tick_conservation_audit_table_returns_when_questdb_is_unreachable() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ensure_tick_conservation_audit_table(&cfg),
        )
        .await;
        assert!(
            matches!(done, Ok(false)),
            "an unreachable schema cannot be ready"
        );
    }
}
