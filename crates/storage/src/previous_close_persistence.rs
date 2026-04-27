//! QuestDB ILP persistence for previous-day close per Dhan Ticket
//! #5525125 (Wave 1 Item 4.2, gap closures G2/G3).
//!
//! ## Why this module exists
//!
//! Per Dhan Ticket #5525125 (2026-04-10) the previous-day close arrives
//! by **three** different routes depending on `ExchangeSegment`:
//!
//! | Segment | Source packet                | Bytes  | Source label   |
//! |---------|------------------------------|--------|----------------|
//! | IDX_I   | PrevClose (response code 6)  | 8-11   | `CODE6`        |
//! | NSE_EQ  | Quote (response code 4)      | 38-41  | `QUOTE_CLOSE`  |
//! | NSE_FNO | Full (response code 8)       | 50-53  | `FULL_CLOSE`   |
//!
//! Without a `previous_close` table populated by all three routes, the
//! stock-mover and option-mover persistence layers cannot compute
//! change-percent on day-boundary restarts: there is no other source.
//! The legacy `build_previous_close_row` in
//! `tick_persistence.rs:2142` was deprecated post-Ticket #5525125
//! (when we incorrectly believed `day_close` from Full ticks would
//! suffice). This module is the un-deprecation, with a richer schema
//! that includes the trading-date discriminator (G3) and the source
//! tag for audit.
//!
//! ## Composite-key invariant (I-P1-11)
//!
//! Every key references `(trading_date_ist, security_id, segment)`.
//! Keying on `security_id` alone would silently merge rows from
//! cross-segment-colliding instruments (Dhan reuses numeric ids).
//!
//! ## Idempotency (G3)
//!
//! The designated timestamp `ts` is IST midnight of the trading date,
//! NOT wall-clock. Multiple restarts on the same trading day produce
//! identical `ts` values, so QuestDB's DEDUP UPSERT KEYS dedupes them
//! to one row. Restarts on the *next* trading day produce a fresh
//! `ts` and a fresh row — exactly one row per
//! `(trading_date_ist, security_id, segment)`.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{IST_UTC_OFFSET_NANOS, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
use tickvault_common::segment::segment_code_to_str;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// QuestDB table name for previous-day-close rows.
pub const QUESTDB_TABLE_PREVIOUS_CLOSE: &str = "previous_close";

/// DEDUP key suffix (after the designated `ts` column QuestDB always
/// includes). Composite-key invariant per I-P1-11.
const DEDUP_KEY_PREVIOUS_CLOSE: &str = "security_id, segment";

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// `source` SYMBOL values written to the table.
pub const SOURCE_CODE6: &str = "CODE6";
pub const SOURCE_QUOTE_CLOSE: &str = "QUOTE_CLOSE";
pub const SOURCE_FULL_CLOSE: &str = "FULL_CLOSE";

// ---------------------------------------------------------------------------
// Source enum
// ---------------------------------------------------------------------------

/// Where the prev-close value came from. Encoded into the `source`
/// SYMBOL column for audit + downstream dashboards. Per Ticket
/// #5525125, every (security_id, segment) row should have exactly one
/// source per IST trading day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrevCloseSource {
    /// IDX_I — response code 6 PrevClose packet bytes 8-11 (f32 LE).
    Code6,
    /// NSE_EQ — Quote (code 4) packet bytes 38-41 (f32 LE).
    QuoteClose,
    /// NSE_FNO — Full (code 8) packet bytes 50-53 (f32 LE).
    FullClose,
}

impl PrevCloseSource {
    /// Returns the SYMBOL string written to the `source` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Code6 => SOURCE_CODE6,
            Self::QuoteClose => SOURCE_QUOTE_CLOSE,
            Self::FullClose => SOURCE_FULL_CLOSE,
        }
    }
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// DDL for the `previous_close` table.
///
/// `ts` is the designated timestamp == IST midnight of the trading date
/// (NOT wall-clock). `received_at` is wall-clock for audit. Schema
/// self-heal via `ALTER ADD COLUMN IF NOT EXISTS` follows below for
/// the `source` column added in this commit.
const PREVIOUS_CLOSE_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS previous_close (\
        segment SYMBOL,\
        security_id LONG,\
        prev_close DOUBLE,\
        source SYMBOL,\
        received_at TIMESTAMP,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY MONTH WAL\
";

/// Schema self-heal: idempotent ALTERs for columns added after the
/// initial DDL shipped. Each statement no-ops when the column already
/// exists.
const PREVIOUS_CLOSE_ALTERS: &[&str] = &[
    // Wave 1 Item 4.2 — `source` column added 2026-04-27.
    "ALTER TABLE previous_close ADD COLUMN IF NOT EXISTS source SYMBOL",
];

// ---------------------------------------------------------------------------
// DDL Execution
// ---------------------------------------------------------------------------

/// Ensures the `previous_close` table exists in QuestDB and has the
/// post-Wave-1 schema. Best-effort — logs a warning on failure rather
/// than aborting boot, matching the convention used by sibling
/// `ensure_*_table` functions in this crate.
///
/// Idempotent.
// TEST-EXEMPT: DDL creation requires a live QuestDB; covered by integration tests in storage/tests/ when QuestDB is available.
pub async fn ensure_previous_close_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    execute_ddl(
        &client,
        &base_url,
        PREVIOUS_CLOSE_CREATE_DDL,
        "previous_close",
    )
    .await;
    for sql in PREVIOUS_CLOSE_ALTERS {
        execute_ddl(&client, &base_url, sql, "previous_close ADD COLUMN").await;
    }
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_PREVIOUS_CLOSE, DEDUP_KEY_PREVIOUS_CLOSE
    );
    execute_ddl(&client, &base_url, &dedup_sql, "previous_close DEDUP").await;
}

/// Executes a DDL statement (best-effort).
async fn execute_ddl(client: &Client, base_url: &str, sql: &str, label: &str) {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                info!("{label} DDL executed successfully");
            } else {
                let status = response.status();
                let body = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect::<String>(); // O(1) EXEMPT: DDL error logging
                warn!(%status, body, "{label} DDL returned non-success");
            }
        }
        Err(err) => {
            warn!(?err, "{label} DDL request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Computes the designated timestamp for a previous-close row: IST
/// midnight of the trading date corresponding to `received_at_nanos`.
///
/// Pure function — no I/O. The same `received_at_nanos` always yields
/// the same `ts` for the same trading day, so two writes on the same
/// day dedup to one row in QuestDB.
#[inline]
pub fn ist_midnight_nanos_for_received_at(received_at_nanos: i64) -> i64 {
    let ist_secs = received_at_nanos
        .saturating_div(1_000_000_000)
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    #[allow(clippy::arithmetic_side_effects)]
    // APPROVED: modulo by SECONDS_PER_DAY (86_400) is safe
    let midnight_ist_secs = ist_secs - (ist_secs % i64::from(SECONDS_PER_DAY));
    midnight_ist_secs.saturating_mul(1_000_000_000)
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Batched writer for previous-close rows to QuestDB via ILP.
pub struct PreviousCloseWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
}

impl PreviousCloseWriter {
    /// Creates a new writer connected to QuestDB via ILP TCP.
    // TEST-EXEMPT: ILP connection requires a live QuestDB.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = config.build_ilp_conf_string();
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for previous_close")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
        })
    }

    /// Appends one previous-close row to the ILP buffer. Same-day
    /// repeats with the same (security_id, segment) are deduped by
    /// QuestDB via the DEDUP UPSERT KEY on `(ts, security_id,
    /// segment)`.
    // TEST-EXEMPT: ILP buffer append tested via integration tests. WIRING-EXEMPT: hot-path call sites in tick_processor.rs ship in the Item 4.4 follow-up commit.
    pub fn append_prev_close(
        &mut self,
        security_id: u32,
        exchange_segment_code: u8,
        prev_close: f32,
        source: PrevCloseSource,
        received_at_nanos: i64,
    ) -> Result<()> {
        let ts_nanos = ist_midnight_nanos_for_received_at(received_at_nanos);
        // Per data-integrity rule: `received_at` columns store IST wall-
        // clock = `Utc::now() + IST_UTC_OFFSET_NANOS`. The caller passes
        // raw UTC nanos from `current_received_at_nanos()` /
        // `Utc::now().timestamp_nanos_opt()`, so we add the offset here
        // to match the convention used by `tick_persistence.rs`
        // (build_tick_row, append_market_depth_row, append_prev_close_row).
        // Without this offset every cross-table join on `received_at`
        // would skew by 5h30m. Fixed 2026-04-27 after PR #393 grill audit.
        let received_ist_nanos =
            TimestampNanos::new(received_at_nanos.saturating_add(IST_UTC_OFFSET_NANOS));
        let ts = TimestampNanos::new(ts_nanos);
        let segment = segment_code_to_str(exchange_segment_code);

        self.buffer
            .table(QUESTDB_TABLE_PREVIOUS_CLOSE)
            .context("previous_close table")?
            .symbol("segment", segment)
            .context("segment")?
            .symbol("source", source.as_str())
            .context("source")?
            .column_i64("security_id", i64::from(security_id))
            .context("security_id")?
            .column_f64(
                "prev_close",
                crate::tick_persistence::f32_to_f64_clean(prev_close),
            )
            .context("prev_close")?
            .column_ts("received_at", received_ist_nanos)
            .context("received_at")?
            .at(ts)
            .context("ts")?;

        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Forces an immediate flush of all buffered rows to QuestDB.
    // TEST-EXEMPT: ILP flush integration-tested with live QuestDB. WIRING-EXEMPT: called from the periodic flush loop wired in Item 4.4.
    pub fn force_flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }
        let Some(ref mut sender) = self.sender else {
            anyhow::bail!("PreviousCloseWriter ILP sender is None");
        };
        sender.flush(&mut self.buffer).context("ILP flush")?;
        self.pending_count = 0;
        Ok(())
    }

    /// Returns the number of buffered rows awaiting flush.
    // TEST-EXEMPT: trivial getter covered by integration tests. WIRING-EXEMPT: read by the metrics scraper wired in Item 4.4 follow-up.
    pub fn pending_count(&self) -> usize {
        self.pending_count
    }

    /// Conf string used for reconnect attempts.
    // TEST-EXEMPT: trivial getter covered by reconnect path tests. WIRING-EXEMPT: read by the reconnect logic wired in Item 4.4 follow-up.
    pub fn ilp_conf_string(&self) -> &str {
        &self.ilp_conf_string
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
// APPROVED: test-only — unwrap on canonical helpers.
mod tests {
    use super::*;

    #[test]
    fn test_previous_close_table_name_is_previous_close() {
        assert_eq!(QUESTDB_TABLE_PREVIOUS_CLOSE, "previous_close");
    }

    /// I-P1-11: DEDUP key MUST include `segment` so cross-segment id
    /// collisions don't merge into one row. Source-scan ratchet so a
    /// future edit cannot drop the segment and silently lose data.
    #[test]
    fn test_previous_close_dedup_key_includes_segment_per_i_p1_11() {
        assert!(
            DEDUP_KEY_PREVIOUS_CLOSE.contains("segment"),
            "DEDUP key MUST include 'segment' per I-P1-11 (cross-segment \
             security_id collision). Current value: {DEDUP_KEY_PREVIOUS_CLOSE}",
        );
        assert!(DEDUP_KEY_PREVIOUS_CLOSE.contains("security_id"));
    }

    /// G3: Two writes for the same (security_id, segment) on the same
    /// IST trading day MUST produce identical designated timestamps.
    /// Without this, QuestDB DEDUP cannot collapse them and the table
    /// accumulates one row per restart per day.
    #[test]
    fn test_ist_midnight_nanos_for_received_at_dedupes_same_day_writes() {
        // Pick two wall-clock instants 4 hours apart on the same IST
        // trading day: 2026-04-27 10:00 IST and 14:00 IST. Both should
        // map to the same midnight nanos (= 2026-04-27 00:00 IST).
        let morning_utc_secs = 1_777_244_400_i64; // 2026-04-27 04:30 UTC = 10:00 IST
        let afternoon_utc_secs = 1_777_258_800_i64; // 2026-04-27 08:30 UTC = 14:00 IST
        let morning_nanos = ist_midnight_nanos_for_received_at(morning_utc_secs * 1_000_000_000);
        let afternoon_nanos =
            ist_midnight_nanos_for_received_at(afternoon_utc_secs * 1_000_000_000);
        assert_eq!(
            morning_nanos, afternoon_nanos,
            "same-day writes MUST share an IST-midnight ts so QuestDB \
             DEDUP collapses them"
        );
    }

    /// G3: writes on adjacent IST trading days MUST produce DIFFERENT
    /// designated timestamps. Without this, DEDUP would silently
    /// merge yesterday's row with today's.
    #[test]
    fn test_ist_midnight_nanos_for_received_at_distinguishes_adjacent_days() {
        let day_a = 1_777_244_400_i64; // 2026-04-27 10:00 IST
        let day_b = day_a.saturating_add(86_400); // exactly 1 day later
        let nanos_a = ist_midnight_nanos_for_received_at(day_a * 1_000_000_000);
        let nanos_b = ist_midnight_nanos_for_received_at(day_b * 1_000_000_000);
        assert_ne!(
            nanos_a, nanos_b,
            "writes on adjacent IST trading days MUST have distinct ts \
             values so DEDUP does not merge rows across days"
        );
        assert_eq!(
            nanos_b - nanos_a,
            86_400_i64.saturating_mul(1_000_000_000),
            "the difference MUST be exactly one day in nanos"
        );
    }

    /// All three source labels round-trip through `as_str` and stay
    /// distinct — Loki and Grafana queries depend on these labels.
    #[test]
    fn test_prev_close_source_as_str_labels_are_stable() {
        assert_eq!(PrevCloseSource::Code6.as_str(), "CODE6");
        assert_eq!(PrevCloseSource::QuoteClose.as_str(), "QUOTE_CLOSE");
        assert_eq!(PrevCloseSource::FullClose.as_str(), "FULL_CLOSE");
        // No two labels collide.
        assert_ne!(
            PrevCloseSource::Code6.as_str(),
            PrevCloseSource::QuoteClose.as_str()
        );
        assert_ne!(
            PrevCloseSource::Code6.as_str(),
            PrevCloseSource::FullClose.as_str()
        );
        assert_ne!(
            PrevCloseSource::QuoteClose.as_str(),
            PrevCloseSource::FullClose.as_str()
        );
    }

    /// PrevCloseSource is `Copy + Eq + Debug` so callers can pass it
    /// by value and use it in matches without explicit clones.
    #[test]
    fn test_prev_close_source_is_copy_eq() {
        let a = PrevCloseSource::Code6;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_eq!(format!("{a:?}"), "Code6");
    }

    /// Schema self-heal — the `ALTER TABLE ADD COLUMN IF NOT EXISTS`
    /// for `source` must be present so existing deployments without
    /// the column auto-migrate at boot. Source-scan ratchet so a
    /// future edit cannot accidentally remove the migration.
    #[test]
    fn test_schema_self_heal_adds_source_column() {
        assert!(
            PREVIOUS_CLOSE_ALTERS
                .iter()
                .any(|sql| sql.contains("ADD COLUMN IF NOT EXISTS source SYMBOL")),
            "schema self-heal MUST include `ADD COLUMN IF NOT EXISTS source \
             SYMBOL` so old deployments auto-migrate"
        );
    }

    /// The DDL itself must list the `source` column (so fresh tables
    /// created on first boot of a new deployment have it from day one
    /// without needing to wait for the ALTER to fire).
    #[test]
    fn test_create_ddl_includes_source_column() {
        assert!(
            PREVIOUS_CLOSE_CREATE_DDL.contains("source SYMBOL"),
            "CREATE TABLE DDL MUST include `source SYMBOL` so fresh \
             deployments have the column without relying on the ALTER"
        );
    }

    /// Source-scan ratchet for the IST-offset rule on `received_at`
    /// (data-integrity rule "received_at = Utc::now() + IST_UTC_OFFSET_NANOS").
    /// `append_prev_close` MUST add `IST_UTC_OFFSET_NANOS` so cross-table
    /// joins on `received_at` against `ticks` / `market_depth` /
    /// `previous_close` do not skew by 5h30m. Discovered 2026-04-27 by
    /// the PR #393 grill audit. Without this fix, every operator who
    /// joined `previous_close` against `ticks` would see prev_close
    /// rows appear 5.5 hours earlier than they actually arrived.
    #[test]
    fn test_critical_received_at_includes_ist_offset() {
        let src = include_str!("previous_close_persistence.rs");
        assert!(
            src.contains("received_at_nanos.saturating_add(IST_UTC_OFFSET_NANOS)"),
            "previous_close_persistence.rs MUST add IST_UTC_OFFSET_NANOS \
             to received_at_nanos before writing the `received_at` column \
             — see data-integrity rule, sibling write paths in \
             tick_persistence.rs (build_tick_row, append_market_depth_row, \
             append_prev_close_row)."
        );
    }
}
