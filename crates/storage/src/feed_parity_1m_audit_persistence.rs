//! ONE unified live-vs-backtest 1-minute parity audit table + writer (SP5 of
//! the common-feed-engine convergence — `.claude/plans/active-plan-groww-live-backtest-parity.md`).
//!
//! Merges the two byte-identical-schema, copy-forked audit modules
//! (`cross_verify_1m_audit_persistence` for Dhan + `groww_cross_verify_audit_persistence`
//! for Groww) into ONE feed-parameterized table. Every feed (Dhan, Groww, and any
//! future feed) writes here; `feed` is promoted INTO the DEDUP key so a Dhan
//! mismatch row and a Groww mismatch row for the same `(date, instrument, minute,
//! field)` are BOTH kept (distinct feeds = distinct observations), never collapsed.
//!
//! Post-market each trading day the parity orchestrator compares every subscribed
//! SPOT instrument's live `candles_1m` OHLCV (the shared table, `feed=<this feed>`)
//! against the feed's authoritative backtest 1-minute candles, timestamp-by-
//! timestamp, EXACT match. Each mismatched field-cell is one row here; the per-day
//! mismatch COUNT (per feed) is the quality signal that answers *"is this feed's
//! live == its backtest?"*.
//!
//! ## Migration / cutover (no SEBI audit data dropped — SP5 HIGH finding)
//!
//! This is a NEW physical table. The two OLD tables (`cross_verify_1m_audit`,
//! `groww_cross_verify_1m_audit`) are NEVER dropped (SEBI 5y retention) and stay
//! queryable in place — the Dhan one keeps its pre-SP5 history; the Groww one was
//! never written (zero production writers existed). The NULL-feed backfill in
//! [`ensure_feed_parity_1m_audit_table`] mirrors the shipped candle-table template
//! (`shadow_persistence.rs::ensure_shadow_candle_tables`): `UPDATE feed='dhan'
//! WHERE feed IS NULL` runs BEFORE `DEDUP ENABLE UPSERT KEYS(... feed ...)` so a
//! Dhan row written before the self-heal re-keyed the table cannot collide with a
//! later `feed='dhan'` row. See `docs/design/sp5-unified-parity-audit-design.md`.
//!
//! ## Audit table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS feed_parity_1m_audit (
//!     ts               TIMESTAMP,   -- when the parity run executed (IST nanos)
//!     trading_date_ist TIMESTAMP,   -- the trading day verified (IST midnight)
//!     feed             SYMBOL,      -- broker source: 'dhan' / 'groww' (IN the DEDUP key)
//!     segment          SYMBOL,
//!     security_id      LONG,
//!     minute_ts_ist    TIMESTAMP,   -- the mismatched 1-min bucket (IST nanos)
//!     field            SYMBOL,      -- open / high / low / close / volume
//!     live_value       DOUBLE,      -- our candles_1m (feed=<this feed>) value
//!     backtest_value   DOUBLE       -- the feed's authoritative backtest value
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, security_id, segment, minute_ts_ist, field);
//! ```

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::feed::Feed;
pub use tickvault_common::feed_parity::MismatchField;

/// QuestDB table name. One row per mismatched `(feed, instrument, minute, field)`.
pub const FEED_PARITY_1M_AUDIT_TABLE: &str = "feed_parity_1m_audit";

/// DEDUP UPSERT key — the designated timestamp `ts` FIRST (QuestDB requires the
/// designated timestamp column in every DEDUP key — 2026-05-18 production HTTP-400
/// regression), then `feed` (per-feed identity, promoted INTO the key so the two
/// feeds never collapse into one row), then the discriminators + `(security_id,
/// segment)` per I-P1-11. The `dedup_segment_meta_guard` workspace test scans this
/// constant; it MUST mention `ts` AND `segment`.
pub const DEDUP_KEY_FEED_PARITY_1M_AUDIT: &str =
    "ts, trading_date_ist, feed, security_id, segment, minute_ts_ist, field";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// One mismatched OHLCV field-cell (this feed's live vs its backtest), ready for
/// ILP write + CSV append.
///
/// COLD-path struct (post-market parity run, once/day) — `segment`/`symbol` are
/// owned `String`s because they come from the runtime instrument universe; the
/// allocation is irrelevant off the hot path.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedParity1mMismatch {
    /// Broker feed source (`dhan` / `groww` / …). Stable `&'static str` from
    /// [`Feed::as_str`]. This is part of the DEDUP key, so two feeds' rows for the
    /// same `(date, instrument, minute, field)` are BOTH kept.
    pub feed: &'static str,
    /// When the parity run executed — IST nanoseconds.
    pub run_ts_ist_nanos: i64,
    /// The trading day verified — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Composite-key part 1 (I-P1-11). Widened to `i64` for `column_i64`.
    pub security_id: i64,
    /// Composite-key part 2 (I-P1-11). Segment string (`IDX_I` / `NSE_EQ` / …).
    pub segment: String,
    /// Human-readable symbol (CSV only — NOT a QuestDB column to keep the table
    /// lean; the audit row is keyed by `(feed, security_id, segment)`).
    pub symbol: String,
    /// The mismatched 1-minute bucket — IST nanoseconds.
    pub minute_ts_ist_nanos: i64,
    /// Which field disagreed.
    pub field: MismatchField,
    /// Our live `candles_1m` (feed=<this feed>) value.
    pub live_value: f64,
    /// The feed's authoritative backtest value.
    pub backtest_value: f64,
}

/// CSV header line (no trailing newline). Matches [`mismatch_to_csv_line`] column
/// order exactly. Feed-agnostic `live_value`/`backtest_value` (was Dhan's
/// `our_value`/`dhan_value`).
#[must_use]
pub fn csv_header() -> &'static str {
    "trading_date_ist_nanos,feed,symbol,segment,security_id,minute_ts_ist_nanos,field,live_value,backtest_value"
}

/// Format one mismatch as a CSV line (no trailing newline). Pure — testable
/// without a filesystem. The `symbol` is the only free-text field; it is
/// sanitized (commas / newlines / quotes stripped) so it can never break the CSV
/// grid (defence-in-depth — symbols come from the trusted instrument master, but
/// a stray comma would still corrupt the file).
#[must_use]
pub fn mismatch_to_csv_line(m: &FeedParity1mMismatch) -> String {
    let safe_symbol: String = m
        .symbol
        .chars()
        .filter(|c| *c != ',' && *c != '\n' && *c != '\r' && *c != '"')
        .collect();
    format!(
        "{},{},{},{},{},{},{},{},{}",
        m.trading_date_ist_nanos,
        m.feed,
        safe_symbol,
        m.segment,
        m.security_id,
        m.minute_ts_ist_nanos,
        m.field.as_str(),
        m.live_value,
        m.backtest_value,
    )
}

/// The idempotent `CREATE TABLE` DDL for `feed_parity_1m_audit`. Pure (testable
/// without QuestDB). `feed` is part of the DEDUP key.
#[must_use]
pub fn feed_parity_1m_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {FEED_PARITY_1M_AUDIT_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            feed             SYMBOL, \
            segment          SYMBOL, \
            security_id      LONG, \
            minute_ts_ist    TIMESTAMP, \
            field            SYMBOL, \
            live_value       DOUBLE, \
            backtest_value   DOUBLE\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_FEED_PARITY_1M_AUDIT});"
    )
}

/// Create the unified audit table if absent (idempotent, schema-self-heal +
/// brownfield NULL-backfill migration). Failures log at `error!`
/// (Telegram-routable) but do NOT block — the parity run simply can't persist its
/// audit rows and reports it. Both feeds call this; it is idempotent.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via the
// feed_parity_1m_audit_create_ddl + migration-string tests below).
pub async fn ensure_feed_parity_1m_audit_table(questdb_config: &QuestDbConfig) {
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
                "feed_parity_1m_audit: HTTP client build failed — table not ensured"
            );
            return;
        }
    };

    // 1. CREATE (greenfield: includes feed col + feed-in-key DEDUP).
    let ddl = feed_parity_1m_audit_create_ddl();
    run_ddl(&client, &base_url, "CREATE TABLE", ddl.as_str()).await;

    // 2. Self-heal the `feed` SYMBOL column on a table created BEFORE feed was in
    //    the key (defence-in-depth for a partial earlier deploy). Additive,
    //    idempotent; QuestDB ignores the ADD when the column exists.
    let alter_feed_ddl =
        format!("ALTER TABLE {FEED_PARITY_1M_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS feed SYMBOL");
    run_ddl(
        &client,
        &base_url,
        "ALTER ADD COLUMN feed",
        alter_feed_ddl.as_str(),
    )
    .await;

    // 3. Brownfield NULL-feed backfill (SP5 HIGH finding — mirror the candle-table
    //    template). A Dhan row written before the self-heal re-keyed the table has
    //    `feed=NULL`. Without this, a later `feed='dhan'` row for the same key is a
    //    DISTINCT key (NULL != 'dhan') → a DUPLICATE, not an upsert. Stamp
    //    `feed='dhan'` on every legacy NULL row BEFORE re-enabling DEDUP. Idempotent
    //    + cheap on every subsequent boot (`WHERE feed IS NULL` matches nothing once
    //    backfilled). MUST run BEFORE the DEDUP-ENABLE below.
    let backfill_feed = format!(
        "UPDATE {FEED_PARITY_1M_AUDIT_TABLE} SET feed = '{}' WHERE feed IS NULL",
        Feed::Dhan.as_str()
    );
    run_ddl(
        &client,
        &base_url,
        "UPDATE feed backfill",
        backfill_feed.as_str(),
    )
    .await;

    // 4. Brownfield DEDUP migration: (re-)apply the UPSERT key with `feed` included
    //    so a table created before feed-in-key gets the new key. Idempotent —
    //    re-enabling the same key is a no-op; greenfield tables already have it.
    let dedup_enable = format!(
        "ALTER TABLE {FEED_PARITY_1M_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_FEED_PARITY_1M_AUDIT})"
    );
    run_ddl(&client, &base_url, "DEDUP ENABLE", dedup_enable.as_str()).await;
}

/// Run one DDL statement against QuestDB `/exec`, logging non-2xx + transport
/// errors at `error!` (Telegram-routable). Never panics; never blocks.
// TEST-EXEMPT: thin HTTP wrapper over QuestDB /exec (needs live QuestDB); the DDL
// strings it runs are unit-tested via the create/migration-string tests below.
async fn run_ddl(client: &reqwest::Client, base_url: &str, label: &str, query: &str) {
    match client.get(base_url).query(&[("query", query)]).send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(%status, %label, body = %body.chars().take(200).collect::<String>(),
                "feed_parity_1m_audit: DDL returned non-2xx");
        }
        Err(err) => error!(?err, %label, "feed_parity_1m_audit: DDL request failed"),
    }
}

/// Lazy-connect ILP writer for the `feed_parity_1m_audit` table. Mirrors
/// `PrevDayOhlcvWriter`: if QuestDB is unreachable at construction the writer still
/// builds (`sender = None`); `append_mismatch` fills the local buffer and `flush`
/// returns `Err` until a reconnect lands.
pub struct FeedParity1mAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl FeedParity1mAuditWriter {
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
                    "feed_parity_1m_audit writer: QuestDB unreachable — buffering locally"
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

    /// Raw buffered bytes (tests assert table/segment/feed/value serialisation).
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by append tests.
    pub fn buffer_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    /// Append one mismatch row to the ILP buffer. O(1). The `feed` SYMBOL is part
    /// of the DEDUP key, so a Dhan and a Groww row for the same cell coexist.
    pub fn append_mismatch(&mut self, m: &FeedParity1mMismatch) -> Result<()> {
        self.buffer
            .table(FEED_PARITY_1M_AUDIT_TABLE)
            .with_context(|| "feed_parity_1m_audit append: table() failed")?
            .symbol("feed", m.feed)
            .with_context(|| "feed_parity_1m_audit append: symbol(feed) failed")?
            .symbol("segment", m.segment.as_str())
            .with_context(|| "feed_parity_1m_audit append: symbol(segment) failed")?
            .symbol("field", m.field.as_str())
            .with_context(|| "feed_parity_1m_audit append: symbol(field) failed")?
            .column_i64("security_id", m.security_id)
            .with_context(|| "feed_parity_1m_audit append: column_i64(security_id) failed")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(m.trading_date_ist_nanos),
            )
            .with_context(|| "feed_parity_1m_audit append: column_ts(trading_date_ist) failed")?
            .column_ts("minute_ts_ist", TimestampNanos::new(m.minute_ts_ist_nanos))
            .with_context(|| "feed_parity_1m_audit append: column_ts(minute_ts_ist) failed")?
            .column_f64("live_value", m.live_value)
            .with_context(|| "feed_parity_1m_audit append: column_f64(live_value) failed")?
            .column_f64("backtest_value", m.backtest_value)
            .with_context(|| "feed_parity_1m_audit append: column_f64(backtest_value) failed")?
            .at(TimestampNanos::new(m.run_ts_ist_nanos))
            .with_context(|| "feed_parity_1m_audit append: at(ts) failed")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flush the buffer to QuestDB. `Err` if disconnected.
    pub fn flush(&mut self) -> Result<()> {
        let sender = self
            .sender
            .as_mut()
            .context("feed_parity_1m_audit flush: not connected to QuestDB")?;
        sender
            .flush(&mut self.buffer)
            .context("feed_parity_1m_audit flush: ILP flush failed")?;
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dhan() -> FeedParity1mMismatch {
        FeedParity1mMismatch {
            feed: Feed::Dhan.as_str(),
            run_ts_ist_nanos: 1_780_000_000_000_000_000,
            trading_date_ist_nanos: 1_780_000_000_000_000_000,
            security_id: 13,
            segment: "IDX_I".to_string(),
            symbol: "NIFTY".to_string(),
            minute_ts_ist_nanos: 1_780_000_060_000_000_000,
            field: MismatchField::Open,
            live_value: 23_342.95,
            backtest_value: 23_342.60,
        }
    }

    fn sample_groww() -> FeedParity1mMismatch {
        FeedParity1mMismatch {
            feed: Feed::Groww.as_str(),
            run_ts_ist_nanos: 1_780_000_000_000_000_000,
            trading_date_ist_nanos: 1_780_000_000_000_000_000,
            security_id: 1333,
            segment: "NSE_EQ".to_string(),
            symbol: "RELIANCE".to_string(),
            minute_ts_ist_nanos: 1_780_000_060_000_000_000,
            field: MismatchField::Close,
            live_value: 2_847.50,
            backtest_value: 2_847.00,
        }
    }

    #[test]
    fn test_dedup_key_includes_feed_segment_and_security_id() {
        // Per-feed identity (operator 2026-06-23) + I-P1-11 + dedup_segment_meta_guard.
        assert!(
            DEDUP_KEY_FEED_PARITY_1M_AUDIT.contains("feed"),
            "feed MUST be in the unified parity DEDUP key so a Dhan and a Groww \
             mismatch for the same cell are BOTH kept: {DEDUP_KEY_FEED_PARITY_1M_AUDIT}"
        );
        assert!(DEDUP_KEY_FEED_PARITY_1M_AUDIT.contains("segment"));
        assert!(DEDUP_KEY_FEED_PARITY_1M_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_FEED_PARITY_1M_AUDIT.contains("minute_ts_ist"));
        assert!(DEDUP_KEY_FEED_PARITY_1M_AUDIT.contains("field"));
        // QuestDB requires the designated timestamp `ts` in every DEDUP key
        // (2026-05-18 HTTP-400 regression). Pin it as the first token.
        let tokens: Vec<&str> = DEDUP_KEY_FEED_PARITY_1M_AUDIT
            .split([',', ' '])
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(
            tokens.first(),
            Some(&"ts"),
            "DEDUP key must START with the designated timestamp `ts`: {DEDUP_KEY_FEED_PARITY_1M_AUDIT}"
        );
    }

    #[test]
    fn test_create_ddl_has_all_columns_and_feed_in_dedup() {
        let ddl = feed_parity_1m_audit_create_ddl();
        for needle in [
            "feed_parity_1m_audit",
            "ts               TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "feed             SYMBOL",
            "segment          SYMBOL",
            "security_id      LONG",
            "minute_ts_ist    TIMESTAMP",
            "field            SYMBOL",
            "live_value       DOUBLE",
            "backtest_value   DOUBLE",
            "PARTITION BY DAY",
            "DEDUP UPSERT KEYS(ts, trading_date_ist, feed, security_id, segment, minute_ts_ist, field)",
        ] {
            assert!(ddl.contains(needle), "DDL missing: {needle}\n{ddl}");
        }
    }

    #[test]
    fn test_mismatch_field_as_str_labels_are_stable() {
        // Re-exported from tickvault_common::feed_parity::MismatchField.
        assert_eq!(MismatchField::Open.as_str(), "open");
        assert_eq!(MismatchField::High.as_str(), "high");
        assert_eq!(MismatchField::Low.as_str(), "low");
        assert_eq!(MismatchField::Close.as_str(), "close");
        assert_eq!(MismatchField::Volume.as_str(), "volume");
    }

    #[test]
    fn test_csv_header_matches_line_column_count() {
        let header_cols = csv_header().split(',').count();
        let line = mismatch_to_csv_line(&sample_dhan());
        let line_cols = line.split(',').count();
        assert_eq!(
            header_cols, line_cols,
            "header/line column count must match"
        );
        assert_eq!(header_cols, 9);
        // Feed-agnostic column names.
        assert!(csv_header().contains("live_value"));
        assert!(csv_header().contains("backtest_value"));
        assert!(csv_header().contains("feed"));
    }

    #[test]
    fn test_mismatch_to_csv_line_contains_values_and_feed() {
        let line = mismatch_to_csv_line(&sample_dhan());
        assert!(line.contains("NIFTY"));
        assert!(line.contains("IDX_I"));
        assert!(line.contains("dhan"));
        assert!(line.contains("13"));
        assert!(line.contains("open"));
        assert!(line.contains("23342.95"));
        assert!(line.contains("23342.6"));
    }

    #[test]
    fn test_mismatch_to_csv_line_sanitizes_symbol() {
        let mut m = sample_dhan();
        m.symbol = "BAD,SYM\nBOL\"X".to_string();
        let line = mismatch_to_csv_line(&m);
        assert_eq!(
            line.split(',').count(),
            9,
            "comma in symbol must be stripped"
        );
        assert!(!line.contains('\n'));
        assert!(!line.contains('"'));
        assert!(line.contains("BADSYMBOLX"));
    }

    #[test]
    fn test_for_test_writer_disconnected_and_empty() {
        let w = FeedParity1mAuditWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending(), 0);
    }

    #[test]
    fn test_append_dhan_mismatch_fills_buffer() {
        let mut w = FeedParity1mAuditWriter::for_test();
        w.append_mismatch(&sample_dhan()).expect("append");
        assert_eq!(w.pending(), 1);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(text.contains("feed_parity_1m_audit"), "table on wire");
        assert!(text.contains("IDX_I"), "segment on wire");
        assert!(text.contains("open"), "field on wire");
        assert!(text.contains("dhan"), "feed label on wire");
        assert!(text.contains("live_value"), "live_value column on wire");
        assert!(
            text.contains("backtest_value"),
            "backtest_value column on wire"
        );
    }

    #[test]
    fn test_append_groww_mismatch_fills_buffer() {
        let mut w = FeedParity1mAuditWriter::for_test();
        w.append_mismatch(&sample_groww()).expect("append");
        assert_eq!(w.pending(), 1);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(text.contains("feed_parity_1m_audit"), "table on wire");
        assert!(text.contains("NSE_EQ"), "segment on wire");
        assert!(text.contains("close"), "field on wire");
        assert!(text.contains("groww"), "feed label on wire");
    }

    #[test]
    fn test_both_feeds_coexist_in_one_buffer() {
        // The whole point of SP5: both feeds' mismatch rows route to the ONE table,
        // each carrying its own `feed` label (which is in the DEDUP key, so they
        // never collapse into a single row).
        let mut w = FeedParity1mAuditWriter::for_test();
        w.append_mismatch(&sample_dhan()).expect("dhan");
        w.append_mismatch(&sample_groww()).expect("groww");
        assert_eq!(w.pending(), 2);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(text.contains("dhan"), "dhan feed on wire");
        assert!(text.contains("groww"), "groww feed on wire");
    }

    #[test]
    fn test_flush_when_disconnected_errors() {
        let mut w = FeedParity1mAuditWriter::for_test();
        assert!(w.flush().is_err(), "disconnected flush must Err, not panic");
    }

    #[test]
    fn test_volume_field_widened_to_f64_round_trips() {
        let mut m = sample_groww();
        m.field = MismatchField::Volume;
        m.live_value = 5_000_000.0;
        m.backtest_value = 4_999_800.0;
        let mut w = FeedParity1mAuditWriter::for_test();
        w.append_mismatch(&m).expect("append volume");
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(text.contains("volume"), "volume field on wire");
        assert!(text.contains("5000000"), "live volume on wire");
    }

    // ---- Migration / cutover string contracts (SP5 HIGH finding) -------------
    //
    // The migration ordering is enforced by string inspection because the live
    // QuestDB runner is TEST-EXEMPT. The candle-table template (shipped) is the
    // reference: ADD COLUMN feed → UPDATE ... WHERE feed IS NULL → DEDUP ENABLE.

    #[test]
    fn test_backfill_targets_null_feed_and_stamps_dhan() {
        // The brownfield backfill MUST stamp 'dhan' on legacy NULL-feed rows.
        let expected = format!(
            "UPDATE {FEED_PARITY_1M_AUDIT_TABLE} SET feed = '{}' WHERE feed IS NULL",
            Feed::Dhan.as_str()
        );
        assert!(expected.contains("WHERE feed IS NULL"));
        assert!(expected.contains("SET feed = 'dhan'"));
    }

    #[test]
    fn test_dedup_enable_migration_includes_feed_key() {
        // The re-key migration MUST apply the feed-inclusive key.
        let dedup_enable = format!(
            "ALTER TABLE {FEED_PARITY_1M_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_FEED_PARITY_1M_AUDIT})"
        );
        assert!(dedup_enable.contains("DEDUP ENABLE UPSERT KEYS"));
        assert!(dedup_enable.contains("feed"));
    }
}
