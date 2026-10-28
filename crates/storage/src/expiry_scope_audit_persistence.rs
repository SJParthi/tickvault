//! `expiry_scope_audit` — one row per daily expiry-scope check (owner
//! request 2026-10-28: "go ahead and add the daily expiry check").
//!
//! ## What the row proves
//!
//! After the close, the app reads today's distinct option contracts from
//! `candles_1m` (feed `dhan`, segments `NSE_FNO` / `BSE_FNO`), works out each
//! contract's expiry (from its stored name, or from today's contract file
//! when the name is missing or unreadable) and compares it with the nearest
//! real expiry of its underlying in today's contract file. The row counts
//! what it found; `verdict` is `clean`, `violation` or `partial` (a read
//! failure or a missing contract file is `partial`, never `clean`).
//!
//! ## Table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS expiry_scope_audit (
//!     ts                  TIMESTAMP,  -- the check's instant (IST nanos)
//!     trading_date_ist    TIMESTAMP,  -- IST midnight of the checked day
//!     feed                SYMBOL,     -- dhan / ...
//!     verdict             SYMBOL,     -- clean / violation / partial
//!     reason              SYMBOL,     -- why (fixed set, see the app module)
//!     rows_read           LONG,       -- distinct (id, segment, name) rows read, -1 unknown
//!     contracts_checked   LONG,       -- rows whose expiry was judged
//!     off_expiry          LONG,       -- not the underlying's nearest real expiry
//!     expired             LONG,       -- expiry before the checked day
//!     unlabelled          LONG,       -- rows with no stored contract name
//!     unknown_underlying  LONG,       -- underlying not in today's contract file
//!     malformed_labels    LONG,       -- a stored name that does not parse
//!     label_mismatch      LONG,       -- name disagrees with the contract file
//!     unresolved          LONG,       -- no usable name and not in the file
//!     subscribe_refused   LONG,       -- contracts the attach refused today, -1 unknown
//!     sample_offender     STRING      -- first off-expiry or expired name, '' none
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed);
//! ```
//!
//! DEDUP carries the designated `ts`, the day and `feed` (operator override
//! 2026-06-28: feed in every persisted key). One row per feed and day, not
//! per instrument, so I-P1-11's `(security_id, exchange_segment)` pair is
//! N/A for the key; the check itself keys every contract on that pair.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;

/// QuestDB table name.
pub const EXPIRY_SCOPE_AUDIT_TABLE: &str = "expiry_scope_audit";

/// DEDUP UPSERT key — `ts` first, then the day and the feed.
pub const DEDUP_KEY_EXPIRY_SCOPE_AUDIT: &str = "ts, trading_date_ist, feed";

/// Longest `sample_offender` stored, in bytes. A contract name is capped at
/// 64 bytes by the app's label renderer; this leaves headroom and bounds the
/// row whatever the input.
pub const EXPIRY_SCOPE_SAMPLE_MAX_BYTES: usize = 96;

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// `(column, type)` for every column, in DDL order. Single source for the
/// `CREATE` and the per-column `ADD COLUMN IF NOT EXISTS` self-heal.
pub const EXPIRY_SCOPE_AUDIT_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("verdict", "SYMBOL"),
    ("reason", "SYMBOL"),
    ("rows_read", "LONG"),
    ("contracts_checked", "LONG"),
    ("off_expiry", "LONG"),
    ("expired", "LONG"),
    ("unlabelled", "LONG"),
    ("unknown_underlying", "LONG"),
    ("malformed_labels", "LONG"),
    ("label_mismatch", "LONG"),
    ("unresolved", "LONG"),
    ("subscribe_refused", "LONG"),
    ("sample_offender", "STRING"),
];

/// One daily check, ready for ILP write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiryScopeRow {
    /// The check's instant (IST epoch nanos). The designated timestamp.
    pub checked_at_ist_nanos: i64,
    /// Any instant on the checked IST day (IST epoch nanos).
    pub day_ist_nanos: i64,
    /// Which feed.
    pub feed: Feed,
    /// `clean` / `violation` / `partial`.
    pub verdict: &'static str,
    /// Why (fixed set, never free text).
    pub reason: &'static str,
    /// Distinct rows read from `candles_1m`; `-1` when the read failed.
    pub rows_read: i64,
    /// Rows whose expiry was judged.
    pub contracts_checked: i64,
    /// Rows on a live expiry that is not the underlying's nearest real one.
    pub off_expiry: i64,
    /// Rows whose expiry is before the checked day.
    pub expired: i64,
    /// Rows with no stored contract name.
    pub unlabelled: i64,
    /// Rows whose underlying has no real expiry in today's contract file.
    pub unknown_underlying: i64,
    /// Rows whose stored name does not parse.
    pub malformed_labels: i64,
    /// Rows whose stored name disagrees with today's contract file.
    pub label_mismatch: i64,
    /// Rows with no usable name that today's contract file does not list.
    pub unresolved: i64,
    /// Contracts the subscribe-time check refused today; `-1` unknown.
    pub subscribe_refused: i64,
    /// First offending name (off-expiry or expired); empty when none.
    pub sample_offender: String,
}

impl ExpiryScopeRow {
    /// IST midnight of the checked day.
    #[must_use]
    pub const fn trading_date_ist_nanos(&self) -> i64 {
        self.day_ist_nanos - self.day_ist_nanos.rem_euclid(NANOS_PER_DAY)
    }

    /// `sample_offender` cut to [`EXPIRY_SCOPE_SAMPLE_MAX_BYTES`] on a char
    /// boundary. Pure.
    #[must_use]
    pub fn bounded_sample(&self) -> &str {
        let s = self.sample_offender.as_str();
        if s.len() <= EXPIRY_SCOPE_SAMPLE_MAX_BYTES {
            return s;
        }
        let mut cut = EXPIRY_SCOPE_SAMPLE_MAX_BYTES;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        &s[..cut]
    }
}

/// The idempotent `CREATE TABLE` DDL. Pure.
#[must_use]
pub fn expiry_scope_audit_create_ddl() -> String {
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed 16-column list
    let cols = EXPIRY_SCOPE_AUDIT_COLUMNS
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    // O(1) EXEMPT: end
    format!(
        "CREATE TABLE IF NOT EXISTS {EXPIRY_SCOPE_AUDIT_TABLE} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({DEDUP_KEY_EXPIRY_SCOPE_AUDIT});"
    )
}

/// `CREATE` → per-column `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`.
/// Never a drop. Pure.
#[must_use]
pub fn expiry_scope_audit_ddl_statements() -> Vec<String> {
    let mut statements = vec![expiry_scope_audit_create_ddl()];
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed 16-column list
    for (col, ty) in EXPIRY_SCOPE_AUDIT_COLUMNS {
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {EXPIRY_SCOPE_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    // O(1) EXEMPT: end
    statements.push(format!(
        "ALTER TABLE {EXPIRY_SCOPE_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_EXPIRY_SCOPE_AUDIT});"
    ));
    statements
}

/// Ensures the table (idempotent, self-healing). Failures are logged with a
/// code and never block the caller.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested below); the unreachable path is tested.
pub async fn ensure_expiry_scope_audit_table(questdb_config: &QuestDbConfig) {
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
                stage = "expiry_scope_ensure_client_build",
                ?err,
                "expiry_scope_audit: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    for ddl in &expiry_scope_audit_ddl_statements() {
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
                    stage = "expiry_scope_ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    "expiry_scope_audit: DDL returned non-2xx"
                );
            }
            Err(err) => error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "expiry_scope_ensure_ddl",
                ?err,
                ddl = ddl.as_str(),
                "expiry_scope_audit: DDL request failed"
            ),
        }
    }
}

/// Lazy ILP-over-HTTP writer. An unreachable QuestDB still builds
/// (`sender = None`); `flush` returns `Err` until it is reachable.
pub struct ExpiryScopeAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl ExpiryScopeAuditWriter {
    /// Production constructor.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        crate::ilp_overflow::register_overflow_baseline(EXPIRY_SCOPE_AUDIT_TABLE);
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
                // The HTTP sender connects lazily, so this is a config error
                // (the address), not an unreachable QuestDB. Nothing is held:
                // the flush that follows returns `Err` and the caller logs the
                // row as lost (coded `error!`).
                warn!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    ?err,
                    "expiry_scope_audit: no ILP sender for this run (address config refused); \
                     the flush that follows reports the row"
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
    pub fn append_row(&mut self, r: &ExpiryScopeRow) -> Result<()> {
        self.buffer
            .table(EXPIRY_SCOPE_AUDIT_TABLE)
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
            .column_i64("rows_read", r.rows_read)
            .context("rows_read")?
            .column_i64("contracts_checked", r.contracts_checked)
            .context("contracts_checked")?
            .column_i64("off_expiry", r.off_expiry)
            .context("off_expiry")?
            .column_i64("expired", r.expired)
            .context("expired")?
            .column_i64("unlabelled", r.unlabelled)
            .context("unlabelled")?
            .column_i64("unknown_underlying", r.unknown_underlying)
            .context("unknown_underlying")?
            .column_i64("malformed_labels", r.malformed_labels)
            .context("malformed_labels")?
            .column_i64("label_mismatch", r.label_mismatch)
            .context("label_mismatch")?
            .column_i64("unresolved", r.unresolved)
            .context("unresolved")?
            .column_i64("subscribe_refused", r.subscribe_refused)
            .context("subscribe_refused")?
            .column_str("sample_offender", r.bounded_sample())
            .context("sample_offender")?
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
            anyhow::bail!("expiry_scope_audit: no ILP sender (QuestDB unreachable)");
        };
        if let Err(err) = sender.flush(&mut self.buffer) {
            let dropped = crate::ilp_overflow::discard_if_overflowing(
                &mut self.buffer,
                &mut self.pending,
                EXPIRY_SCOPE_AUDIT_TABLE,
            );
            return Err(anyhow::Error::new(err).context(
                crate::ilp_overflow::flush_failure_context("expiry_scope_audit ILP flush", dropped),
            ));
        }
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_START: i64 = 1_793_145_600_000_000_000; // 2026-10-28 00:00 IST-epoch

    fn sample() -> ExpiryScopeRow {
        ExpiryScopeRow {
            checked_at_ist_nanos: DAY_START + 57_000 * 1_000_000_000,
            day_ist_nanos: DAY_START + 57_000 * 1_000_000_000,
            feed: Feed::Dhan,
            verdict: "violation",
            reason: "off_expiry",
            rows_read: 25_000,
            contracts_checked: 24_990,
            off_expiry: 3,
            expired: 1,
            unlabelled: 1,
            unknown_underlying: 2,
            malformed_labels: 0,
            label_mismatch: 0,
            unresolved: 0,
            subscribe_refused: 0,
            sample_offender: "SBIN-29Dec2026-1040-PE".to_string(),
        }
    }

    #[test]
    fn test_dedup_key_expiry_scope_audit_includes_ts_day_and_feed() {
        assert_eq!(DEDUP_KEY_EXPIRY_SCOPE_AUDIT, "ts, trading_date_ist, feed");
        assert!(DEDUP_KEY_EXPIRY_SCOPE_AUDIT.contains("feed"));
    }

    #[test]
    fn test_expiry_scope_audit_create_ddl_carries_dedup_with_feed_and_day_partition() {
        let ddl = expiry_scope_audit_create_ddl();
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS expiry_scope_audit ("));
        assert!(ddl.contains("DEDUP UPSERT KEYS(ts, trading_date_ist, feed)"));
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
        for (col, ty) in EXPIRY_SCOPE_AUDIT_COLUMNS {
            assert!(ddl.contains(&format!("{col} {ty}")), "{col} missing");
        }
    }

    #[test]
    fn test_expiry_scope_audit_ddl_statements_self_heal_every_column_and_never_drop() {
        let stmts = expiry_scope_audit_ddl_statements();
        assert_eq!(stmts.len(), 1 + (EXPIRY_SCOPE_AUDIT_COLUMNS.len() - 1) + 1);
        for (col, ty) in EXPIRY_SCOPE_AUDIT_COLUMNS.iter().skip(1) {
            let alter =
                format!("ALTER TABLE expiry_scope_audit ADD COLUMN IF NOT EXISTS {col} {ty};");
            assert!(stmts.contains(&alter), "missing self-heal for {col}");
        }
        assert!(stmts.last().is_some_and(|s| {
            s.contains("DEDUP ENABLE UPSERT KEYS(ts, trading_date_ist, feed)")
        }));
        assert!(
            stmts
                .iter()
                .all(|s| !s.to_ascii_uppercase().contains("DROP"))
        );
    }

    #[test]
    fn test_expiry_scope_row_trading_date_is_ist_midnight() {
        assert_eq!(sample().trading_date_ist_nanos(), DAY_START);
    }

    #[test]
    fn test_bounded_sample_cuts_on_a_char_boundary() {
        let mut r = sample();
        assert_eq!(r.bounded_sample(), "SBIN-29Dec2026-1040-PE");
        r.sample_offender = "é".repeat(EXPIRY_SCOPE_SAMPLE_MAX_BYTES);
        let cut = r.bounded_sample();
        assert!(cut.len() <= EXPIRY_SCOPE_SAMPLE_MAX_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
        r.sample_offender = String::new();
        assert_eq!(r.bounded_sample(), "");
    }

    #[test]
    fn test_expiry_scope_append_row_writes_every_column() {
        let mut w = ExpiryScopeAuditWriter::for_test();
        w.append_row(&sample()).expect("append");
        assert_eq!(w.pending(), 1);
        let line = String::from_utf8(w.buffer.as_bytes().to_vec()).unwrap_or_default();
        for needle in [
            "expiry_scope_audit,feed=dhan,verdict=violation,reason=off_expiry",
            "rows_read=25000i",
            "contracts_checked=24990i",
            "off_expiry=3i",
            "expired=1i",
            "unlabelled=1i",
            "unknown_underlying=2i",
            "malformed_labels=0i",
            "label_mismatch=0i",
            "unresolved=0i",
            "subscribe_refused=0i",
            "sample_offender=\"SBIN-29Dec2026-1040-PE\"",
        ] {
            assert!(line.contains(needle), "{needle} missing in {line}");
        }
    }

    #[test]
    fn test_expiry_scope_flush_without_sender_keeps_rows() {
        let mut w = ExpiryScopeAuditWriter::for_test();
        assert!(w.flush().is_ok(), "nothing pending is a no-op");
        w.append_row(&sample()).expect("append");
        assert!(w.flush().is_err());
        assert_eq!(w.pending(), 1);
    }

    #[tokio::test]
    async fn test_ensure_expiry_scope_audit_table_returns_when_questdb_is_unreachable() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ensure_expiry_scope_audit_table(&cfg),
        )
        .await;
        assert!(done.is_ok(), "the ensure must return, not hang");
    }
}
