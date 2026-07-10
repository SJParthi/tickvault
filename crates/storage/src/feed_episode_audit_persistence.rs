//! Dual-feed scoreboard **episode** audit table (`feed_episode_audit`,
//! SCOREBOARD-01 family — operator directive 2026-07-10: *"ensure and
//! CAPTURE that the issue really arose from the broker side"*).
//!
//! One row per feed EPISODE — disconnect / off-hours disconnect / stall
//! restart / never-streamed restart / boot-reconciled process death — with
//! the blame verdict PERSISTED (`blame` = broker / ours / indeterminate +
//! a machine reason slug). The 15:45 IST daily aggregation and the boot-time
//! process-death reconciler are the only writers; classification happens in
//! ONE place (`tickvault_common::feed_blame::classify_episode`), and the
//! append signature takes [`BlameClass`] BY VALUE — persisting an episode
//! without a blame class is a COMPILE error (the no-blank-blame ratchet).
//!
//! ## Audit table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS feed_episode_audit (
//!     ts                TIMESTAMP,  -- episode event ts (IST nanos, designated)
//!     trading_date_ist  TIMESTAMP,  -- the trading day (IST midnight)
//!     feed              SYMBOL,     -- dhan / groww
//!     ws_type           SYMBOL,     -- main_feed / order_update / groww_bridge
//!     connection_index  LONG,       -- 0..pool_size-1
//!     episode_kind      SYMBOL,     -- disconnect / off_hours_disconnect /
//!                                   -- stall_restart / never_streamed_restart /
//!                                   -- process_death
//!     blame             SYMBOL,     -- broker / ours / indeterminate (NEVER blank)
//!     blame_reason      SYMBOL,     -- machine slug (rate_limit_805 / bare_rst / ...)
//!     source            SYMBOL,     -- copied ws_event_audit source label
//!     dhan_code         LONG,       -- 805/807/...; -1 sentinel = none
//!     detector          SYMBOL,     -- ws_event_audit / stall_row / boot_reconciled
//!     down_secs         LONG,       -- downtime attributed to the episode (0 unknown)
//!     market_hours      BOOLEAN,    -- inside [09:00,15:30) IST
//!     evidence          STRING,     -- ≤200 chars, secret-redacted
//!     run_partial       BOOLEAN     -- the classifying run had partial evidence
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, ws_type, connection_index, episode_kind);
//! ```
//!
//! The DEDUP key mirrors `DEDUP_KEY_WS_EVENT_AUDIT` (`ts` first per the
//! 2026-04-28 regression rule; `feed` in-key per the 2026-06-28 operator
//! override; `(ws_type, connection_index)` = the I-P1-11 composite-uniqueness
//! discipline extended to WS streams) so RE-AGGREGATION IS IDEMPOTENT: the
//! daily task re-classifying the same audit rows, or a repeated boot
//! reconciliation stamping the same deterministic episode ts, UPSERTs in
//! place instead of duplicating. The table is per-connection-episode, not
//! per-instrument, so I-P1-11's `(security_id, exchange_segment)` pair is N/A.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::feed_blame::BlameClass;
use tickvault_common::sanitize::capture_rest_error_body;

/// QuestDB table name. One row per feed episode.
pub const FEED_EPISODE_AUDIT_TABLE: &str = "feed_episode_audit";

/// DEDUP UPSERT key. Designated timestamp first (2026-04-28 regression rule);
/// `feed` in-key (operator override 2026-06-28); `(ws_type,
/// connection_index)` per the I-P1-11 composite discipline for WS streams;
/// `episode_kind` distinguishes the kinds. Mirrors `DEDUP_KEY_WS_EVENT_AUDIT`
/// so re-aggregation is idempotent.
pub const DEDUP_KEY_FEED_EPISODE_AUDIT: &str =
    "ts, trading_date_ist, feed, ws_type, connection_index, episode_kind";

/// Bound on the persisted `evidence` string (post-redaction truncation).
pub const EPISODE_EVIDENCE_MAX_CHARS: usize = 200;

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// One classified feed episode, ready for ILP write.
///
/// `blame` is a [`BlameClass`] BY VALUE — the no-blank-blame ratchet layer 2:
/// there is no way to construct a row without a verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedEpisodeAuditRow {
    /// Episode event timestamp — IST nanoseconds (designated timestamp).
    /// Deterministic per episode (the audit row's own ts, or the boot's
    /// first post-boot connected-row ts for process deaths) so re-runs
    /// UPSERT in place.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Feed wire label (`"dhan"` / `"groww"`) as read from the source row.
    pub feed: String,
    /// WS-type wire label (`"main_feed"` / `"order_update"` / `"groww_bridge"`).
    pub ws_type: String,
    /// 0-based connection index within its ws_type pool.
    pub connection_index: i64,
    /// Episode kind wire label (see `tickvault_common::feed_blame::EPISODE_KIND_*`).
    pub episode_kind: &'static str,
    /// The blame verdict (compile-enforced non-blank).
    pub blame: BlameClass,
    /// Machine reason slug from the classifier (`rate_limit_805` / ...).
    pub blame_reason: &'static str,
    /// The source label copied from the originating audit row.
    pub source: String,
    /// Dhan disconnect code; `-1` sentinel = none.
    pub dhan_code: i64,
    /// Which detector produced the episode (`ws_event_audit` /
    /// `stall_row` / `boot_reconciled`).
    pub detector: &'static str,
    /// Downtime attributed to the episode in seconds (0 = unknown).
    pub down_secs: i64,
    /// `true` when the episode happened inside [09:00, 15:30) IST.
    pub market_hours: bool,
    /// Free-form evidence note. Redacted + truncated at the write boundary.
    pub evidence: String,
    /// `true` when the classifying run had PARTIAL evidence (e.g. the
    /// errors.jsonl correlation window had aged out on a backfill day).
    pub run_partial: bool,
}

/// The idempotent `CREATE TABLE` DDL. Pure (testable without QuestDB).
#[must_use]
pub fn feed_episode_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {FEED_EPISODE_AUDIT_TABLE} (\
            ts                TIMESTAMP, \
            trading_date_ist  TIMESTAMP, \
            feed              SYMBOL, \
            ws_type           SYMBOL, \
            connection_index  LONG, \
            episode_kind      SYMBOL, \
            blame             SYMBOL, \
            blame_reason      SYMBOL, \
            source            SYMBOL, \
            dhan_code         LONG, \
            detector          SYMBOL, \
            down_secs         LONG, \
            market_hours      BOOLEAN, \
            evidence          STRING, \
            run_partial       BOOLEAN\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_FEED_EPISODE_AUDIT});"
    )
}

/// Create the episode table if absent (idempotent, schema-self-heal
/// pattern). Failures log at `error!` but never block the caller — the
/// classified episodes still reach the log sinks.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested via feed_episode_audit_create_ddl tests)
pub async fn ensure_feed_episode_audit_table(questdb_config: &QuestDbConfig) {
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
                ?err,
                "feed_episode_audit: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    // CREATE first, then the feed self-heal triple (operator override
    // 2026-06-28: feed in-key on every persisted table). Greenfield tables
    // get the column + key from the CREATE; the triple is a no-op there and
    // heals any table created by an earlier build. Backfill precedes the
    // DEDUP-ENABLE so a re-keyed table upserts over a legacy NULL-feed row.
    // Never drops the table (SEBI retention).
    let statements = [
        feed_episode_audit_create_ddl(),
        format!("ALTER TABLE {FEED_EPISODE_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS feed SYMBOL;"),
        format!("UPDATE {FEED_EPISODE_AUDIT_TABLE} SET feed = 'dhan' WHERE feed IS NULL;"),
        format!(
            "ALTER TABLE {FEED_EPISODE_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_FEED_EPISODE_AUDIT});"
        ),
    ];
    for ddl in &statements {
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
                error!(%status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "feed_episode_audit: DDL returned non-2xx");
            }
            Err(err) => error!(
                ?err,
                ddl = ddl.as_str(),
                "feed_episode_audit: DDL request failed"
            ),
        }
    }
}

/// Builds the ILP-over-HTTP conf string. HTTP (NOT ILP TCP) so every flush
/// gets a per-request server ACK — a server-side reject (schema drift, DEDUP
/// violation) surfaces as `Err` instead of a silently empty table (the
/// 2026-07-05 ws_event_audit lesson).
fn episode_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!("http::addr={}:{};", config.host, config.http_port)
}

/// Lazy ILP-over-HTTP writer for `feed_episode_audit`. Mirrors
/// `WsEventAuditWriter`: unreachable QuestDB at construction still builds
/// (`sender = None`); `append_row` fills the local buffer and `flush`
/// returns `Err` until QuestDB is reachable.
pub struct FeedEpisodeAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl FeedEpisodeAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure
    /// (`http::` does not dial at construction; failures surface at flush).
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = episode_ilp_http_conf(config);
        match Sender::from_conf(&conf) {
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
                    "feed_episode_audit writer: QuestDB unreachable — buffering locally"
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
    // TEST-EXEMPT: test-only helper used by append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Test-only view of the ILP buffer bytes (shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one classified episode row (cold path — episodes are rare).
    ///
    /// SECURITY: `evidence` is routed through the `capture_rest_error_body`
    /// redaction choke point (control-char strip, URL-param / JWT-shape /
    /// credential-field redaction) then truncated to
    /// [`EPISODE_EVIDENCE_MAX_CHARS`] — a token can never reach the table
    /// even if a producer forgets.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &FeedEpisodeAuditRow) -> Result<()> {
        let safe_evidence: String = capture_rest_error_body(&r.evidence)
            .chars()
            .take(EPISODE_EVIDENCE_MAX_CHARS)
            .collect();
        self.buffer
            .table(FEED_EPISODE_AUDIT_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("ws_type", r.ws_type.as_str())
            .context("ws_type")?
            .symbol("episode_kind", r.episode_kind)
            .context("episode_kind")?
            .symbol("blame", r.blame.as_str())
            .context("blame")?
            .symbol("blame_reason", r.blame_reason)
            .context("blame_reason")?
            .symbol("source", r.source.as_str())
            .context("source")?
            .symbol("detector", r.detector)
            .context("detector")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("connection_index", r.connection_index)
            .context("connection_index")?
            .column_i64("dhan_code", r.dhan_code)
            .context("dhan_code")?
            .column_i64("down_secs", r.down_secs)
            .context("down_secs")?
            .column_bool("market_hours", r.market_hours)
            .context("market_hours")?
            .column_str("evidence", safe_evidence.as_str())
            .context("evidence")?
            .column_bool("run_partial", r.run_partial)
            .context("run_partial")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK — a
    /// server-side reject surfaces as `Err`, never a silently empty table).
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (rows stay buffered).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("feed_episode_audit: no ILP sender (QuestDB unreachable)");
        };
        sender
            .flush(&mut self.buffer)
            .context("feed_episode_audit ILP flush")?;
        self.pending = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::feed_blame::EPISODE_KIND_DISCONNECT;

    fn sample_row() -> FeedEpisodeAuditRow {
        FeedEpisodeAuditRow {
            ts_ist_nanos: 1_770_000_000_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            feed: "dhan".to_string(),
            ws_type: "main_feed".to_string(),
            connection_index: 0,
            episode_kind: EPISODE_KIND_DISCONNECT,
            blame: BlameClass::Broker,
            blame_reason: "rate_limit_805",
            source: "Dhan (another login)".to_string(),
            dhan_code: 805,
            detector: "ws_event_audit",
            down_secs: 12,
            market_hours: true,
            evidence: "code=805 source=Dhan (another login)".to_string(),
            run_partial: false,
        }
    }

    #[test]
    fn test_feed_episode_audit_ddl_contains_expected_columns() {
        let ddl = feed_episode_audit_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "feed",
            "ws_type",
            "connection_index",
            "episode_kind",
            "blame",
            "blame_reason",
            "source",
            "dhan_code",
            "detector",
            "down_secs",
            "market_hours",
            "evidence",
            "run_partial",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        // DDL-guard leg of the no-blank-blame ratchet: `blame` is a SYMBOL
        // column in the CREATE (a rename/retype fails here first).
        assert!(
            ddl.contains("blame             SYMBOL"),
            "blame must be a SYMBOL column: {ddl}"
        );
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_FEED_EPISODE_AUDIT})"
        )));
    }

    #[test]
    fn test_feed_episode_dedup_key_ts_first_feed_and_connection_composite() {
        // ts first (2026-04-28 regression rule).
        assert!(
            DEDUP_KEY_FEED_EPISODE_AUDIT.trim_start().starts_with("ts,"),
            "designated timestamp must lead the DEDUP key"
        );
        // feed in-key (operator override 2026-06-28) — whole-token match.
        let has_token = |t: &str| {
            DEDUP_KEY_FEED_EPISODE_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        assert!(has_token("feed"), "feed must be in the DEDUP key");
        // I-P1-11 composite discipline extended to WS streams.
        assert!(has_token("ws_type"));
        assert!(has_token("connection_index"));
        assert!(has_token("episode_kind"));
        assert!(has_token("trading_date_ist"));
        // Exactly the 6 columns mirroring DEDUP_KEY_WS_EVENT_AUDIT.
        assert_eq!(DEDUP_KEY_FEED_EPISODE_AUDIT.matches(',').count() + 1, 6);
    }

    #[test]
    fn test_episode_append_row_writes_blame_and_feed_symbols() {
        // No-blank-blame ratchet layer 2 proof: the ILP line carries a
        // non-empty blame + reason tag; the append signature already made a
        // blame-less row unconstructible.
        let mut w = FeedEpisodeAuditWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.contains(",feed=dhan"), "feed tag missing: {line}");
        assert!(line.contains(",blame=broker"), "blame tag missing: {line}");
        assert!(
            line.contains(",blame_reason=rate_limit_805"),
            "blame_reason tag missing: {line}"
        );
        assert!(
            line.contains(",episode_kind=disconnect"),
            "episode_kind tag missing: {line}"
        );
        assert!(
            line.contains(",detector=ws_event_audit"),
            "detector tag missing: {line}"
        );
    }

    #[test]
    fn test_episode_evidence_is_redacted_and_bounded() {
        // A JWT-shaped credential in the evidence must never reach the
        // table, and evidence is truncated to the 200-char bound.
        let mut r = sample_row();
        let jwt = format!(
            "eyJ{}.eyJ{}.sig{}",
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(200)
        );
        r.evidence = format!("reject body token={jwt}");
        let mut w = FeedEpisodeAuditWriter::for_test();
        w.append_row(&r).expect("append must succeed");
        let line = w.buffer_utf8();
        assert!(
            !line.contains("eyJaaaa"),
            "JWT-shaped credential must be redacted: {line}"
        );
        // Bound: the evidence field content is ≤ 200 chars post-redaction.
        let evidence_field = line
            .split("evidence=")
            .nth(1)
            .expect("evidence field present");
        assert!(
            evidence_field.len() <= EPISODE_EVIDENCE_MAX_CHARS + 210,
            "evidence must be bounded (field tail: {} bytes)",
            evidence_field.len()
        );
    }

    #[test]
    fn test_episode_flush_when_disconnected_errors() {
        let mut w = FeedEpisodeAuditWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        // Rows stay pending (not lost) for a later retry.
        assert_eq!(w.pending(), 1);
    }

    #[test]
    fn test_episode_flush_empty_is_ok_even_disconnected() {
        let mut w = FeedEpisodeAuditWriter::for_test();
        assert!(w.flush().is_ok(), "empty flush must be a no-op Ok");
    }

    #[test]
    fn test_episode_writer_uses_ilp_http_conf() {
        // Transport ratchet (the 2026-07-05 fire-and-forget lesson): the
        // writer targets the HTTP port for a per-flush server ACK.
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = episode_ilp_http_conf(&cfg);
        assert_eq!(conf, "http::addr=tv-questdb:9000;");
        assert!(
            !conf.contains("9009"),
            "must target the HTTP port, not ILP TCP: {conf}"
        );
    }
}
