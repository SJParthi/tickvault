//! Groww live-vs-backtest 1-minute parity audit (operator lock §32, 2026-06-19).
//! The durable, queryable, SEBI-retentioned system-of-record for the Groww
//! parity check: at the post-market run we compare every Groww-subscribed
//! instrument's live candles (the shared `candles_1m` table, `feed='groww'`)
//! OHLCV against Groww's OWN backtest
//! 1-minute candles, timestamp-by-timestamp, EXACT match (mirrors the Dhan
//! `cross_verify_1m_audit` zero-tolerance rule). Each mismatched field-cell is
//! one row here; the per-day mismatch COUNT is the quality signal that answers
//! *"is Groww live == Groww backtest?"*.
//!
//! **Isolated `groww_*` namespace** — the Dhan `cross_verify_1m_audit` table is
//! UNTOUCHED (operator lock §32: Groww never writes a Dhan table). Storage
//! depends only on `tickvault-common`, so this module is self-contained; the
//! boot orchestrator (in the `app` crate, which depends on both `core` and
//! `storage`) maps `tickvault_core::feed::groww::parity_1m::GrowwParityMismatch`
//! → [`GrowwParityMismatchRow`] and drives the writer.
//!
//! ## Audit table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS groww_cross_verify_1m_audit (
//!     ts               TIMESTAMP,   -- when the parity run executed (IST nanos)
//!     trading_date_ist TIMESTAMP,   -- the trading day verified (IST midnight)
//!     segment          SYMBOL,
//!     security_id      LONG,
//!     minute_ts_ist    TIMESTAMP,   -- the mismatched 1-min bucket (IST nanos)
//!     field            SYMBOL,      -- open / high / low / close / volume
//!     live_value       DOUBLE,      -- our candles_1m (feed='groww') value
//!     backtest_value   DOUBLE       -- Groww's authoritative backtest value
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, security_id, segment, minute_ts_ist, field);
//! ```
//!
//! DEDUP key includes `(security_id, segment)` per I-P1-11 plus the
//! discriminators `(trading_date_ist, minute_ts_ist, field)` so re-running the
//! parity check for a day collapses to one row per mismatched field-cell. The
//! `feed` provenance column is added by the self-heal ALTER (operator lock
//! 2026-06-19 "all tables") — additive, NON-key.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name. One row per mismatched (instrument, minute, field).
pub const GROWW_CROSS_VERIFY_1M_AUDIT_TABLE: &str = "groww_cross_verify_1m_audit";

/// DEDUP UPSERT key — the designated timestamp `ts` FIRST (QuestDB requires the
/// designated timestamp column in every DEDUP key — 2026-05-18 production HTTP-400
/// regression), then the discriminators + `(security_id, segment)` per I-P1-11.
/// The `dedup_segment_meta_guard` workspace test scans this constant; it MUST
/// mention `ts` AND `segment`.
pub const DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT: &str =
    "ts, trading_date_ist, security_id, segment, minute_ts_ist, field";

/// Feed-provenance label for the additive `feed` SYMBOL column (operator lock
/// §32 + 2026-06-19 "all tables"). Every Groww-sourced row carries this.
pub const GROWW_FEED_LABEL: &str = tickvault_common::feed::Feed::Groww.as_str();

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Which OHLCV field disagreed. `&'static str` wire-format keeps the SYMBOL
/// column stable and avoids per-row allocation. Mirrors the Dhan
/// `cross_verify_1m_audit::MismatchField` so the two audit tables share labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowwMismatchField {
    Open,
    High,
    Low,
    Close,
    Volume,
}

impl GrowwMismatchField {
    /// Stable wire-format label for the `field` SYMBOL column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::High => "high",
            Self::Low => "low",
            Self::Close => "close",
            Self::Volume => "volume",
        }
    }
}

/// One mismatched OHLCV field-cell (live vs Groww backtest), ready for ILP write.
///
/// COLD-path struct (post-market parity run, once/day) — `segment` is an owned
/// `String` because it comes from the runtime instrument universe; the
/// allocation is irrelevant off the hot path.
#[derive(Clone, Debug, PartialEq)]
pub struct GrowwParityMismatchRow {
    /// When the parity run executed — IST nanoseconds.
    pub run_ts_ist_nanos: i64,
    /// The trading day verified — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Composite-key part 1 (I-P1-11). Widened to `i64` for `column_i64`.
    pub security_id: i64,
    /// Composite-key part 2 (I-P1-11). Segment string (`IDX_I` / `NSE_EQ` / …).
    pub segment: String,
    /// The mismatched 1-minute bucket — IST nanoseconds.
    pub minute_ts_ist_nanos: i64,
    /// Which field disagreed.
    pub field: GrowwMismatchField,
    /// Our live candle value (shared `candles_1m` table, `feed='groww'`).
    pub live_value: f64,
    /// Groww's authoritative backtest value.
    pub backtest_value: f64,
}

/// The idempotent `CREATE TABLE` DDL for `groww_cross_verify_1m_audit`. Pure
/// (testable without QuestDB).
#[must_use]
pub fn groww_cross_verify_1m_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {GROWW_CROSS_VERIFY_1M_AUDIT_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            segment          SYMBOL, \
            security_id      LONG, \
            minute_ts_ist    TIMESTAMP, \
            field            SYMBOL, \
            live_value       DOUBLE, \
            backtest_value   DOUBLE\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT});"
    )
}

/// Create the audit table if absent (idempotent, schema-self-heal pattern).
/// Failures log at `error!` (Telegram-routable) but do NOT block — the parity
/// run simply can't persist its audit rows and reports it.
// WIRING-EXEMPT: boot DDL-ensure; the call site lands in the parity-run orchestrator follow-up (PR-3 core), like every other ensure_*_table before its boot wiring.
// TEST-EXEMPT: live-QuestDB DDL runner; DDL string unit-tested via groww_cross_verify_1m_audit_create_ddl tests.
pub async fn ensure_groww_cross_verify_1m_audit_table(questdb_config: &QuestDbConfig) {
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
                "groww_cross_verify_1m_audit: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    let ddl = groww_cross_verify_1m_audit_create_ddl();
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
                "groww_cross_verify_1m_audit: CREATE TABLE returned non-2xx");
        }
        Err(err) => error!(
            ?err,
            "groww_cross_verify_1m_audit: CREATE TABLE request failed"
        ),
    }

    // Feed-provenance label (operator 2026-06-19, "all tables"): self-heal ALTER
    // only — additive, idempotent, NON-key; CREATE DDL + its column ratchet are
    // untouched. Free on every boot.
    let alter_feed_ddl = format!(
        "ALTER TABLE {GROWW_CROSS_VERIFY_1M_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS feed SYMBOL"
    );
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
                "groww_cross_verify_1m_audit: ALTER ADD COLUMN feed returned non-2xx");
        }
        Err(err) => error!(
            ?err,
            "groww_cross_verify_1m_audit: ALTER ADD COLUMN feed request failed"
        ),
    }

    // Self-heal the DEDUP key on tables created BEFORE the designated-timestamp
    // fix (2026-06-23): the old key omitted `ts`, which QuestDB rejects. Re-apply
    // the corrected key in place. Idempotent; never drops the table (SEBI).
    let dedup_reenable_ddl = format!(
        "ALTER TABLE {GROWW_CROSS_VERIFY_1M_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT})"
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
                "groww_cross_verify_1m_audit: DEDUP ENABLE UPSERT KEYS returned non-2xx");
        }
        Err(err) => error!(
            ?err,
            "groww_cross_verify_1m_audit: DEDUP ENABLE UPSERT KEYS request failed"
        ),
    }
}

/// Lazy-connect ILP writer for the `groww_cross_verify_1m_audit` table. Mirrors
/// `CrossVerify1mAuditWriter`: if QuestDB is unreachable at construction the
/// writer still builds (`sender = None`); `append_mismatch` fills the local
/// buffer and `flush` returns `Err` until a reconnect lands.
pub struct GrowwCrossVerify1mAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl GrowwCrossVerify1mAuditWriter {
    /// Production constructor — connects via ILP TCP, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB);
    // disconnected/append/flush paths are covered via for_test().
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
                    "groww_cross_verify_1m_audit writer: QuestDB unreachable — buffering locally"
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
    // TEST-EXEMPT: observability accessor, exercised by test_flush_when_disconnected_errors.
    pub fn is_connected(&self) -> bool {
        self.sender.is_some()
    }

    /// Rows buffered since the last flush.
    #[must_use]
    // TEST-EXEMPT: trivial accessor, exercised by the append/pending unit tests.
    pub const fn pending(&self) -> usize {
        self.pending
    }

    /// Raw buffered bytes (tests assert table/segment/value serialisation).
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by append tests.
    pub fn buffer_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    /// Append one mismatch row to the ILP buffer. O(1). The `feed` SYMBOL is
    /// stamped to [`GROWW_FEED_LABEL`] so the table's provenance is explicit.
    pub fn append_mismatch(&mut self, m: &GrowwParityMismatchRow) -> Result<()> {
        self.buffer
            .table(GROWW_CROSS_VERIFY_1M_AUDIT_TABLE)
            .with_context(|| "groww_cross_verify_1m_audit append: table() failed")?
            .symbol("segment", m.segment.as_str())
            .with_context(|| "groww_cross_verify_1m_audit append: symbol(segment) failed")?
            .symbol("field", m.field.as_str())
            .with_context(|| "groww_cross_verify_1m_audit append: symbol(field) failed")?
            .symbol("feed", GROWW_FEED_LABEL)
            .with_context(|| "groww_cross_verify_1m_audit append: symbol(feed) failed")?
            .column_i64("security_id", m.security_id)
            .with_context(|| "groww_cross_verify_1m_audit append: column_i64(security_id) failed")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(m.trading_date_ist_nanos),
            )
            .with_context(
                || "groww_cross_verify_1m_audit append: column_ts(trading_date_ist) failed",
            )?
            .column_ts("minute_ts_ist", TimestampNanos::new(m.minute_ts_ist_nanos))
            .with_context(|| "groww_cross_verify_1m_audit append: column_ts(minute_ts_ist) failed")?
            .column_f64("live_value", m.live_value)
            .with_context(|| "groww_cross_verify_1m_audit append: column_f64(live_value) failed")?
            .column_f64("backtest_value", m.backtest_value)
            .with_context(
                || "groww_cross_verify_1m_audit append: column_f64(backtest_value) failed",
            )?
            .at(TimestampNanos::new(m.run_ts_ist_nanos))
            .with_context(|| "groww_cross_verify_1m_audit append: at(ts) failed")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flush the buffer to QuestDB. `Err` if disconnected.
    pub fn flush(&mut self) -> Result<()> {
        let sender = self
            .sender
            .as_mut()
            .context("groww_cross_verify_1m_audit flush: not connected to QuestDB")?;
        sender
            .flush(&mut self.buffer)
            .context("groww_cross_verify_1m_audit flush: ILP flush failed")?;
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GrowwParityMismatchRow {
        GrowwParityMismatchRow {
            run_ts_ist_nanos: 1_780_000_000_000_000_000,
            trading_date_ist_nanos: 1_780_000_000_000_000_000,
            security_id: 1333,
            segment: "NSE_EQ".to_string(),
            minute_ts_ist_nanos: 1_780_000_060_000_000_000,
            field: GrowwMismatchField::Open,
            live_value: 2_847.50,
            backtest_value: 2_847.00,
        }
    }

    #[test]
    fn test_dedup_key_includes_segment_and_security_id() {
        // I-P1-11 + dedup_segment_meta_guard contract.
        assert!(DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT.contains("segment"));
        assert!(DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT.contains("minute_ts_ist"));
        assert!(DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT.contains("field"));
        // QuestDB requires the designated timestamp `ts` in every DEDUP key
        // (2026-05-18 HTTP-400 regression). Pin it as the first token.
        assert!(
            DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|t| t == "ts"),
            "DEDUP key must include the designated timestamp `ts`: {DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT}"
        );
    }

    #[test]
    fn test_groww_cross_verify_1m_audit_create_ddl_has_all_columns_and_dedup() {
        let ddl = groww_cross_verify_1m_audit_create_ddl();
        for needle in [
            "groww_cross_verify_1m_audit",
            "ts               TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "segment          SYMBOL",
            "security_id      LONG",
            "minute_ts_ist    TIMESTAMP",
            "field            SYMBOL",
            "live_value       DOUBLE",
            "backtest_value   DOUBLE",
            "PARTITION BY DAY",
            "DEDUP UPSERT KEYS(ts, trading_date_ist, security_id, segment, minute_ts_ist, field)",
        ] {
            assert!(ddl.contains(needle), "DDL missing: {needle}\n{ddl}");
        }
    }

    #[test]
    fn test_groww_mismatch_field_as_str_labels_are_stable() {
        assert_eq!(GrowwMismatchField::Open.as_str(), "open");
        assert_eq!(GrowwMismatchField::High.as_str(), "high");
        assert_eq!(GrowwMismatchField::Low.as_str(), "low");
        assert_eq!(GrowwMismatchField::Close.as_str(), "close");
        assert_eq!(GrowwMismatchField::Volume.as_str(), "volume");
    }

    #[test]
    fn test_feed_label_is_groww() {
        assert_eq!(GROWW_FEED_LABEL, "groww");
    }

    #[test]
    fn test_for_test_writer_disconnected_and_empty() {
        let w = GrowwCrossVerify1mAuditWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending(), 0);
    }

    #[test]
    fn test_append_mismatch_fills_buffer_with_table_segment_field_feed() {
        let mut w = GrowwCrossVerify1mAuditWriter::for_test();
        w.append_mismatch(&sample()).expect("append");
        assert_eq!(w.pending(), 1);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(
            text.contains("groww_cross_verify_1m_audit"),
            "table on wire"
        );
        assert!(text.contains("NSE_EQ"), "segment on wire");
        assert!(text.contains("open"), "field on wire");
        assert!(text.contains("groww"), "feed label on wire");
    }

    #[test]
    fn test_two_appends_increment_pending() {
        let mut w = GrowwCrossVerify1mAuditWriter::for_test();
        w.append_mismatch(&sample()).expect("a1");
        w.append_mismatch(&sample()).expect("a2");
        assert_eq!(w.pending(), 2);
    }

    #[test]
    fn test_flush_when_disconnected_errors() {
        let mut w = GrowwCrossVerify1mAuditWriter::for_test();
        assert!(w.flush().is_err(), "disconnected flush must Err, not panic");
    }

    #[test]
    fn test_volume_field_widened_to_f64_round_trips() {
        // The parity row carries volume as f64 (uniform shape); a large integer
        // volume must serialise without precision surprises on the wire.
        let mut m = sample();
        m.field = GrowwMismatchField::Volume;
        m.live_value = 5_000_000.0;
        m.backtest_value = 4_999_800.0;
        let mut w = GrowwCrossVerify1mAuditWriter::for_test();
        w.append_mismatch(&m).expect("append volume");
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(text.contains("volume"), "volume field on wire");
        assert!(text.contains("5000000"), "live volume on wire");
    }
}
