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

/// Ensures the table (idempotent, self-healing). Failures are logged with a
/// code and never block the caller; a failed ensure leaves the first ILP
/// write to create the table without DEDUP until a later ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested below); the unreachable path is tested.
pub async fn ensure_tick_conservation_audit_table(questdb_config: &QuestDbConfig) {
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
            return;
        }
    };
    for ddl in &tick_conservation_audit_ddl_statements() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "tick_conservation_ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    "tick_conservation_audit: DDL returned non-2xx"
                );
            }
            Err(err) => error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "tick_conservation_ensure_ddl",
                ?err,
                ddl = ddl.as_str(),
                "tick_conservation_audit: DDL request failed"
            ),
        }
    }
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
        let conf = format!("http::addr={}:{};", config.host, config.http_port);
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
        assert!(done.is_ok(), "the ensure must return, not hang");
    }
}
