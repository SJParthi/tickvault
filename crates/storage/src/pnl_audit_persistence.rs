//! `pnl_audit` table — daily P&L snapshot forensics (cluster-C rebuild
//! 2026-07-14 of the Phase-0 Item-25 table deleted in #T4 2026-05-20;
//! runbook `.claude/rules/project/wave-2-error-codes.md` STORAGE-GAP-03).
//!
//! Snapshot triggers (the `snapshot_kind` SYMBOL):
//!   * `OnEod` — **the ONLY emitted kind today**: ONE aggregate row per
//!     trading day at the market-close signal (sentinel `security_id = 0`,
//!     `exchange_segment = "ALL"`), carrying the risk engine's
//!     session-cumulative realized + unrealized P&L — the DAILY
//!     HEARTBEAT/DENOMINATOR exercising channel→writer→ILP→QuestDB
//!     end-to-end. HONEST ENVELOPE (C1, 2026-07-14 hostile review): the
//!     emitting consumer + market-close signal are spawned by the trading
//!     pipeline, whose BOTH spawn sites are Dhan-lane-gated — the whole
//!     subsystem is code-ready but DORMANT while `feeds.dhan_enabled =
//!     false` (today's prod default): on a dhan-off boot NO row lands here
//!     (and nothing false-pages — nothing runs). The "≥1 on_eod row per
//!     trading day, so silence is detectable" heartbeat contract (audit
//!     Rule 11) holds whenever the Dhan lane / live trading runs (dhan-ON,
//!     in-session, strategy-config-present boots).
//!   * `OnFill` / `OnMinute` — enum variants SHIPPED, emits DEFERRED to
//!     Phase-1 (per-position fill/minute snapshots need the live fill path;
//!     honest scope — no dormant emit sites exist).
//!
//! BEST-EFFORT ONLY: a forensics write failure must NEVER affect the order
//! path or the market-close sweep — the consumer logs (coded
//! STORAGE-GAP-03, staged) + counts; a failed OnEod flush flips the daily
//! reconcile verdict to Mismatch (OMS-GAP-02), so the heartbeat's absence
//! is loud, never silent.
//!
//! **SEBI 5-year retention applies — rows are NEVER deleted.**
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS pnl_audit (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP,
//!     security_id LONG, exchange_segment SYMBOL, snapshot_kind SYMBOL,
//!     net_position_qty LONG, avg_entry_price DOUBLE, mark_price DOUBLE,
//!     realized_pnl DOUBLE, unrealized_pnl DOUBLE,
//!     mode SYMBOL, feed SYMBOL
//! ) timestamp(ts) PARTITION BY DAY
//! DEDUP UPSERT KEYS(ts, trading_date_ist, security_id, exchange_segment, snapshot_kind, feed);
//! ```
//!
//! DEDUP discipline: designated `ts` FIRST (2026-04-28 rule);
//! `trading_date_ist` in-key (phase-0 composite-DEDUP rule 1);
//! `exchange_segment` alongside `security_id` (I-P1-11); `snapshot_kind`
//! in-key (an OnFill and an OnMinute row for the same instant both
//! survive); `feed` in-key (feed-in-key EVERYWHERE, 2026-06-28 — every
//! writer stamps `'dhan'` today).

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;

/// QuestDB table name — P&L snapshot rows (SEBI 5y).
pub const PNL_AUDIT_TABLE: &str = "pnl_audit";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `trading_date_ist` in-key (phase-0 composite-DEDUP rule 1);
/// `exchange_segment` alongside `security_id` (I-P1-11); `snapshot_kind`
/// in-key; `feed` in-key (2026-06-28 override).
pub const DEDUP_KEY_PNL_AUDIT: &str =
    "ts, trading_date_ist, security_id, exchange_segment, snapshot_kind, feed";

/// Sentinel `security_id` for the aggregate OnEod heartbeat row (paired
/// with [`PNL_AUDIT_AGGREGATE_SEGMENT`]) — the row aggregates the WHOLE
/// session, not one instrument. Test-pinned; doc-commented at the emit
/// site in `crates/app/src/order_observability.rs`.
pub const PNL_AUDIT_AGGREGATE_SECURITY_ID: i64 = 0;

/// Sentinel `exchange_segment` for the aggregate OnEod heartbeat row.
pub const PNL_AUDIT_AGGREGATE_SEGMENT: &str = "ALL";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Typed snapshot trigger — the SYMBOL wire strings are stable (recovered
/// pre-#T4 strings preserved) and part of the DEDUP key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PnlSnapshotKind {
    /// Per-fill snapshot (`mark_price` = fill price; realized jumps).
    /// Emit is Phase-1 (live fill path) — variant shipped, no emit site.
    OnFill,
    /// 1-minute-boundary mark-to-market snapshot. Emit is Phase-1 —
    /// variant shipped, no emit site.
    OnMinute,
    /// The market-close terminal row — the daily heartbeat (emitted TODAY
    /// as one aggregate row per trading day).
    OnEod,
}

impl PnlSnapshotKind {
    /// Stable SYMBOL wire string (in the DEDUP key — never reworded).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OnFill => "on_fill",
            Self::OnMinute => "on_minute",
            Self::OnEod => "on_eod",
        }
    }
}

/// One P&L snapshot forensics row, ready for ILP write. Non-finite
/// doubles (avg_entry/mark/realized/unrealized) are clamped to 0.0 +
/// counted at append time — never a QuestDB reject.
#[derive(Clone, Debug, PartialEq)]
pub struct PnlAuditRow {
    /// Designated timestamp — snapshot instant, IST wall-clock nanoseconds.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Instrument id (the [`PNL_AUDIT_AGGREGATE_SECURITY_ID`] sentinel = 0
    /// for the aggregate OnEod row).
    pub security_id: i64,
    /// Segment classification ([`PNL_AUDIT_AGGREGATE_SEGMENT`] `"ALL"`
    /// for the aggregate OnEod row).
    pub exchange_segment: &'static str,
    /// Typed snapshot trigger.
    pub snapshot_kind: PnlSnapshotKind,
    /// Signed net position (+long / -short / 0 flat or aggregate).
    pub net_position_qty: i64,
    /// Average entry price (0.0 for the aggregate row).
    pub avg_entry_price: f64,
    /// Mark price at snapshot time (0.0 for the aggregate row).
    pub mark_price: f64,
    /// Session-cumulative booked P&L (rupees).
    pub realized_pnl: f64,
    /// Mark-to-market P&L on open positions (rupees).
    pub unrealized_pnl: f64,
    /// `paper` (dry-run) or `live`.
    pub mode: &'static str,
    /// Source broker (`dhan` always today).
    pub feed: &'static str,
}

/// The idempotent `CREATE TABLE` DDL for `pnl_audit`. Pure.
// FINANCIAL-TEST-EXEMPT: pure DDL string builder — no financial arithmetic ("pnl" is the table name)
#[must_use]
pub fn pnl_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {PNL_AUDIT_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            security_id      LONG, \
            exchange_segment SYMBOL, \
            snapshot_kind    SYMBOL, \
            net_position_qty LONG, \
            avg_entry_price  DOUBLE, \
            mark_price       DOUBLE, \
            realized_pnl     DOUBLE, \
            unrealized_pnl   DOUBLE, \
            mode             SYMBOL, \
            feed             SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_PNL_AUDIT});"
    )
}

/// Create the `pnl_audit` table if absent (idempotent self-heal order:
/// CREATE → per-column `ALTER ADD COLUMN IF NOT EXISTS` → DEDUP ENABLE —
/// the house template). Failures log at `error!` (code STORAGE-GAP-03,
/// stage `ensure_*`) but never block boot and never panic; NOTE the
/// HTTP-CLIENT-01-class consequence: a failed ensure leaves the table to
/// be auto-created by the first ILP write WITHOUT dedup (a duplicate-row
/// window until a later ensure succeeds).
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
// FINANCIAL-TEST-EXEMPT: idempotent DDL runner — no financial arithmetic ("pnl" is the table name)
pub async fn ensure_pnl_audit_table(questdb_config: &QuestDbConfig) {
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
            metrics::counter!("tv_pnl_audit_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "ensure_client_build",
                ?err,
                "STORAGE-GAP-03: HTTP client build failed — pnl_audit table not \
                 ensured (first ILP write may auto-create it WITHOUT dedup — \
                 duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![pnl_audit_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern).
    for (col, ty) in [
        ("trading_date_ist", "TIMESTAMP"),
        ("security_id", "LONG"),
        ("exchange_segment", "SYMBOL"),
        ("snapshot_kind", "SYMBOL"),
        ("net_position_qty", "LONG"),
        ("avg_entry_price", "DOUBLE"),
        ("mark_price", "DOUBLE"),
        ("realized_pnl", "DOUBLE"),
        ("unrealized_pnl", "DOUBLE"),
        ("mode", "SYMBOL"),
        ("feed", "SYMBOL"),
    ] {
        statements.push(format!(
            "ALTER TABLE {PNL_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {PNL_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_PNL_AUDIT});"
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
                metrics::counter!("tv_pnl_audit_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                // SEC-2: bounded DDL prefix (our own static statement — the
                // prefix identifies it; the full ~600-char CREATE would
                // exceed the ≤300-char error-payload house bound).
                error!(code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    %status, ddl = %ddl.chars().take(80).collect::<String>(),
                    body = %body.chars().take(200).collect::<String>(),
                    "STORAGE-GAP-03: pnl_audit DDL returned non-2xx (dedup may be \
                     missing — duplicate-row window until a later ensure succeeds)");
            }
            Err(err) => {
                metrics::counter!("tv_pnl_audit_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    // SEC-2: bounded DDL prefix (see the non-2xx arm).
                    ddl = %ddl.chars().take(80).collect::<String>(),
                    "STORAGE-GAP-03: pnl_audit DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf — per-flush server ACK (the 2026-07-05
/// fire-and-forget lesson) with the shadow-candle-writer knobs.
fn pnl_audit_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Clamp a non-finite double to 0.0 + count (QuestDB rejects NaN/Inf —
/// one poisoned value must never wedge the daily heartbeat row).
fn clamp_finite(value: f64, field: &'static str) -> f64 {
    if value.is_finite() {
        value
    } else {
        metrics::counter!("tv_pnl_audit_nonfinite_clamped_total").increment(1);
        warn!(
            field,
            raw = value,
            "pnl_audit: non-finite value clamped to 0.0 to avoid QuestDB reject"
        );
        0.0
    }
}

/// Lazy ILP-over-HTTP writer for `pnl_audit`. Same contract as
/// `OrderAuditWriter`: unreachable QuestDB at construction still builds
/// (rows buffer locally); a failed flush DISCARDS the pending buffer
/// (poisoned-buffer defense) — counted + coded, never silent.
pub struct PnlAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl PnlAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_pnl_audit_writer_new_is_lazy...); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = pnl_audit_ilp_http_conf(config);
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
                    "pnl_audit writer: QuestDB unreachable — buffering locally"
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

    /// Appends one P&L snapshot row (cold path — one OnEod row per
    /// trading day today). Non-finite doubles clamp to 0.0 + count.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_pnl_audit_row(&mut self, r: &PnlAuditRow) -> Result<()> {
        self.buffer
            .table(PNL_AUDIT_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("exchange_segment", r.exchange_segment)
            .context("exchange_segment")?
            .symbol("snapshot_kind", r.snapshot_kind.as_str())
            .context("snapshot_kind")?
            .symbol("mode", r.mode)
            .context("mode")?
            .symbol("feed", r.feed)
            .context("feed")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("net_position_qty", r.net_position_qty)
            .context("net_position_qty")?
            .column_f64(
                "avg_entry_price",
                clamp_finite(r.avg_entry_price, "avg_entry_price"),
            )
            .context("avg_entry_price")?
            .column_f64("mark_price", clamp_finite(r.mark_price, "mark_price"))
            .context("mark_price")?
            .column_f64("realized_pnl", clamp_finite(r.realized_pnl, "realized_pnl"))
            .context("realized_pnl")?
            .column_f64(
                "unrealized_pnl",
                clamp_finite(r.unrealized_pnl, "unrealized_pnl"),
            )
            .context("unrealized_pnl")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        metrics::counter!("tv_pnl_audit_rows_total", "snapshot_kind" => r.snapshot_kind.as_str())
            .increment(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK). On ANY
    /// failed flush the pending buffer is DISCARDED (poisoned-buffer
    /// defense) — counted (`tv_pnl_audit_rows_discarded_total`), never
    /// silent. A failed OnEod flush flips the daily reconcile verdict to
    /// Mismatch (OMS-GAP-02) at the consumer.
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
                "pnl_audit: no ILP sender (QuestDB unreachable) — \
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
                    "pnl_audit ILP flush failed — {dropped} pending \
                     forensics row(s) discarded (poisoned-buffer defense)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("pnl_audit: ILP sender vanished — {dropped} row(s) discarded");
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted so a discard is never silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_pnl_audit_rows_discarded_total").increment(dropped as u64);
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

    fn sample_row() -> PnlAuditRow {
        PnlAuditRow {
            ts_ist_nanos: 1_770_000_900_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            security_id: PNL_AUDIT_AGGREGATE_SECURITY_ID,
            exchange_segment: PNL_AUDIT_AGGREGATE_SEGMENT,
            snapshot_kind: PnlSnapshotKind::OnEod,
            net_position_qty: 0,
            avg_entry_price: 0.0,
            mark_price: 0.0,
            realized_pnl: -1250.5,
            unrealized_pnl: 340.25,
            mode: "paper",
            feed: "dhan",
        }
    }

    #[test]
    fn test_pnl_audit_create_ddl_contains_expected_columns() {
        let ddl = pnl_audit_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "security_id",
            "exchange_segment",
            "snapshot_kind",
            "net_position_qty",
            "avg_entry_price",
            "mark_price",
            "realized_pnl",
            "unrealized_pnl",
            "mode",
            "feed",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_PNL_AUDIT})")));
    }

    /// DEDUP-key discipline: designated `ts` FIRST (2026-04-28 regression
    /// rule); `trading_date_ist` in-key (phase-0 composite-DEDUP rule 1);
    /// `exchange_segment` alongside `security_id` (I-P1-11);
    /// `snapshot_kind` in-key; `feed` in-key (2026-06-28 override) —
    /// whole-token matches + arity 6.
    #[test]
    fn test_pnl_audit_dedup_key_ts_first_snapshot_kind_and_feed_in_key() {
        assert!(DEDUP_KEY_PNL_AUDIT.trim_start().starts_with("ts,"));
        let has_token = |t: &str| {
            DEDUP_KEY_PNL_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        assert!(has_token("trading_date_ist"));
        assert!(has_token("security_id"));
        assert!(has_token("exchange_segment"));
        assert!(has_token("snapshot_kind"));
        assert!(has_token("feed"));
        // Exactly (ts, trading_date_ist, security_id, exchange_segment,
        // snapshot_kind, feed).
        assert_eq!(DEDUP_KEY_PNL_AUDIT.matches(',').count() + 1, 6);
    }

    /// Snapshot-kind SYMBOL wire strings are stable (the recovered pre-#T4
    /// strings preserved) and cover every enum variant distinctly.
    #[test]
    fn test_pnl_snapshot_kind_wire_strings_stable_and_distinct() {
        let all = [
            (PnlSnapshotKind::OnFill, "on_fill"),
            (PnlSnapshotKind::OnMinute, "on_minute"),
            (PnlSnapshotKind::OnEod, "on_eod"),
        ];
        for (variant, wire) in all {
            assert_eq!(variant.as_str(), wire);
        }
        let mut wires: Vec<&str> = all.iter().map(|(v, _)| v.as_str()).collect();
        wires.sort_unstable();
        wires.dedup();
        assert_eq!(wires.len(), all.len(), "wire strings must be distinct");
    }

    /// The aggregate OnEod heartbeat sentinels are pinned — the daily row
    /// aggregates the WHOLE session (security_id 0 / segment "ALL").
    #[test]
    fn test_pnl_audit_aggregate_sentinels_pinned() {
        assert_eq!(PNL_AUDIT_AGGREGATE_SECURITY_ID, 0);
        assert_eq!(PNL_AUDIT_AGGREGATE_SEGMENT, "ALL");
    }

    #[test]
    fn test_append_pnl_audit_row_writes_symbols_and_columns() {
        let mut w = PnlAuditWriter::for_test();
        w.append_pnl_audit_row(&sample_row())
            .expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(PNL_AUDIT_TABLE));
        assert!(
            line.contains(",exchange_segment=ALL"),
            "segment tag: {line}"
        );
        assert!(
            line.contains(",snapshot_kind=on_eod"),
            "snapshot_kind tag: {line}"
        );
        assert!(line.contains(",mode=paper"), "mode tag: {line}");
        assert!(line.contains(",feed=dhan"), "feed tag: {line}");
        assert!(line.contains("security_id=0i"), "security_id: {line}");
        assert!(
            line.contains("realized_pnl=-1250.5"),
            "realized_pnl: {line}"
        );
        assert!(
            line.contains("unrealized_pnl=340.25"),
            "unrealized_pnl: {line}"
        );
    }

    /// Financial boundary: a flat zero-P&L day (no trades) appends a
    /// legitimate all-zero heartbeat row.
    #[test]
    fn test_append_pnl_audit_row_zero_pnl_boundary() {
        let mut w = PnlAuditWriter::for_test();
        let row = PnlAuditRow {
            realized_pnl: 0.0,
            unrealized_pnl: 0.0,
            ..sample_row()
        };
        w.append_pnl_audit_row(&row).expect("append must succeed");
        let line = w.buffer_utf8();
        assert!(line.contains("realized_pnl=0"), "got: {line}");
        assert!(line.contains("unrealized_pnl=0"), "got: {line}");
    }

    /// Financial boundary: an extreme negative realized P&L (catastrophic
    /// loss day) survives the append without distortion.
    #[test]
    fn test_append_pnl_audit_row_extreme_negative_realized_pnl_boundary() {
        let mut w = PnlAuditWriter::for_test();
        let row = PnlAuditRow {
            realized_pnl: -1.0e12,
            ..sample_row()
        };
        w.append_pnl_audit_row(&row).expect("append must succeed");
        let line = w.buffer_utf8();
        assert!(
            line.contains("realized_pnl=-1000000000000"),
            "extreme negative realized pnl must survive verbatim: {line}"
        );
    }

    /// Financial boundary: NaN/Infinity P&L values clamp to 0.0 + never
    /// poison the ILP buffer (QuestDB rejects non-finite doubles).
    #[test]
    fn test_append_pnl_audit_row_nonfinite_values_clamped_to_zero_boundary() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut w = PnlAuditWriter::for_test();
            let row = PnlAuditRow {
                realized_pnl: bad,
                unrealized_pnl: bad,
                mark_price: bad,
                avg_entry_price: bad,
                ..sample_row()
            };
            w.append_pnl_audit_row(&row).expect("append must succeed");
            let line = w.buffer_utf8();
            assert!(!line.contains("NaN"), "NaN leaked into ILP: {line}");
            assert!(line.contains("realized_pnl=0"), "got: {line}");
            assert!(line.contains("unrealized_pnl=0"), "got: {line}");
        }
    }

    #[test]
    fn test_pnl_audit_flush_when_disconnected_errors_and_discards_pending() {
        let mut w = PnlAuditWriter::for_test();
        w.append_pnl_audit_row(&sample_row())
            .expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert!(err.to_string().contains("discarded"));
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
        // Empty flush is a no-op Ok.
        let mut empty = PnlAuditWriter::for_test();
        assert!(empty.flush().is_ok());
        // Idempotent: a second discard drops nothing.
        assert_eq!(w.discard_pending(), 0);
    }

    /// Transport ratchet (2026-07-05 fire-and-forget lesson): ILP-over-HTTP
    /// with the shadow-candle-writer knobs — never ILP TCP 9009.
    #[test]
    fn test_pnl_audit_writer_uses_ilp_http_conf_with_bounded_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = pnl_audit_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
    }

    // ========================================================================
    // Mock QuestDB /exec HTTP server + unreachable host (the
    // rest_fetch_audit_persistence pattern) — exercises the real ensure /
    // constructor code paths: success (200), non-2xx (500), transport error.
    // ========================================================================

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_500: &str =
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 13\r\n\r\n{\"error\":\"x\"}";

    async fn spawn_mock_http(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    fn mock_cfg(http_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    fn unreachable_cfg() -> QuestDbConfig {
        // Port 1 is reserved and never listening; guarantees a real HTTP
        // transport failure without touching any live service.
        mock_cfg(1)
    }

    #[tokio::test]
    async fn test_ensure_pnl_audit_table_mock_200_completes() {
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_pnl_audit_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_pnl_audit_table_mock_500_degrades_without_panic() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_pnl_audit_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_pnl_audit_table_unreachable_degrades_without_panic() {
        ensure_pnl_audit_table(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_pnl_audit_writer_new_is_lazy_and_buffers_without_network() {
        let mut w = PnlAuditWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_pnl_audit_row(&sample_row())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }

    /// M2 (2026-07-14 hostile review): the `Some(Err(..))` flush arm — a
    /// REAL server reject through a live (lazily-built) HTTP sender —
    /// exercises the poisoned-buffer discard, not just the no-sender bail.
    /// Multi-thread flavor so the mock server task serves while the sync
    /// flush blocks the test task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_pnl_audit_flush_server_reject_hits_some_err_and_discards() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        let mut w = PnlAuditWriter::new(&mock_cfg(port));
        w.append_pnl_audit_row(&sample_row())
            .expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let err = w.flush().expect_err("server 500 must surface as Err");
        assert!(
            err.to_string().contains("discarded"),
            "discard context expected: {err:#}"
        );
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
    }
}
