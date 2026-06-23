//! WebSocket lifecycle audit table (`ws_event_audit`, AUDIT-WS-01).
//!
//! Operator directive 2026-06-12: every WebSocket connect / disconnect /
//! reconnect / sleep event must be durably tracked, AND the tracking must be
//! future-proof for a possible expansion to 5 main-feed + 5 depth-20 + 5
//! depth-200 + 1 order-update (= 16) connections. One row per WS lifecycle
//! event, keyed by `(ws_type, connection_index)` so the SAME append path tracks
//! the current 2 connections AND a future 16 with ZERO schema change.
//!
//! This does NOT lift the 2-WebSocket runtime lock
//! (`websocket-connection-scope-lock.md`) — it only makes the forensic record
//! ready for that expansion.
//!
//! ## Audit table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS ws_event_audit (
//!     ts                TIMESTAMP,  -- when the event happened (IST nanos)
//!     trading_date_ist  TIMESTAMP,  -- the trading day (IST midnight)
//!     ws_type           SYMBOL,     -- main_feed / depth_20 / depth_200 / order_update
//!     connection_index  LONG,       -- 0..pool_size-1
//!     pool_size         LONG,       -- configured conns of this ws_type (1 today, up to 5 later)
//!     event_kind        SYMBOL,     -- connected / disconnected / .../ sleep_resumed
//!     source            SYMBOL,     -- classifier label (dhan_token_expired / network / ...)
//!     reason            STRING,     -- REDACTED reason (no JWT) or ""
//!     dhan_code         LONG,       -- 805/807/... ; -1 sentinel = none
//!     down_secs         LONG,       -- reconnect downtime (0 otherwise)
//!     attempts          LONG,       -- reconnect attempts (0 otherwise)
//!     market_hours      BOOLEAN     -- inside [09:00,15:30) IST
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, ws_type, connection_index, event_kind);
//! ```
//!
//! DEDUP key carries the designated timestamp `ts` (2026-04-28 regression rule)
//! AND the composite `(ws_type, connection_index)` — the I-P1-11 composite-
//! uniqueness discipline extended to WebSocket streams, so the 16 future streams
//! never collide. `ts` lets two same-kind events on the same conn in the same
//! second both survive (e.g. 807 then immediate retry-fail).

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::redact_url_params;
pub use tickvault_common::ws_event_types::{WS_EVENT_NO_DHAN_CODE, WsEventAuditRow};
#[cfg(test)]
use tickvault_common::ws_event_types::{WsEventKind, WsType};

/// QuestDB table name. One row per WS lifecycle event.
pub const WS_EVENT_AUDIT_TABLE: &str = "ws_event_audit";

/// DEDUP UPSERT key. Designated timestamp first (2026-04-28 regression rule);
/// `(ws_type, connection_index)` is the composite-unique connection key per
/// I-P1-11 (extended to WS streams); `event_kind` distinguishes the 6 kinds.
pub const DEDUP_KEY_WS_EVENT_AUDIT: &str =
    "ts, trading_date_ist, feed, ws_type, connection_index, event_kind";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// The idempotent `CREATE TABLE` DDL. Pure (testable without QuestDB).
#[must_use]
pub fn ws_event_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {WS_EVENT_AUDIT_TABLE} (\
            ts                TIMESTAMP, \
            trading_date_ist  TIMESTAMP, \
            feed              SYMBOL, \
            ws_type           SYMBOL, \
            connection_index  LONG, \
            pool_size         LONG, \
            event_kind        SYMBOL, \
            source            SYMBOL, \
            reason            STRING, \
            dhan_code         LONG, \
            down_secs         LONG, \
            attempts          LONG, \
            market_hours      BOOLEAN\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_WS_EVENT_AUDIT});"
    )
}

/// Create the audit table if absent (idempotent, schema-self-heal pattern).
/// Failures log at `error!` (Telegram-routable) but do NOT block boot — the WS
/// events still reach CloudWatch logs + Telegram.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested via ws_event_audit_create_ddl tests)
pub async fn ensure_ws_event_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                ?err,
                "ws_event_audit: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    let ddl = ws_event_audit_create_ddl();
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
            error!(%status, body = %body.chars().take(200).collect::<String>(),
                "ws_event_audit: CREATE TABLE returned non-2xx");
        }
        Err(err) => error!(?err, "ws_event_audit: CREATE TABLE request failed"),
    }

    // Per-feed identity (operator 2026-06-23): `feed` is now a DEDUP-key column —
    // a Dhan and a Groww connection can share `(ws_type, connection_index)`, so
    // their lifecycle events must stay distinct rows. Fresh tables get the column
    // + key from the CREATE DDL above; this self-heal ALTER lands `feed` on tables
    // created before the directive. Idempotent. Free on every boot.
    let alter_feed_ddl =
        format!("ALTER TABLE {WS_EVENT_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS feed SYMBOL");
    match client
        .get(&base_url)
        .query(&[("query", alter_feed_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(%status, body = %body.chars().take(200).collect::<String>(),
                "ws_event_audit: ALTER ADD COLUMN feed returned non-2xx");
        }
        Err(err) => error!(?err, "ws_event_audit: ALTER ADD COLUMN feed request failed"),
    }

    // Re-apply the DEDUP key (now including `feed`) on tables created before the
    // 2026-06-23 directive. Runs AFTER the `feed` column ALTER so the key column
    // exists. Idempotent; never drops the table (SEBI). Mirrors the ticks +
    // prev_day_ohlcv `DEDUP ENABLE` self-heal.
    let dedup_reenable_ddl = format!(
        "ALTER TABLE {WS_EVENT_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_WS_EVENT_AUDIT})"
    );
    match client
        .get(&base_url)
        .query(&[("query", dedup_reenable_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(%status, body = %body.chars().take(200).collect::<String>(),
                "ws_event_audit: DEDUP ENABLE UPSERT KEYS returned non-2xx");
        }
        Err(err) => error!(
            ?err,
            "ws_event_audit: DEDUP ENABLE UPSERT KEYS request failed"
        ),
    }
}

/// Lazy-connect ILP writer for the `ws_event_audit` table. Mirrors
/// `TickConservationAuditWriter`: if QuestDB is unreachable at construction the
/// writer still builds (`sender = None`); `append_row` fills the local buffer
/// and `flush` returns `Err` until QuestDB is reachable.
pub struct WsEventAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl WsEventAuditWriter {
    /// Production constructor — connects via ILP TCP, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); disconnected/append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = format!("tcp::addr={}:{};", config.host, config.ilp_port);
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
                    "ws_event_audit writer: QuestDB unreachable — buffering locally"
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

    /// `true` when a live ILP sender is held.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by flush tests below.
    pub fn is_connected(&self) -> bool {
        self.sender.is_some()
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Appends one WS-event audit row to the ILP buffer (cold path, once per
    /// disconnect/reconnect/sleep — never per tick).
    ///
    /// SECURITY: the `reason` is redacted here via [`redact_url_params`] so a
    /// token-bearing URL can never reach the table even if a caller forgets.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &WsEventAuditRow) -> Result<()> {
        let safe_reason = redact_url_params(&r.reason);
        self.buffer
            .table(WS_EVENT_AUDIT_TABLE)
            .context("table")?
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("ws_type", r.ws_type.as_str())
            .context("ws_type")?
            .symbol("event_kind", r.event_kind.as_str())
            .context("event_kind")?
            .symbol("source", r.source.as_str())
            .context("source")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("connection_index", r.connection_index)
            .context("connection_index")?
            .column_i64("pool_size", r.pool_size)
            .context("pool_size")?
            .column_str("reason", safe_reason.as_str())
            .context("reason")?
            .column_i64("dhan_code", r.dhan_code)
            .context("dhan_code")?
            .column_i64("down_secs", r.down_secs)
            .context("down_secs")?
            .column_i64("attempts", r.attempts)
            .context("attempts")?
            .column_bool("market_hours", r.market_hours)
            .context("market_hours")?
            .at(TimestampNanos::new(r.event_ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows to QuestDB.
    ///
    /// # Errors
    /// `Err` when disconnected or the TCP flush fails (rows stay buffered).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("ws_event_audit: no ILP sender (QuestDB unreachable)");
        };
        sender
            .flush(&mut self.buffer)
            .context("ws_event_audit ILP flush")?;
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

    fn sample_row() -> WsEventAuditRow {
        WsEventAuditRow {
            event_ts_ist_nanos: 1_770_000_000_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            feed: tickvault_common::feed::Feed::Dhan,
            ws_type: WsType::MainFeed,
            connection_index: 0,
            pool_size: 1,
            event_kind: WsEventKind::Disconnected,
            source: "dhan_token_expired".to_string(),
            reason: "Token expired (807)".to_string(),
            dhan_code: 807,
            down_secs: 0,
            attempts: 0,
            market_hours: true,
        }
    }

    #[test]
    fn test_ws_event_audit_create_ddl_contains_expected_columns() {
        let ddl = ws_event_audit_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "feed",
            "ws_type",
            "connection_index",
            "pool_size",
            "event_kind",
            "source",
            "reason",
            "dhan_code",
            "down_secs",
            "attempts",
            "market_hours",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
    }

    #[test]
    fn test_ws_event_dedup_key_includes_ts_and_composite_connection_key() {
        // 2026-04-28 regression rule: designated timestamp in the DEDUP key.
        assert!(DEDUP_KEY_WS_EVENT_AUDIT.contains("ts"));
        // I-P1-11 (extended to WS streams): the connection is uniquely
        // (ws_type, connection_index) — both MUST be in the key.
        assert!(DEDUP_KEY_WS_EVENT_AUDIT.contains("ws_type"));
        assert!(DEDUP_KEY_WS_EVENT_AUDIT.contains("connection_index"));
        assert!(DEDUP_KEY_WS_EVENT_AUDIT.contains("event_kind"));
        // Per-feed identity (2026-06-23): Dhan + Groww connections can share
        // (ws_type, connection_index), so `feed` MUST be in the key.
        assert!(DEDUP_KEY_WS_EVENT_AUDIT.contains("feed"));
        let ddl = ws_event_audit_create_ddl();
        assert!(ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_WS_EVENT_AUDIT})")));
    }

    #[test]
    fn test_no_dhan_code_sentinel_is_negative_one() {
        assert_eq!(WS_EVENT_NO_DHAN_CODE, -1);
    }

    #[test]
    fn test_append_row_fills_buffer_disconnected() {
        let mut w = WsEventAuditWriter::for_test();
        assert!(!w.is_connected());
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 1);
    }

    #[test]
    fn test_append_row_redacts_token_in_reason() {
        // SECURITY: a reason embedding the feed URL must NOT be storable raw.
        // We can't read the ILP buffer back, but we can prove the redactor is
        // invoked by feeding a reason and confirming append succeeds + the
        // public redactor neutralizes the same input.
        let mut row = sample_row();
        row.reason = "TLS error wss://api-feed.dhan.co?token=eyJsecretjwt&clientId=99".to_string();
        let mut w = WsEventAuditWriter::for_test();
        w.append_row(&row).expect("append must succeed");
        // The same input through the public redactor must drop the token —
        // append_row uses exactly this function on the reason before write.
        let redacted = redact_url_params(&row.reason);
        assert!(
            !redacted.contains("eyJsecretjwt"),
            "token leaked: {redacted}"
        );
        assert!(
            !redacted.contains("clientId=99"),
            "clientId leaked: {redacted}"
        );
    }

    #[test]
    fn test_append_row_each_event_kind_and_ws_type() {
        // Every (ws_type, event_kind) builds a valid row — pins the append path
        // for all 4 future WS types + all 6 event kinds (16-connection scenario).
        for ws_type in WsType::all() {
            for event_kind in WsEventKind::all() {
                let mut w = WsEventAuditWriter::for_test();
                let mut row = sample_row();
                row.ws_type = ws_type;
                row.event_kind = event_kind;
                row.connection_index = 4; // future depth pool index
                row.pool_size = 5;
                w.append_row(&row)
                    .unwrap_or_else(|e| panic!("append {ws_type:?}/{event_kind:?}: {e}"));
                assert_eq!(w.pending(), 1);
            }
        }
    }

    #[test]
    fn test_flush_when_disconnected_errors() {
        let mut w = WsEventAuditWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert_eq!(w.pending(), 1);
    }

    #[test]
    fn test_flush_empty_is_ok_even_disconnected() {
        let mut w = WsEventAuditWriter::for_test();
        assert!(w.flush().is_ok(), "empty flush must be a no-op Ok");
    }
}
