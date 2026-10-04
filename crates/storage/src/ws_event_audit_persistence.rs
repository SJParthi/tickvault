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
//!     feed              SYMBOL,     -- which broker feed the socket belongs to
//!     ws_type           SYMBOL,     -- main_feed / depth_20 / depth_200 / order_update
//!     connection_index  LONG,       -- 0..pool_size-1
//!     pool_size         LONG,       -- configured conns of this ws_type (1 today, up to 5 later)
//!     event_kind        SYMBOL,     -- connected / disconnected / .../ sleep_resumed
//!     source            SYMBOL,     -- classifier label (dhan_token_expired / network / ...)
//!     reason            STRING,     -- REDACTED reason (no JWT) or ""
//!     dhan_code         LONG,       -- 805/807/... ; -1 sentinel = none
//!     down_secs         LONG,       -- reconnect downtime (0 otherwise)
//!     attempts          LONG,       -- reconnect attempts (0 otherwise)
//!     market_hours      BOOLEAN,    -- inside [09:00,15:30) IST
//!     security_id       LONG,       -- instrument that LEFT (swap old / ghost), NULL if none
//!     segment           SYMBOL,     -- its segment (I-P1-11 pair with security_id)
//!     new_security_id   LONG,       -- instrument that ARRIVED (swap new), NULL if none
//!     new_segment       SYMBOL,     -- its segment
//!     instruments_added LONG,       -- subscribed by the change (what landed), NULL if n/a
//!     instruments_removed LONG,     -- unsubscribed by the change (what landed), NULL if n/a
//!     instruments_held  LONG        -- socket's instrument count after the event, NULL if n/a
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, ws_type, connection_index, event_kind);
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
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::redact_url_params;
pub use tickvault_common::ws_event_types::{WS_EVENT_NO_DHAN_CODE, WsEventAuditRow};
#[cfg(test)]
use tickvault_common::ws_event_types::{WsEventKind, WsType};

/// QuestDB table name. One row per WS lifecycle event.
pub const WS_EVENT_AUDIT_TABLE: &str = "ws_event_audit";

/// DEDUP UPSERT key. Designated timestamp first (2026-04-28 regression rule);
/// `(ws_type, connection_index)` is the composite-unique connection key per
/// I-P1-11 (extended to WS streams); `event_kind` distinguishes the kinds.
///
/// Audit M8 (2026-10-04): the subscription-change kinds (`subscription_swapped`,
/// `subscription_resubscribed`, `ghost_unsubscribe_resent`, `overflow_parked`)
/// use this SAME key; the instrument columns are deliberately NOT in it. Two
/// such events on one connection could only collide if they carried the same
/// `ts` AND the same `event_kind`; the live-feed forwarder that stamps every
/// market-data row makes `ts` strictly increasing across ALL its rows
/// (`max(now, previous + 1 ns)`), so no two rows it writes share a `ts`, and
/// a later row can never UPSERT over an earlier one.
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
            market_hours      BOOLEAN, \
            security_id       LONG, \
            segment           SYMBOL, \
            new_security_id   LONG, \
            new_segment       SYMBOL, \
            instruments_added LONG, \
            instruments_removed LONG, \
            instruments_held  LONG\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_WS_EVENT_AUDIT});"
    )
}

/// The subscription-change columns (audit M8, 2026-10-04), name and type,
/// in the order the CREATE DDL lists them.
///
/// `security_id` + `segment` name the instrument that LEFT (swap `old`, the
/// ghost, the single unsubscribed instrument of a resubscribe);
/// `new_security_id` + `new_segment` the one that ARRIVED. Always written as a
/// pair (I-P1-11: the id alone is not an instrument). Absent facts are NULL.
pub const WS_EVENT_AUDIT_SUBSCRIPTION_COLUMNS: [(&str, &str); 7] = [
    ("security_id", "LONG"),
    ("segment", "SYMBOL"),
    ("new_security_id", "LONG"),
    ("new_segment", "SYMBOL"),
    ("instruments_added", "LONG"),
    ("instruments_removed", "LONG"),
    ("instruments_held", "LONG"),
];

/// One `ALTER TABLE … ADD COLUMN IF NOT EXISTS` per subscription-change
/// column, so a table created before 2026-10-04 heals at boot. Pure.
#[must_use]
pub fn ws_event_audit_subscription_alter_ddls() -> Vec<String> {
    WS_EVENT_AUDIT_SUBSCRIPTION_COLUMNS
        .iter()
        .map(|(name, kind)| {
            format!("ALTER TABLE {WS_EVENT_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {name} {kind}")
        })
        .collect()
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
                code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
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
            error!(code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                %status, body = %body.chars().take(200).collect::<String>(),
                "ws_event_audit: CREATE TABLE returned non-2xx");
        }
        Err(err) => error!(
            code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
            ?err,
            "ws_event_audit: CREATE TABLE request failed"
        ),
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
            error!(code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                %status, body = %body.chars().take(200).collect::<String>(),
                "ws_event_audit: ALTER ADD COLUMN feed returned non-2xx");
        }
        Err(err) => error!(
            code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
            ?err,
            "ws_event_audit: ALTER ADD COLUMN feed request failed"
        ),
    }

    // Audit M8 (2026-10-04): the subscription-change columns land on tables
    // created before them. Idempotent; never drops the table (SEBI). None of
    // them is in the DEDUP key, so no DEDUP re-apply depends on them.
    for ddl in ws_event_audit_subscription_alter_ddls() {
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
                error!(code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                    %status, body = %body.chars().take(200).collect::<String>(),
                    ddl = %ddl,
                    "ws_event_audit: ALTER ADD COLUMN (subscription change) returned non-2xx");
            }
            Err(err) => error!(
                code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                ?err,
                ddl = %ddl,
                "ws_event_audit: ALTER ADD COLUMN (subscription change) request failed"
            ),
        }
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
            error!(code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
                %status, body = %body.chars().take(200).collect::<String>(),
                "ws_event_audit: DEDUP ENABLE UPSERT KEYS returned non-2xx");
        }
        Err(err) => error!(
            code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
            ?err,
            "ws_event_audit: DEDUP ENABLE UPSERT KEYS request failed"
        ),
    }
}

/// Lazy-connect ILP writer for the `ws_event_audit` table. Mirrors
/// `TickConservationAuditWriter`: if QuestDB is unreachable at construction the
/// writer still builds (`sender = None`); `append_row` fills the local buffer
/// and `flush` returns `Err` until QuestDB is reachable.
///
/// 2026-07-05: transport is **ILP-over-HTTP** (`http::addr=host:http_port`),
/// NOT ILP TCP. TCP flush is fire-and-forget — a server-side reject (schema
/// drift, DEDUP violation, table error) NEVER surfaced as `Err`, so the
/// AUDIT-WS-01 flush arm never paged and the table could stay silently EMPTY
/// (the 2026-07-05 operator-caught incident). HTTP flush returns a real server
/// ACK per request; rejects surface as `Err` → the existing AUDIT-WS-01 arm
/// pages. Cold path (a handful of lifecycle rows per day) — the extra HTTP
/// round-trip latency is irrelevant here.
pub struct WsEventAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

/// Builds the ILP-over-HTTP conf string for the `ws_event_audit` writer.
/// Targets the QuestDB HTTP port (9000-class), NOT the ILP TCP port (9009) —
/// HTTP gives a per-flush server ACK (see the 2026-07-05 note on
/// [`WsEventAuditWriter`]). Pure + unit-tested (the transport ratchet).
fn ilp_http_conf(config: &QuestDbConfig) -> String {
    format!("http::addr={}:{};", config.host, config.http_port)
}

/// A `SecurityId` (u64) as the LONG the table stores. Dhan ids are far below
/// 2^63; an id that is not is stored as -1 rather than wrapped into a
/// different, plausible-looking id.
fn audit_security_id(security_id: u64) -> i64 {
    i64::try_from(security_id).unwrap_or(-1)
}

impl WsEventAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    /// (`Sender::from_conf` with `http::` does not dial at construction; an
    /// unreachable QuestDB surfaces at `flush()` as `Err` → AUDIT-WS-01.)
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); disconnected/append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        // Seeded here so an absent discard series can never read as a healthy
        // zero — see `ilp_overflow::register_overflow_baseline`.
        crate::ilp_overflow::register_overflow_baseline("ws_event_audit");
        let conf = ilp_http_conf(config);
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
            .context("source")?;
        // Audit M8 (2026-10-04): the subscription-change facts. ILP needs
        // every symbol before the first non-symbol column, so the two segment
        // symbols go here. An absent fact is LEFT OUT of the row, which
        // QuestDB stores as NULL — a lifecycle row reads exactly as before.
        let sub = &r.subscription;
        if let Some(i) = sub.instrument {
            self.buffer
                .symbol("segment", i.segment.as_str())
                .context("segment")?;
        }
        if let Some(i) = sub.new_instrument {
            self.buffer
                .symbol("new_segment", i.segment.as_str())
                .context("new_segment")?;
        }
        if let Some(i) = sub.instrument {
            self.buffer
                .column_i64("security_id", audit_security_id(i.security_id))
                .context("security_id")?;
        }
        if let Some(i) = sub.new_instrument {
            self.buffer
                .column_i64("new_security_id", audit_security_id(i.security_id))
                .context("new_security_id")?;
        }
        for (name, value) in [
            ("instruments_added", sub.added),
            ("instruments_removed", sub.removed),
            ("instruments_held", sub.held),
        ] {
            if let Some(v) = value {
                self.buffer.column_i64(name, i64::from(v)).context(name)?;
            }
        }
        self.buffer
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

    /// Flushes buffered rows to QuestDB over ILP-HTTP.
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails — including a
    /// SERVER-SIDE reject, which the HTTP ACK surfaces (TCP never did). Rows
    /// stay buffered; the consumer's AUDIT-WS-01 arm pages.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("ws_event_audit: no ILP sender (QuestDB unreachable)");
        };
        if let Err(err) = sender.flush(&mut self.buffer) {
            // RETAIN on failure (the questdb-rs contract), but BOUNDED: a
            // sustained outage or a server-side reject would otherwise grow
            // this buffer without limit and, for a reject, keep it poisoned
            // for the process lifetime. See ilp_overflow.
            let dropped = crate::ilp_overflow::discard_if_overflowing(
                &mut self.buffer,
                &mut self.pending,
                "ws_event_audit",
            );
            return Err(anyhow::Error::new(err).context(
                crate::ilp_overflow::flush_failure_context("ws_event_audit ILP flush", dropped),
            ));
        }
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
            subscription: tickvault_common::ws_event_types::WsSubscriptionDetail::NONE,
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
    fn test_ilp_http_conf_targets_http_port() {
        // 2026-07-05 transport ratchet: the ws_event_audit writer MUST use
        // ILP-over-HTTP (per-flush server ACK — server-side rejects surface as
        // flush Err → AUDIT-WS-01). Regressing to `tcp::addr` restores the
        // fire-and-forget silent-loss window the operator caught on 2026-07-05.
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = ilp_http_conf(&cfg);
        assert_eq!(conf, "http::addr=tv-questdb:9000;");
        assert!(
            conf.starts_with("http::addr="),
            "must be ILP-over-HTTP, never tcp:: — got {conf}"
        );
        assert!(
            !conf.contains("9009"),
            "must target http_port, not the ILP TCP port — got {conf}"
        );
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

    /// Audit M8 (2026-10-04): the subscription-change columns are in the
    /// CREATE DDL and each has its own idempotent self-heal ALTER.
    #[test]
    fn test_ws_event_audit_subscription_columns_in_ddl_and_self_heal() {
        let ddl = ws_event_audit_create_ddl();
        let alters = ws_event_audit_subscription_alter_ddls();
        assert_eq!(alters.len(), WS_EVENT_AUDIT_SUBSCRIPTION_COLUMNS.len());
        for ((name, kind), alter) in WS_EVENT_AUDIT_SUBSCRIPTION_COLUMNS.iter().zip(&alters) {
            assert!(ddl.contains(name), "DDL missing {name}: {ddl}");
            assert_eq!(
                alter,
                &format!("ALTER TABLE ws_event_audit ADD COLUMN IF NOT EXISTS {name} {kind}")
            );
        }
        // The id is always paired with its segment (I-P1-11).
        assert!(ddl.contains("security_id       LONG") && ddl.contains("segment           SYMBOL"));
        assert!(ddl.contains("new_security_id   LONG") && ddl.contains("new_segment       SYMBOL"));
        // None of the new columns joined the DEDUP key: uniqueness rests on
        // the forwarder's strictly increasing `ts` (see DEDUP_KEY doc).
        for (name, _) in WS_EVENT_AUDIT_SUBSCRIPTION_COLUMNS {
            assert!(
                !DEDUP_KEY_WS_EVENT_AUDIT.split(", ").any(|k| k == name),
                "{name} must not be in the DEDUP key"
            );
        }
    }

    /// Every subscription-change kind appends with its instrument facts, and
    /// a row carrying them still appends alongside a plain lifecycle row.
    #[test]
    fn test_append_row_with_subscription_detail_for_each_new_kind() {
        use tickvault_common::types::ExchangeSegment;
        use tickvault_common::ws_event_types::{WsAuditInstrument, WsSubscriptionDetail};
        let old = WsAuditInstrument {
            security_id: 52_175,
            segment: ExchangeSegment::NseFno,
        };
        let new = WsAuditInstrument {
            security_id: 52_176,
            segment: ExchangeSegment::NseFno,
        };
        let mut w = WsEventAuditWriter::for_test();
        for kind in [
            WsEventKind::SubscriptionSwapped,
            WsEventKind::SubscriptionResubscribed,
            WsEventKind::GhostUnsubscribeResent,
            WsEventKind::OverflowParked,
        ] {
            let mut row = sample_row();
            row.ws_type = WsType::Depth200;
            row.event_kind = kind;
            row.subscription = WsSubscriptionDetail {
                instrument: Some(old),
                new_instrument: (kind == WsEventKind::SubscriptionSwapped).then_some(new),
                added: Some(1),
                removed: Some(1),
                held: Some(1),
            };
            w.append_row(&row)
                .unwrap_or_else(|e| panic!("append {kind:?}: {e}"));
        }
        w.append_row(&sample_row()).expect("plain lifecycle row");
        assert_eq!(w.pending(), 5);
    }

    #[test]
    fn test_audit_security_id_never_wraps() {
        assert_eq!(audit_security_id(13), 13);
        assert_eq!(audit_security_id(u64::MAX), -1);
    }

    #[test]
    fn test_flush_empty_is_ok_even_disconnected() {
        let mut w = WsEventAuditWriter::for_test();
        assert!(w.flush().is_ok(), "empty flush must be a no-op Ok");
    }
}
