//! `cross_fill_audit` table — per-event forensics for the cadence
//! cross-fill / fallback rungs (operator directive 2026-07-20: every
//! cross-fill "highlighted, logged, monitored, audited, visualised — every
//! day, week, month — precisely at what time it is happening").
//!
//! ONE row per (cycle minute, borrowing lane, stage) event: a lane that
//! exhausted its own path and borrowed the OTHER broker's same-cycle data
//! (`stage='cross_fill'`), or the Groww verdict launching its own late
//! refetch fallback (`stage='groww_fallback'`). Written best-effort,
//! fire-and-forget from the app-side consumer
//! (`crates/app/src/cross_fill_visibility.rs`) — NEVER on the cadence
//! decision path; a write failure logs CADENCE-04 and the coalesced
//! CADENCE-01 log + counters still carry the event.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS cross_fill_audit (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP, lane SYMBOL,
//!     source_lane SYMBOL, stage SYMBOL, cycle_minute_ist INT, legs SYMBOL,
//!     rows_filled INT, cycle_latency_ms LONG, ladder_rung INT,
//!     resolution SYMBOL, retry_attempts INT, resolved_at_ms_after_close LONG
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, lane, cycle_minute_ist, stage);
//! ```
//!
//! DEDUP discipline: designated `ts` FIRST (2026-04-28 rule); the key is the
//! operator-specified `(trading_date_ist + ts + lane + cycle_minute_ist +
//! stage)` — `stage` in-key so a cross_fill row and a groww_fallback row for
//! the same minute BOTH survive (phase-0 DEDUP rule 3); a replay of the same
//! (minute, lane, stage) UPSERTs in place (idempotent). No `security_id`
//! column exists — `lane`/`source_lane` identify broker LANES, not
//! instruments, so I-P1-11 does not apply to this table.
//!
//! `resolution` values (stable SYMBOL wire strings):
//! - `cross_fill` (default) — the lane resolved by borrowing the other
//!   broker's data; `resolved_at_ms_after_close` is measured.
//! - `native_late_retry` — the lane's OWN late refetch (today: the Groww
//!   verdict fallback launch; `resolved_at_ms_after_close = -1` — the
//!   resolution instant is unknown at launch time). The T+4s retry
//!   session's PR stamps `native_first_try` / `native_late_retry` with
//!   measured values when it lands.
//! - `native_first_try` — reserved for the T+4s session (unused today).

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::warn;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;

/// QuestDB table name — one row per (cycle minute, lane, stage) event.
pub const CROSS_FILL_AUDIT_TABLE: &str = "cross_fill_audit";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule); `stage`
/// in-key so same-minute cross_fill + groww_fallback rows BOTH survive.
pub const DEDUP_KEY_CROSS_FILL_AUDIT: &str = "ts, trading_date_ist, lane, cycle_minute_ist, stage";

/// `stage` SYMBOL — a lane borrowed the OTHER broker's same-cycle data.
pub const CROSS_FILL_STAGE_CROSS_FILL: &str = "cross_fill";

/// `stage` SYMBOL — the Groww verdict launched its own late refetch.
pub const CROSS_FILL_STAGE_GROWW_FALLBACK: &str = "groww_fallback";

/// `resolution` SYMBOL — resolved by borrowing the other broker's data.
pub const CROSS_FILL_RESOLUTION_CROSS_FILL: &str = "cross_fill";

/// `resolution` SYMBOL — the lane's own late refetch (fallback launch).
pub const CROSS_FILL_RESOLUTION_NATIVE_LATE_RETRY: &str = "native_late_retry";

/// `resolution` SYMBOL — reserved for the T+4s retry session's PR.
pub const CROSS_FILL_RESOLUTION_NATIVE_FIRST_TRY: &str = "native_first_try";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// One cross-fill/fallback forensics row, ready for ILP write. Every string
/// field is a BOUNDED static slug — never raw external text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossFillAuditRow {
    /// Designated timestamp — the decided minute's OPEN, IST nanoseconds
    /// (joins `spot_1m_rest.ts` / `rest_fetch_audit.ts` per minute).
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// The DEGRADED (borrowing) broker lane (`dhan` / `groww`).
    pub lane: &'static str,
    /// Where the data came from (`groww` / `dhan`; for a fallback launch
    /// this equals `lane` — the lane retried its OWN source).
    pub source_lane: &'static str,
    /// `cross_fill` / `groww_fallback`.
    pub stage: &'static str,
    /// The decided minute's OPEN, IST seconds-of-day (in the DEDUP key).
    pub cycle_minute_ist: i64,
    /// Which legs were involved: `spot` / `chain` / `spot+chain`.
    pub legs: &'static str,
    /// How many cells filled (cross_fill) or legs queued (fallback).
    pub rows_filled: i64,
    /// Minute-close boundary T → this event's emit instant, in ms.
    pub cycle_latency_ms: i64,
    /// The borrowing lane's shape-ladder rung at this cycle's slot build
    /// (Dhan `dhan_shape` / Groww `groww_shape`).
    pub ladder_rung: i64,
    /// `cross_fill` / `native_late_retry` (see module docs).
    pub resolution: &'static str,
    /// In-cycle retry attempts consumed before this event (0 today; the
    /// T+4s retry session's PR stamps measured values).
    pub retry_attempts: i64,
    /// Minute-close T → resolution instant, ms; `-1` = not yet known at
    /// emit time (fallback launch rows) — never a fabricated latency.
    pub resolved_at_ms_after_close: i64,
}

/// The idempotent `CREATE TABLE` DDL for `cross_fill_audit`. Pure.
#[must_use]
pub fn cross_fill_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {CROSS_FILL_AUDIT_TABLE} (\
            ts                         TIMESTAMP, \
            trading_date_ist           TIMESTAMP, \
            lane                       SYMBOL, \
            source_lane                SYMBOL, \
            stage                      SYMBOL, \
            cycle_minute_ist           INT, \
            legs                       SYMBOL, \
            rows_filled                INT, \
            cycle_latency_ms           LONG, \
            ladder_rung                INT, \
            resolution                 SYMBOL, \
            retry_attempts             INT, \
            resolved_at_ms_after_close LONG\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_CROSS_FILL_AUDIT});"
    )
}

/// Create the `cross_fill_audit` table if absent (idempotent self-heal
/// order: CREATE → per-column `ALTER ADD COLUMN IF NOT EXISTS` → DEDUP
/// ENABLE — the house template). Failures log at `error!` (CADENCE-04,
/// stage `audit_ensure_*`) but never block; NOTE the HTTP-CLIENT-01-class
/// consequence: a failed ensure leaves the table to be auto-created by the
/// first ILP write WITHOUT dedup (a duplicate-row window until a later
/// ensure succeeds).
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via the create_ddl test; the runner mirrors ensure_rest_fetch_audit_table byte-for-byte apart from names)
pub async fn ensure_cross_fill_audit_table(questdb_config: &QuestDbConfig) {
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
            metrics::counter!("tv_cross_fill_audit_write_errors_total", "stage" => "audit_ensure_client_build")
                .increment(1);
            tracing::error!(
                code = ErrorCode::Cadence04AuditWriteFailed.code_str(),
                stage = "audit_ensure_client_build",
                ?err,
                "CADENCE-04: HTTP client build failed — cross_fill_audit table \
                 not ensured (first ILP write may auto-create it WITHOUT dedup \
                 — duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![cross_fill_audit_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern).
    for (col, ty) in [
        ("trading_date_ist", "TIMESTAMP"),
        ("lane", "SYMBOL"),
        ("source_lane", "SYMBOL"),
        ("stage", "SYMBOL"),
        ("cycle_minute_ist", "INT"),
        ("legs", "SYMBOL"),
        ("rows_filled", "INT"),
        ("cycle_latency_ms", "LONG"),
        ("ladder_rung", "INT"),
        ("resolution", "SYMBOL"),
        ("retry_attempts", "INT"),
        ("resolved_at_ms_after_close", "LONG"),
    ] {
        statements.push(format!(
            "ALTER TABLE {CROSS_FILL_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {CROSS_FILL_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_CROSS_FILL_AUDIT});"
    ));
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
                metrics::counter!("tv_cross_fill_audit_write_errors_total", "stage" => "audit_ensure_ddl")
                    .increment(1);
                tracing::error!(code = ErrorCode::Cadence04AuditWriteFailed.code_str(),
                    stage = "audit_ensure_ddl",
                    %status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "CADENCE-04: cross_fill_audit DDL returned non-2xx (dedup may \
                     be missing — duplicate-row window until a later ensure succeeds)");
            }
            Err(err) => {
                metrics::counter!("tv_cross_fill_audit_write_errors_total", "stage" => "audit_ensure_ddl")
                    .increment(1);
                tracing::error!(
                    code = ErrorCode::Cadence04AuditWriteFailed.code_str(),
                    stage = "audit_ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "CADENCE-04: cross_fill_audit DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf — per-flush server ACK (the 2026-07-05
/// fire-and-forget lesson) with the shadow-candle-writer knobs.
fn cross_fill_audit_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `cross_fill_audit`. Same contract as
/// `RestFetchAuditWriter`: unreachable QuestDB at construction still builds
/// (rows buffer locally); a failed flush DISCARDS the pending buffer
/// (poisoned-buffer defense) — the rows are forensics, best-effort, and the
/// discard is counted + coded, never silent.
pub struct CrossFillAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl CrossFillAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract mirrors RestFetchAuditWriter::new); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = cross_fill_audit_ilp_http_conf(config);
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
                    "cross_fill_audit writer: QuestDB unreachable — buffering locally"
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

    /// Test-only view of the ILP buffer bytes (shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one cross-fill forensics row (cold path, a handful/day).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &CrossFillAuditRow) -> Result<()> {
        self.buffer
            .table(CROSS_FILL_AUDIT_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("lane", r.lane)
            .context("lane")?
            .symbol("source_lane", r.source_lane)
            .context("source_lane")?
            .symbol("stage", r.stage)
            .context("stage")?
            .symbol("legs", r.legs)
            .context("legs")?
            .symbol("resolution", r.resolution)
            .context("resolution")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("cycle_minute_ist", r.cycle_minute_ist)
            .context("cycle_minute_ist")?
            .column_i64("rows_filled", r.rows_filled)
            .context("rows_filled")?
            .column_i64("cycle_latency_ms", r.cycle_latency_ms)
            .context("cycle_latency_ms")?
            .column_i64("ladder_rung", r.ladder_rung)
            .context("ladder_rung")?
            .column_i64("retry_attempts", r.retry_attempts)
            .context("retry_attempts")?
            .column_i64("resolved_at_ms_after_close", r.resolved_at_ms_after_close)
            .context("resolved_at_ms_after_close")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK). On ANY
    /// failed flush the pending buffer is DISCARDED (poisoned-buffer
    /// defense) — the rows are best-effort forensics; the discard is
    /// counted (`tv_cross_fill_audit_rows_discarded_total`), never silent.
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (pending discarded).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        if self.sender.is_none() {
            let dropped = self.discard_pending();
            anyhow::bail!(
                "cross_fill_audit: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending forensics row(s) discarded (best-effort)"
            );
        }
        let flushed = self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer));
        match flushed {
            Some(Ok(())) => {
                self.pending = 0;
                Ok(())
            }
            Some(Err(err)) => {
                let dropped = self.discard_pending();
                Err(anyhow::Error::new(err).context(format!(
                    "cross_fill_audit ILP flush failed — {dropped} pending \
                     forensics row(s) discarded (poisoned-buffer defense)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("cross_fill_audit: ILP sender vanished — {dropped} row(s) discarded");
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted so a discard is never silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_cross_fill_audit_rows_discarded_total").increment(dropped as u64);
        }
        self.buffer.clear();
        self.pending = 0;
        dropped
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> CrossFillAuditRow {
        CrossFillAuditRow {
            ts_ist_nanos: 1_770_000_900_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            lane: "dhan",
            source_lane: "groww",
            stage: CROSS_FILL_STAGE_CROSS_FILL,
            cycle_minute_ist: 33_360,
            legs: "spot",
            rows_filled: 1,
            cycle_latency_ms: 12_500,
            ladder_rung: 0,
            resolution: CROSS_FILL_RESOLUTION_CROSS_FILL,
            retry_attempts: 0,
            resolved_at_ms_after_close: 12_500,
        }
    }

    #[test]
    fn test_cross_fill_audit_create_ddl_shape() {
        let ddl = cross_fill_audit_create_ddl();
        // Every operator-specified column present.
        for col in [
            "ts ",
            "trading_date_ist",
            "lane",
            "source_lane",
            "stage",
            "cycle_minute_ist",
            "legs",
            "rows_filled",
            "cycle_latency_ms",
            "ladder_rung",
            "resolution",
            "retry_attempts",
            "resolved_at_ms_after_close",
        ] {
            assert!(ddl.contains(col), "DDL must carry column {col}: {ddl}");
        }
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(
            ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_CROSS_FILL_AUDIT})")),
            "DEDUP key clause present"
        );
        // Designated ts FIRST in the DEDUP key (2026-04-28 rule).
        assert!(DEDUP_KEY_CROSS_FILL_AUDIT.starts_with("ts,"));
        // Stage in-key so cross_fill + groww_fallback rows both survive.
        assert!(DEDUP_KEY_CROSS_FILL_AUDIT.contains("stage"));
        assert!(DEDUP_KEY_CROSS_FILL_AUDIT.contains("cycle_minute_ist"));
        assert!(DEDUP_KEY_CROSS_FILL_AUDIT.contains("lane"));
        assert!(DEDUP_KEY_CROSS_FILL_AUDIT.contains("trading_date_ist"));
    }

    #[test]
    fn test_cross_fill_audit_append_row_buffers_symbols_and_columns() {
        let mut w = CrossFillAuditWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        assert_eq!(w.pending(), 1);
        let buf = w.buffer_utf8();
        assert!(buf.starts_with(CROSS_FILL_AUDIT_TABLE), "table name first");
        for frag in [
            "lane=dhan",
            "source_lane=groww",
            "stage=cross_fill",
            "legs=spot",
            "resolution=cross_fill",
            "cycle_minute_ist=33360i",
            "rows_filled=1i",
            "cycle_latency_ms=12500i",
            "ladder_rung=0i",
            "retry_attempts=0i",
            "resolved_at_ms_after_close=12500i",
        ] {
            assert!(buf.contains(frag), "ILP line must carry {frag}: {buf}");
        }
    }

    #[test]
    fn test_cross_fill_audit_flush_disconnected_discards_and_errors() {
        let mut w = CrossFillAuditWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        assert_eq!(w.pending(), 1);
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("discarded"), "{err}");
        assert_eq!(w.pending(), 0, "poisoned-buffer defense clears pending");
        // Empty flush after discard is a clean no-op.
        w.flush().expect("empty flush ok");
    }

    #[test]
    fn test_cross_fill_audit_discard_pending_counts_and_clears() {
        let mut w = CrossFillAuditWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        assert_eq!(w.discard_pending(), 1);
        assert_eq!(w.pending(), 0);
        assert_eq!(w.discard_pending(), 0, "second discard is a no-op");
    }

    #[test]
    fn test_resolution_and_stage_wire_strings_are_pinned() {
        // Stable SYMBOL wire strings — forensic queries + the T+4s retry
        // session's PR depend on them.
        assert_eq!(CROSS_FILL_STAGE_CROSS_FILL, "cross_fill");
        assert_eq!(CROSS_FILL_STAGE_GROWW_FALLBACK, "groww_fallback");
        assert_eq!(CROSS_FILL_RESOLUTION_CROSS_FILL, "cross_fill");
        assert_eq!(CROSS_FILL_RESOLUTION_NATIVE_LATE_RETRY, "native_late_retry");
        assert_eq!(CROSS_FILL_RESOLUTION_NATIVE_FIRST_TRY, "native_first_try");
    }
}
