//! Post-market 1-minute cross-verification audit + CSV (operator directive
//! 2026-06-02). At 15:31 IST each trading day we compare every subscribed
//! SPOT instrument's live `candles_1m` OHLCV against Dhan's authoritative
//! intraday 1-minute candles, timestamp-by-timestamp, EXACT match (operator:
//! *"csv and exact match cross verification is needed"*). Each mismatch is
//! recorded here (QuestDB audit table) AND appended to an easily-accessible,
//! trackable CSV file. The per-day mismatch COUNT is the quality signal.
//!
//! **This is NOT the deleted 90-day `cross_verify.rs` chain** — it is a
//! narrowed replacement: 1-minute only, spot-only, today-only, post-market
//! only. See `live-feed-purity.md` rule 11.
//!
//! ## Audit table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS cross_verify_1m_audit (
//!     ts               TIMESTAMP,   -- when the verification ran (IST nanos)
//!     trading_date_ist TIMESTAMP,   -- the trading day verified (IST midnight)
//!     segment          SYMBOL,
//!     security_id      LONG,
//!     minute_ts_ist    TIMESTAMP,   -- the mismatched 1-min bucket (IST nanos)
//!     field            SYMBOL,      -- open / high / low / close / volume
//!     our_value        DOUBLE,
//!     dhan_value       DOUBLE
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, security_id, segment, minute_ts_ist, field);
//! ```
//!
//! DEDUP key includes `(security_id, segment)` per I-P1-11 plus the
//! discriminators `(trading_date_ist, minute_ts_ist, field)` so re-running the
//! verification for a day collapses to one row per mismatched field-cell.
//!
//! ## CSV
//!
//! `data/cross-verify/cross-verify-1m-YYYY-MM-DD.csv` — one header + one line
//! per mismatch. Opens directly in Excel. Pure formatting helpers
//! ([`csv_header`], [`mismatch_to_csv_line`]) are unit-tested without a
//! filesystem; the file write is a thin async wrapper.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name. One row per mismatched (instrument, minute, field).
pub const CROSS_VERIFY_1M_AUDIT_TABLE: &str = "cross_verify_1m_audit";

/// DEDUP UPSERT key — the designated timestamp `ts` FIRST (QuestDB requires the
/// designated timestamp column in every DEDUP key — 2026-05-18 production HTTP-400
/// regression), then the discriminators + `(security_id, segment)` per I-P1-11.
/// The `dedup_segment_meta_guard` workspace test scans this constant; it MUST
/// mention `ts` AND `segment`.
pub const DEDUP_KEY_CROSS_VERIFY_1M_AUDIT: &str =
    "ts, trading_date_ist, security_id, segment, minute_ts_ist, field";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Which OHLCV field disagreed. `&'static str` wire-format keeps the SYMBOL
/// column stable and avoids per-row allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MismatchField {
    Open,
    High,
    Low,
    Close,
    Volume,
}

impl MismatchField {
    /// Stable wire-format label for the `field` SYMBOL column + CSV.
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

/// One mismatched OHLCV field-cell, ready for ILP write + CSV append.
///
/// Not `Copy` — `segment`/`symbol` are owned `String`s because they come from
/// the runtime instrument universe. This is a COLD-path struct (post-market
/// verification, once/day), so the allocation is irrelevant.
#[derive(Clone, Debug, PartialEq)]
pub struct CrossVerify1mMismatch {
    /// Broker feed source (`dhan` / `groww`). This Dhan cross-verify audit is
    /// Dhan-only by design (Groww has its OWN `groww_cross_verify_1m_audit`
    /// table per lock §32), so this is always `dhan` — a self-describing LABEL,
    /// NOT a DEDUP-key column (the verification is per-instrument-minute, not
    /// per-feed within this table).
    pub feed: &'static str,
    /// When the verification ran — IST nanoseconds.
    pub run_ts_ist_nanos: i64,
    /// The trading day verified — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Composite-key part 1 (I-P1-11). Widened to `i64` for `column_i64`.
    pub security_id: i64,
    /// Composite-key part 2 (I-P1-11). Segment string (`IDX_I` / `NSE_EQ` / …).
    pub segment: String,
    /// Human-readable symbol (CSV only — NOT a QuestDB column to keep the
    /// table lean; the audit row is keyed by security_id + segment).
    pub symbol: String,
    /// The mismatched 1-minute bucket — IST nanoseconds.
    pub minute_ts_ist_nanos: i64,
    /// Which field disagreed.
    pub field: MismatchField,
    /// Our `candles_1m` value.
    pub our_value: f64,
    /// Dhan's authoritative intraday value.
    pub dhan_value: f64,
}

/// CSV header line (no trailing newline). Matches [`mismatch_to_csv_line`]
/// column order exactly.
#[must_use]
pub fn csv_header() -> &'static str {
    "trading_date_ist_nanos,symbol,segment,security_id,minute_ts_ist_nanos,field,our_value,dhan_value"
}

/// Format one mismatch as a CSV line (no trailing newline). Pure — testable
/// without a filesystem. The `symbol` is the only free-text field; it is
/// sanitized (commas / newlines / quotes stripped) so it can never break the
/// CSV grid (defence-in-depth — symbols come from the trusted instrument
/// master, but a stray comma would still corrupt the file).
#[must_use]
pub fn mismatch_to_csv_line(m: &CrossVerify1mMismatch) -> String {
    let safe_symbol: String = m
        .symbol
        .chars()
        .filter(|c| *c != ',' && *c != '\n' && *c != '\r' && *c != '"')
        .collect();
    format!(
        "{},{},{},{},{},{},{},{}",
        m.trading_date_ist_nanos,
        safe_symbol,
        m.segment,
        m.security_id,
        m.minute_ts_ist_nanos,
        m.field.as_str(),
        m.our_value,
        m.dhan_value,
    )
}

/// The idempotent `CREATE TABLE` DDL for `cross_verify_1m_audit`. Pure
/// (testable without QuestDB).
#[must_use]
pub fn cross_verify_1m_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {CROSS_VERIFY_1M_AUDIT_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            segment          SYMBOL, \
            security_id      LONG, \
            minute_ts_ist    TIMESTAMP, \
            field            SYMBOL, \
            our_value        DOUBLE, \
            dhan_value       DOUBLE\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_CROSS_VERIFY_1M_AUDIT});"
    )
}

/// Create the audit table if absent (idempotent, schema-self-heal pattern).
/// Failures log at `error!` (Telegram-routable) but do NOT block — the
/// verification simply can't persist its audit rows and reports it.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested via
// cross_verify_1m_audit_create_ddl tests).
pub async fn ensure_cross_verify_1m_audit_table(questdb_config: &QuestDbConfig) {
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
                "cross_verify_1m_audit: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    let ddl = cross_verify_1m_audit_create_ddl();
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
                "cross_verify_1m_audit: CREATE TABLE returned non-2xx");
        }
        Err(err) => error!(?err, "cross_verify_1m_audit: CREATE TABLE request failed"),
    }

    // Feed-provenance label (operator 2026-06-19, "all tables"): self-heal ALTER
    // only — additive, idempotent, NON-key; CREATE DDL + its column ratchet are
    // untouched. Free on every boot.
    let alter_feed_ddl =
        format!("ALTER TABLE {CROSS_VERIFY_1M_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS feed SYMBOL");
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
                "cross_verify_1m_audit: ALTER ADD COLUMN feed returned non-2xx");
        }
        Err(err) => error!(
            ?err,
            "cross_verify_1m_audit: ALTER ADD COLUMN feed request failed"
        ),
    }

    // Self-heal the DEDUP key on tables created BEFORE the designated-timestamp
    // fix (2026-06-23): the old key omitted `ts`, which QuestDB rejects. Re-apply
    // the corrected key in place. Idempotent; never drops the table (SEBI).
    let dedup_reenable_ddl = format!(
        "ALTER TABLE {CROSS_VERIFY_1M_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_CROSS_VERIFY_1M_AUDIT})"
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
                "cross_verify_1m_audit: DEDUP ENABLE UPSERT KEYS returned non-2xx");
        }
        Err(err) => error!(
            ?err,
            "cross_verify_1m_audit: DEDUP ENABLE UPSERT KEYS request failed"
        ),
    }
}

/// Lazy-connect ILP writer for the `cross_verify_1m_audit` table. Mirrors
/// `PrevDayOhlcvWriter`: if QuestDB is unreachable at construction the writer
/// still builds (`sender = None`); `append_mismatch` fills the local buffer and
/// `flush` returns `Err` until a reconnect lands.
pub struct CrossVerify1mAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl CrossVerify1mAuditWriter {
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
                    "cross_verify_1m_audit writer: QuestDB unreachable — buffering locally"
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
    pub const fn pending(&self) -> usize {
        self.pending
    }

    /// Raw buffered bytes (tests assert table/segment/value serialisation).
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by append tests.
    pub fn buffer_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    /// Append one mismatch row to the ILP buffer. O(1).
    pub fn append_mismatch(&mut self, m: &CrossVerify1mMismatch) -> Result<()> {
        self.buffer
            .table(CROSS_VERIFY_1M_AUDIT_TABLE)
            .with_context(|| "cross_verify_1m_audit append: table() failed")?
            .symbol("feed", m.feed)
            .with_context(|| "cross_verify_1m_audit append: symbol(feed) failed")?
            .symbol("segment", m.segment.as_str())
            .with_context(|| "cross_verify_1m_audit append: symbol(segment) failed")?
            .symbol("field", m.field.as_str())
            .with_context(|| "cross_verify_1m_audit append: symbol(field) failed")?
            .column_i64("security_id", m.security_id)
            .with_context(|| "cross_verify_1m_audit append: column_i64(security_id) failed")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(m.trading_date_ist_nanos),
            )
            .with_context(|| "cross_verify_1m_audit append: column_ts(trading_date_ist) failed")?
            .column_ts("minute_ts_ist", TimestampNanos::new(m.minute_ts_ist_nanos))
            .with_context(|| "cross_verify_1m_audit append: column_ts(minute_ts_ist) failed")?
            .column_f64("our_value", m.our_value)
            .with_context(|| "cross_verify_1m_audit append: column_f64(our_value) failed")?
            .column_f64("dhan_value", m.dhan_value)
            .with_context(|| "cross_verify_1m_audit append: column_f64(dhan_value) failed")?
            .at(TimestampNanos::new(m.run_ts_ist_nanos))
            .with_context(|| "cross_verify_1m_audit append: at(ts) failed")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flush the buffer to QuestDB. `Err` if disconnected.
    pub fn flush(&mut self) -> Result<()> {
        let sender = self
            .sender
            .as_mut()
            .context("cross_verify_1m_audit flush: not connected to QuestDB")?;
        sender
            .flush(&mut self.buffer)
            .context("cross_verify_1m_audit flush: ILP flush failed")?;
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CrossVerify1mMismatch {
        CrossVerify1mMismatch {
            feed: "dhan",
            run_ts_ist_nanos: 1_780_000_000_000_000_000,
            trading_date_ist_nanos: 1_780_000_000_000_000_000,
            security_id: 13,
            segment: "IDX_I".to_string(),
            symbol: "NIFTY".to_string(),
            minute_ts_ist_nanos: 1_780_000_060_000_000_000,
            field: MismatchField::Open,
            our_value: 23_342.95,
            dhan_value: 23_342.60,
        }
    }

    #[test]
    fn test_dedup_key_includes_segment_and_security_id() {
        // I-P1-11 + dedup_segment_meta_guard contract.
        assert!(DEDUP_KEY_CROSS_VERIFY_1M_AUDIT.contains("segment"));
        assert!(DEDUP_KEY_CROSS_VERIFY_1M_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_CROSS_VERIFY_1M_AUDIT.contains("minute_ts_ist"));
        assert!(DEDUP_KEY_CROSS_VERIFY_1M_AUDIT.contains("field"));
        // QuestDB requires the designated timestamp `ts` in every DEDUP key
        // (2026-05-18 HTTP-400 regression). Pin it as the first token.
        assert!(
            DEDUP_KEY_CROSS_VERIFY_1M_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|t| t == "ts"),
            "DEDUP key must include the designated timestamp `ts`: {DEDUP_KEY_CROSS_VERIFY_1M_AUDIT}"
        );
    }

    #[test]
    fn test_cross_verify_1m_audit_create_ddl_has_all_columns_and_dedup() {
        let ddl = cross_verify_1m_audit_create_ddl();
        for needle in [
            "cross_verify_1m_audit",
            "ts               TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "segment          SYMBOL",
            "security_id      LONG",
            "minute_ts_ist    TIMESTAMP",
            "field            SYMBOL",
            "our_value        DOUBLE",
            "dhan_value       DOUBLE",
            "PARTITION BY DAY",
            "DEDUP UPSERT KEYS(ts, trading_date_ist, security_id, segment, minute_ts_ist, field)",
        ] {
            assert!(ddl.contains(needle), "DDL missing: {needle}\n{ddl}");
        }
    }

    #[test]
    fn test_mismatch_field_as_str_labels_are_stable() {
        assert_eq!(MismatchField::Open.as_str(), "open");
        assert_eq!(MismatchField::High.as_str(), "high");
        assert_eq!(MismatchField::Low.as_str(), "low");
        assert_eq!(MismatchField::Close.as_str(), "close");
        assert_eq!(MismatchField::Volume.as_str(), "volume");
    }

    #[test]
    fn test_csv_header_matches_line_column_count() {
        let header_cols = csv_header().split(',').count();
        let line = mismatch_to_csv_line(&sample());
        let line_cols = line.split(',').count();
        assert_eq!(
            header_cols, line_cols,
            "header/line column count must match"
        );
        assert_eq!(header_cols, 8);
    }

    #[test]
    fn test_mismatch_to_csv_line_contains_values() {
        let line = mismatch_to_csv_line(&sample());
        assert!(line.contains("NIFTY"));
        assert!(line.contains("IDX_I"));
        assert!(line.contains("13"));
        assert!(line.contains("open"));
        assert!(line.contains("23342.95"));
        assert!(line.contains("23342.6"));
    }

    #[test]
    fn test_mismatch_to_csv_line_sanitizes_symbol_with_comma() {
        let mut m = sample();
        m.symbol = "BAD,SYM\nBOL\"X".to_string();
        let line = mismatch_to_csv_line(&m);
        // Sanitized symbol must not introduce extra columns or line breaks.
        assert_eq!(
            line.split(',').count(),
            8,
            "comma in symbol must be stripped"
        );
        assert!(!line.contains('\n'));
        assert!(!line.contains('"'));
        assert!(line.contains("BADSYMBOLX"));
    }

    #[test]
    fn test_for_test_writer_disconnected_and_empty() {
        let w = CrossVerify1mAuditWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending(), 0);
    }

    #[test]
    fn test_append_mismatch_fills_buffer() {
        let mut w = CrossVerify1mAuditWriter::for_test();
        w.append_mismatch(&sample()).expect("append");
        assert_eq!(w.pending(), 1);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(text.contains("cross_verify_1m_audit"), "table on wire");
        assert!(text.contains("IDX_I"), "segment on wire");
        assert!(text.contains("open"), "field on wire");
    }

    #[test]
    fn test_two_appends_increment_pending() {
        let mut w = CrossVerify1mAuditWriter::for_test();
        w.append_mismatch(&sample()).expect("a1");
        w.append_mismatch(&sample()).expect("a2");
        assert_eq!(w.pending(), 2);
    }

    #[test]
    fn test_flush_when_disconnected_errors() {
        let mut w = CrossVerify1mAuditWriter::for_test();
        assert!(w.flush().is_err(), "disconnected flush must Err, not panic");
    }
}
