//! Daily end-to-end tick-conservation audit table (TICK-CONSERVE-01,
//! operator directive 2026-06-10: *"Go ahead to achieve zero tick loss"*).
//!
//! At **15:40 IST** each trading day the conservation auditor reconciles the
//! THREE independent tick record stores end-to-end:
//!
//! 1. the **WAL disk log** — every frame Dhan delivered, captured durably at
//!    the WebSocket read instant (`ws_frame_spill`),
//! 2. the **processor outcome counters** — persisted / junk / stale-day /
//!    outside-hours / dedup / parse-error / storage-error,
//! 3. the actual queryable **`ticks` rows in QuestDB**.
//!
//! One row per run is written here, so "was even one tick unaccounted on
//! 2026-06-10?" is a one-row SELECT five years later.
//!
//! ## Audit table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS tick_conservation_audit (
//!     ts                 TIMESTAMP,  -- when the audit ran (IST nanos)
//!     trading_date_ist   TIMESTAMP,  -- the trading day audited (IST midnight)
//!     wal_tick_frames    LONG,       -- tick-class frames Dhan delivered (WAL)
//!     wal_other_frames   LONG,       -- non-tick live-feed frames (WAL)
//!     wal_unattributable LONG,       -- legacy v1 records (no frame_seq)
//!     db_rows            LONG,       -- ticks rows queryable for the day (-1 = unavailable)
//!     processed          LONG,       -- ticks entering the pipeline (since boot)
//!     persisted          LONG,
//!     junk               LONG,
//!     stale_day          LONG,
//!     outside_hours      LONG,
//!     dedup              LONG,
//!     parse_errors       LONG,
//!     storage_errors     LONG,
//!     dropped_total      LONG,       -- DLQ double-fault true-loss counter
//!     delivery_residual  LONG,       -- wal_tick_frames - processed
//!     outcome_residual   LONG,       -- processed - (persisted + filters + storage_errors)
//!     partial_coverage   BOOLEAN,    -- boot after 09:00 IST / a source missing
//!     outcome            SYMBOL      -- balanced / leak / partial
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist);
//! ```
//!
//! The table is per-RUN (not per-instrument), so I-P1-11's
//! `(security_id, exchange_segment)` composite rule is N/A — there is no
//! instrument key. The designated timestamp `ts` is in the DEDUP key per the
//! 2026-04-28 regression rule; including `ts` keeps every re-run of the same
//! trading day as its own forensic row.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name. One row per conservation-audit run.
pub const TICK_CONSERVATION_AUDIT_TABLE: &str = "tick_conservation_audit";

/// DEDUP UPSERT key. Designated timestamp first (2026-04-28 regression rule);
/// `trading_date_ist` second so per-day queries hit the partition cleanly.
/// No `security_id` → I-P1-11 N/A (per-run table, no instrument key).
pub const DEDUP_KEY_TICK_CONSERVATION_AUDIT: &str = "ts, trading_date_ist";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Audit verdict for the `outcome` SYMBOL column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConservationOutcome {
    /// Every delivered tick reached a known outcome AND the counters cover
    /// the full session.
    Balanced,
    /// At least one residual is positive — ticks delivered but unaccounted.
    Leak,
    /// A source was unavailable or the process did not cover the full
    /// session (mid-day boot) — honest "cannot vouch", never a false OK.
    Partial,
}

impl ConservationOutcome {
    /// Stable wire-format label for the `outcome` SYMBOL column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Leak => "leak",
            Self::Partial => "partial",
        }
    }
}

/// One conservation-audit run, ready for ILP write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickConservationRow {
    /// When the audit ran — IST nanoseconds (designated timestamp).
    pub run_ts_ist_nanos: i64,
    /// The trading day audited — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Tick-class frames (Ticker/Quote/Full) found in the WAL for the day.
    pub wal_tick_frames: i64,
    /// Non-tick live-feed frames in the WAL for the day.
    pub wal_other_frames: i64,
    /// Legacy v1 WAL records that cannot be day-attributed.
    pub wal_unattributable: i64,
    /// `ticks` rows queryable for the day. `-1` = QuestDB unavailable.
    pub db_rows: i64,
    /// Processor outcome counters (since boot).
    pub processed: i64,
    pub persisted: i64,
    pub junk: i64,
    pub stale_day: i64,
    pub outside_hours: i64,
    pub dedup: i64,
    pub parse_errors: i64,
    pub storage_errors: i64,
    /// DLQ double-fault true-loss counter (`tv_ticks_dropped_total`).
    pub dropped_total: i64,
    /// `wal_tick_frames - processed` (positive = delivered but unprocessed).
    pub delivery_residual: i64,
    /// `processed - (persisted + junk + stale_day + outside_hours + dedup
    /// + storage_errors)` (positive = in-process leak).
    pub outcome_residual: i64,
    /// `true` when the counters cannot cover the full session.
    pub partial_coverage: bool,
    /// The verdict.
    pub outcome: ConservationOutcome,
}

/// The idempotent `CREATE TABLE` DDL. Pure (testable without QuestDB).
#[must_use]
pub fn tick_conservation_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {TICK_CONSERVATION_AUDIT_TABLE} (\
            ts                 TIMESTAMP, \
            trading_date_ist   TIMESTAMP, \
            wal_tick_frames    LONG, \
            wal_other_frames   LONG, \
            wal_unattributable LONG, \
            db_rows            LONG, \
            processed          LONG, \
            persisted          LONG, \
            junk               LONG, \
            stale_day          LONG, \
            outside_hours      LONG, \
            dedup              LONG, \
            parse_errors       LONG, \
            storage_errors     LONG, \
            dropped_total      LONG, \
            delivery_residual  LONG, \
            outcome_residual   LONG, \
            partial_coverage   BOOLEAN, \
            outcome            SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_TICK_CONSERVATION_AUDIT});"
    )
}

/// Create the audit table if absent (idempotent, schema-self-heal pattern).
/// Failures log at `error!` (Telegram-routable) but do NOT block the audit —
/// the run still reports its numbers via the log/Telegram path.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested via
// tick_conservation_audit_create_ddl tests).
pub async fn ensure_tick_conservation_audit_table(questdb_config: &QuestDbConfig) {
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
                "tick_conservation_audit: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    let ddl = tick_conservation_audit_create_ddl();
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
                "tick_conservation_audit: CREATE TABLE returned non-2xx");
        }
        Err(err) => error!(?err, "tick_conservation_audit: CREATE TABLE request failed"),
    }
}

/// Lazy-connect ILP writer for the `tick_conservation_audit` table. Mirrors
/// `CrossVerify1mAuditWriter`: if QuestDB is unreachable at construction the
/// writer still builds (`sender = None`); `append_row` fills the local buffer
/// and `flush` returns `Err` until QuestDB is reachable.
pub struct TickConservationAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl TickConservationAuditWriter {
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
                    "tick_conservation_audit writer: QuestDB unreachable — buffering locally"
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

    /// Appends one audit row to the ILP buffer (cold path, once/day).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &TickConservationRow) -> Result<()> {
        self.buffer
            .table(TICK_CONSERVATION_AUDIT_TABLE)
            .context("table")?
            .symbol("outcome", r.outcome.as_str())
            .context("outcome")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("wal_tick_frames", r.wal_tick_frames)
            .context("wal_tick_frames")?
            .column_i64("wal_other_frames", r.wal_other_frames)
            .context("wal_other_frames")?
            .column_i64("wal_unattributable", r.wal_unattributable)
            .context("wal_unattributable")?
            .column_i64("db_rows", r.db_rows)
            .context("db_rows")?
            .column_i64("processed", r.processed)
            .context("processed")?
            .column_i64("persisted", r.persisted)
            .context("persisted")?
            .column_i64("junk", r.junk)
            .context("junk")?
            .column_i64("stale_day", r.stale_day)
            .context("stale_day")?
            .column_i64("outside_hours", r.outside_hours)
            .context("outside_hours")?
            .column_i64("dedup", r.dedup)
            .context("dedup")?
            .column_i64("parse_errors", r.parse_errors)
            .context("parse_errors")?
            .column_i64("storage_errors", r.storage_errors)
            .context("storage_errors")?
            .column_i64("dropped_total", r.dropped_total)
            .context("dropped_total")?
            .column_i64("delivery_residual", r.delivery_residual)
            .context("delivery_residual")?
            .column_i64("outcome_residual", r.outcome_residual)
            .context("outcome_residual")?
            .column_bool("partial_coverage", r.partial_coverage)
            .context("partial_coverage")?
            .at(TimestampNanos::new(r.run_ts_ist_nanos))
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
            anyhow::bail!("tick_conservation_audit: no ILP sender (QuestDB unreachable)");
        };
        sender
            .flush(&mut self.buffer)
            .context("tick_conservation_audit ILP flush")?;
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

    fn sample_row() -> TickConservationRow {
        TickConservationRow {
            run_ts_ist_nanos: 1_770_000_000_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            wal_tick_frames: 100_000,
            wal_other_frames: 50,
            wal_unattributable: 0,
            db_rows: 99_000,
            processed: 100_000,
            persisted: 99_000,
            junk: 10,
            stale_day: 200,
            outside_hours: 700,
            dedup: 90,
            parse_errors: 0,
            storage_errors: 0,
            dropped_total: 0,
            delivery_residual: 0,
            outcome_residual: 0,
            partial_coverage: false,
            outcome: ConservationOutcome::Balanced,
        }
    }

    #[test]
    fn test_conservation_ddl_contains_expected_columns() {
        let ddl = tick_conservation_audit_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "wal_tick_frames",
            "wal_other_frames",
            "wal_unattributable",
            "db_rows",
            "processed",
            "persisted",
            "junk",
            "stale_day",
            "outside_hours",
            "dedup",
            "parse_errors",
            "storage_errors",
            "dropped_total",
            "delivery_residual",
            "outcome_residual",
            "partial_coverage",
            "outcome",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
    }

    #[test]
    fn test_conservation_dedup_key_includes_designated_timestamp() {
        // 2026-04-28 regression rule: the designated timestamp MUST be in
        // the DEDUP key. The table is per-run (no instrument key) — pin both.
        assert!(DEDUP_KEY_TICK_CONSERVATION_AUDIT.contains("ts"));
        assert!(DEDUP_KEY_TICK_CONSERVATION_AUDIT.contains("trading_date_ist"));
        let ddl = tick_conservation_audit_create_ddl();
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_TICK_CONSERVATION_AUDIT})"
        )));
    }

    #[test]
    fn test_conservation_outcome_labels_stable() {
        assert_eq!(ConservationOutcome::Balanced.as_str(), "balanced");
        assert_eq!(ConservationOutcome::Leak.as_str(), "leak");
        assert_eq!(ConservationOutcome::Partial.as_str(), "partial");
    }

    #[test]
    fn test_append_row_fills_buffer_disconnected() {
        let mut w = TickConservationAuditWriter::for_test();
        assert!(!w.is_connected());
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 1);
    }

    #[test]
    fn test_flush_when_disconnected_errors() {
        let mut w = TickConservationAuditWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        // Rows stay pending (not lost) for a later retry.
        assert_eq!(w.pending(), 1);
    }

    #[test]
    fn test_flush_empty_is_ok_even_disconnected() {
        let mut w = TickConservationAuditWriter::for_test();
        assert!(w.flush().is_ok(), "empty flush must be a no-op Ok");
    }
}
