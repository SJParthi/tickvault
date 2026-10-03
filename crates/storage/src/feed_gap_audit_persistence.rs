//! Durable record of every live-feed reconnect GAP (`feed_gap_audit`).
//!
//! ## Why this table exists
//!
//! `ws_event_audit` records each socket lifecycle EVENT as its own row: a
//! `disconnected` row, later a `connected` row. The gap between them — how
//! long one socket carried nothing, and why — is the fact an operator needs
//! when asking "were we blind between 10:02 and 10:04 on connection 3?", and
//! until 2026-10-02 it had to be rebuilt by pairing rows by hand. The rows also
//! carried `dhan_code = -1`, `down_secs = 0` and `attempts = 0` for every live
//! feed socket, whatever had happened.
//!
//! This table is the paired form: **one row per gap**, written when a
//! `connected` follows a `disconnected` for the same connection slot. The
//! row's designated timestamp is the gap START, so a time-range query finds
//! every gap that began in the range.
//!
//! A gap still open at shutdown (a socket parked for the session, or one that
//! never came back before the process stopped) is written with `gap_end` = the
//! shutdown instant and `open_at_shutdown = true`, so "the socket was still
//! down when we stopped" is a row, never a missing row.
//!
//! ## Table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS feed_gap_audit (
//!     ts                TIMESTAMP,  -- gap start (IST nanos)
//!     gap_end           TIMESTAMP,  -- reconnect, or the shutdown instant
//!     trading_date_ist  TIMESTAMP,  -- IST midnight of the gap start
//!     feed              SYMBOL,     -- dhan / ...
//!     ws_type           SYMBOL,     -- main_feed / depth_20 / depth_200
//!     connection_index  LONG,       -- the socket's global slot
//!     reason            SYMBOL,     -- the close cause slug
//!     dhan_code         LONG,       -- vendor disconnect code, -1 = none
//!     instruments_held  LONG,       -- instruments the socket held at the close
//!     down_secs         LONG,       -- gap_end minus ts, whole seconds
//!     attempts          LONG,       -- failed dials inside the gap
//!     open_at_shutdown  BOOLEAN     -- true = closed by shutdown, not a reconnect
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, ws_type, connection_index);
//! ```
//!
//! The DEDUP key carries the designated timestamp first, `feed` (operator
//! override 2026-06-28: feed in every persisted key) and the
//! `(ws_type, connection_index)` composite connection identity. One socket
//! cannot start two gaps at the same nanosecond, so a replayed row UPSERTs in
//! place instead of duplicating. Per connection, not per instrument, so
//! I-P1-11's `(security_id, exchange_segment)` pair is N/A here.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::ws_event_types::WsType;

/// QuestDB table name. One row per reconnect gap.
pub const FEED_GAP_AUDIT_TABLE: &str = "feed_gap_audit";

/// DEDUP UPSERT key — designated `ts` (the gap start) first, then the day,
/// the feed, and the `(ws_type, connection_index)` connection identity.
pub const DEDUP_KEY_FEED_GAP_AUDIT: &str = "ts, trading_date_ist, feed, ws_type, connection_index";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_DAY: i64 = 86_400 * NANOS_PER_SEC;

/// One reconnect gap, ready for ILP write. Every field is `Copy`, so building
/// one allocates nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedGapRow {
    /// When the socket was lost (IST epoch nanos). The designated timestamp.
    pub gap_start_ist_nanos: i64,
    /// When it was back (subscribe acked), or the shutdown instant.
    pub gap_end_ist_nanos: i64,
    /// Which feed.
    pub feed: Feed,
    /// Which pool.
    pub ws_type: WsType,
    /// The socket's global slot.
    pub connection_index: i64,
    /// The close cause slug (fixed set, never vendor text).
    pub reason: &'static str,
    /// The vendor disconnect code that closed the socket, `-1` when none.
    pub dhan_code: i64,
    /// Instruments the socket held when it was lost.
    pub instruments_held: i64,
    /// Failed dials inside the gap.
    pub attempts: i64,
    /// `true` when shutdown, not a reconnect, closed the gap.
    pub open_at_shutdown: bool,
}

impl FeedGapRow {
    /// IST midnight of the gap start: the day the gap belongs to.
    #[must_use]
    pub const fn trading_date_ist_nanos(&self) -> i64 {
        self.gap_start_ist_nanos - self.gap_start_ist_nanos.rem_euclid(NANOS_PER_DAY)
    }

    /// The gap's length in whole seconds, never negative (a clock step
    /// backwards between the two stamps reads as zero, not as a negative gap).
    #[must_use]
    pub const fn down_secs(&self) -> i64 {
        let nanos = self
            .gap_end_ist_nanos
            .saturating_sub(self.gap_start_ist_nanos);
        if nanos <= 0 { 0 } else { nanos / NANOS_PER_SEC }
    }
}

/// `(column, type)` for every column, in DDL order.
///
/// SINGLE SOURCE OF TRUTH: both the `CREATE TABLE` statement and the
/// per-column `ADD COLUMN IF NOT EXISTS` self-heal are generated from this
/// one list, so a column added here reaches a fresh table AND one that
/// already exists.
pub const FEED_GAP_AUDIT_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("gap_end", "TIMESTAMP"),
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("ws_type", "SYMBOL"),
    ("connection_index", "LONG"),
    ("reason", "SYMBOL"),
    ("dhan_code", "LONG"),
    ("instruments_held", "LONG"),
    ("down_secs", "LONG"),
    ("attempts", "LONG"),
    ("open_at_shutdown", "BOOLEAN"),
];

/// The idempotent `CREATE TABLE` DDL, generated from
/// [`FEED_GAP_AUDIT_COLUMNS`]. Pure (testable without QuestDB).
#[must_use]
pub fn feed_gap_audit_create_ddl() -> String {
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed 12-column list
    let cols = FEED_GAP_AUDIT_COLUMNS
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    // O(1) EXEMPT: end
    format!(
        "CREATE TABLE IF NOT EXISTS {FEED_GAP_AUDIT_TABLE} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({DEDUP_KEY_FEED_GAP_AUDIT});"
    )
}

/// The full ordered DDL list: `CREATE` → per-column
/// `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`. Never a drop. Pure.
///
/// The per-column ALTERs make the schema self-healing: a table created by an
/// older build, or auto-created by a first ILP write without DEDUP, gains
/// every missing column and the dedup key on the next boot.
#[must_use]
pub fn feed_gap_audit_ddl_statements() -> Vec<String> {
    let mut statements = vec![feed_gap_audit_create_ddl()];
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed 12-column list
    for (col, ty) in FEED_GAP_AUDIT_COLUMNS {
        // The designated timestamp cannot be added after the fact; it exists
        // by construction on any table this code created.
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {FEED_GAP_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    // O(1) EXEMPT: end
    statements.push(format!(
        "ALTER TABLE {FEED_GAP_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_FEED_GAP_AUDIT});"
    ));
    statements
}

/// Create the gap table if absent (idempotent, schema self-heal), at boot,
/// the same way `ws_event_audit` is ensured.
///
/// Failures log at `error!` but never block the caller: the paired events
/// still exist in `ws_event_audit`. A failed ensure leaves the table to be
/// auto-created by the first ILP write WITHOUT DEDUP UPSERT KEYS — a
/// duplicate-row window until a later boot's ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via feed_gap_audit_ddl_statements tests)
pub async fn ensure_feed_gap_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                stage = "feed_gap_ensure_client_build",
                ?err,
                "feed_gap_audit: HTTP client build failed — table not ensured (the first \
                 ILP write may auto-create it WITHOUT dedup until the next boot)"
            );
            return;
        }
    };
    for ddl in &feed_gap_audit_ddl_statements() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                error!(
                    code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                    stage = "feed_gap_ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "feed_gap_audit: DDL returned non-2xx (dedup may be missing — \
                     duplicate-row window until a later ensure succeeds)"
                );
            }
            Err(err) => error!(
                code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                stage = "feed_gap_ensure_ddl",
                ?err,
                ddl = ddl.as_str(),
                "feed_gap_audit: DDL request failed"
            ),
        }
    }
}

/// ILP-over-HTTP conf. HTTP (not TCP) so every flush gets a server ACK and a
/// server-side reject surfaces as `Err` instead of a silently empty table.
fn feed_gap_audit_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!("http::addr={}:{};", config.host, config.http_port)
}

/// Lazy ILP-over-HTTP writer for `feed_gap_audit`. An unreachable QuestDB at
/// construction still builds (`sender = None`); `append_row` fills the local
/// buffer and `flush` returns `Err` until QuestDB is reachable.
pub struct FeedGapAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl FeedGapAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        // Seeded so an absent discard series can never read as a healthy zero.
        crate::ilp_overflow::register_overflow_baseline(FEED_GAP_AUDIT_TABLE);
        match Sender::from_conf(feed_gap_audit_ilp_http_conf(config)) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "feed_gap_audit writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one gap row. Cold path: one row per reconnect.
    ///
    /// # Errors
    /// Propagates ILP buffer errors.
    pub fn append_row(&mut self, r: &FeedGapRow) -> Result<()> {
        self.buffer
            .table(FEED_GAP_AUDIT_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("ws_type", r.ws_type.as_str())
            .context("ws_type")?
            .symbol("reason", r.reason)
            .context("reason")?
            .column_ts("gap_end", TimestampNanos::new(r.gap_end_ist_nanos))
            .context("gap_end")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos()),
            )
            .context("trading_date_ist")?
            .column_i64("connection_index", r.connection_index)
            .context("connection_index")?
            .column_i64("dhan_code", r.dhan_code)
            .context("dhan_code")?
            .column_i64("instruments_held", r.instruments_held)
            .context("instruments_held")?
            .column_i64("down_secs", r.down_secs())
            .context("down_secs")?
            .column_i64("attempts", r.attempts)
            .context("attempts")?
            .column_bool("open_at_shutdown", r.open_at_shutdown)
            .context("open_at_shutdown")?
            .at(TimestampNanos::new(r.gap_start_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP.
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (rows stay buffered,
    /// bounded by `ilp_overflow::discard_if_overflowing`).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("feed_gap_audit: no ILP sender (QuestDB unreachable)");
        };
        if let Err(err) = sender.flush(&mut self.buffer) {
            let dropped = crate::ilp_overflow::discard_if_overflowing(
                &mut self.buffer,
                &mut self.pending,
                FEED_GAP_AUDIT_TABLE,
            );
            return Err(anyhow::Error::new(err).context(
                crate::ilp_overflow::flush_failure_context("feed_gap_audit ILP flush", dropped),
            ));
        }
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_START: i64 = 1_769_990_400_000_000_000;

    fn sample() -> FeedGapRow {
        FeedGapRow {
            gap_start_ist_nanos: DAY_START + 37_800 * NANOS_PER_SEC,
            gap_end_ist_nanos: DAY_START + 37_842 * NANOS_PER_SEC + 900_000_000,
            feed: Feed::Dhan,
            ws_type: WsType::MainFeed,
            connection_index: 3,
            reason: "transport_error",
            dhan_code: 800,
            instruments_held: 4_990,
            attempts: 2,
            open_at_shutdown: false,
        }
    }

    /// The boot ensure never blocks or panics the writer task when QuestDB is
    /// down: every DDL fails fast, is logged with a code, and the fn returns
    /// so the writer still buffers rows.
    #[tokio::test]
    async fn test_ensure_feed_gap_audit_table_returns_when_questdb_is_unreachable() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ensure_feed_gap_audit_table(&cfg),
        )
        .await;
        assert!(done.is_ok(), "the ensure must return, not hang");
        let mut w = FeedGapAuditWriter::for_test();
        w.append_row(&sample())
            .expect("append after a failed ensure");
        assert_eq!(w.pending(), 1);
    }

    #[test]
    fn test_feed_gap_row_down_secs_and_trading_date() {
        let r = sample();
        assert_eq!(r.down_secs(), 42, "whole seconds, truncated");
        assert_eq!(r.trading_date_ist_nanos(), DAY_START);
        let mut back = r;
        back.gap_end_ist_nanos = r.gap_start_ist_nanos - 5;
        assert_eq!(
            back.down_secs(),
            0,
            "a backwards clock never reads negative"
        );
    }

    #[test]
    fn test_dedup_key_feed_gap_audit_is_ts_first_with_feed_and_connection() {
        assert_eq!(
            DEDUP_KEY_FEED_GAP_AUDIT,
            "ts, trading_date_ist, feed, ws_type, connection_index"
        );
        assert!(DEDUP_KEY_FEED_GAP_AUDIT.starts_with("ts,"));
        let ddl = feed_gap_audit_create_ddl();
        assert!(
            ddl.contains(
                "DEDUP UPSERT KEYS(ts, trading_date_ist, feed, ws_type, connection_index)"
            )
        );
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
    }

    #[test]
    fn test_feed_gap_audit_create_ddl_carries_every_column() {
        let ddl = feed_gap_audit_create_ddl();
        for (col, ty) in FEED_GAP_AUDIT_COLUMNS {
            assert!(ddl.contains(&format!("{col} {ty}")), "missing {col} {ty}");
        }
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS feed_gap_audit ("));
    }

    #[test]
    fn test_feed_gap_audit_ddl_statements_self_heal_every_column_and_never_drop() {
        let stmts = feed_gap_audit_ddl_statements();
        assert_eq!(stmts.len(), 1 + (FEED_GAP_AUDIT_COLUMNS.len() - 1) + 1);
        for (col, ty) in FEED_GAP_AUDIT_COLUMNS.iter().filter(|(c, _)| *c != "ts") {
            let want = format!("ALTER TABLE feed_gap_audit ADD COLUMN IF NOT EXISTS {col} {ty};");
            assert!(stmts.contains(&want), "missing self-heal for {col}");
        }
        assert_eq!(
            stmts.last().map(String::as_str),
            Some(
                "ALTER TABLE feed_gap_audit DEDUP ENABLE UPSERT KEYS(ts, trading_date_ist, \
                 feed, ws_type, connection_index);"
            )
        );
        for s in &stmts {
            assert!(
                !s.to_ascii_uppercase().contains("DROP"),
                "never a drop: {s}"
            );
        }
    }

    #[test]
    fn test_append_row_writes_every_column_and_the_gap_start_as_ts() {
        let mut w = FeedGapAuditWriter::for_test();
        let r = sample();
        w.append_row(&r).expect("append");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(
            line.starts_with("feed_gap_audit,feed=dhan,ws_type=main_feed,reason=transport_error ")
        );
        for part in [
            "connection_index=3i",
            "dhan_code=800i",
            "instruments_held=4990i",
            "down_secs=42i",
            "attempts=2i",
            "open_at_shutdown=f",
        ] {
            assert!(line.contains(part), "missing {part} in {line}");
        }
        assert!(
            line.trim_end()
                .ends_with(&r.gap_start_ist_nanos.to_string()),
            "the designated timestamp is the gap START"
        );
    }

    #[test]
    fn test_flush_without_a_sender_errors_and_keeps_rows() {
        let mut w = FeedGapAuditWriter::for_test();
        assert!(w.flush().is_ok(), "nothing pending = nothing to flush");
        w.append_row(&sample()).expect("append");
        assert!(w.flush().is_err());
        assert_eq!(w.pending(), 1, "rows stay buffered on a failed flush");
    }
}
