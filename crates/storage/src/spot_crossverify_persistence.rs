//! Post-close Dhan↔Groww `spot_1m_rest` cross-broker comparator — audit
//! persistence (SPOT-XVERIFY-01/02).
//!
//! Two QuestDB tables via ILP-over-HTTP (per-flush server ACK — the
//! 2026-07-05 fire-and-forget lesson): `spot_crossverify_cell_audit` (one
//! row per divergent/missing cell) + `spot_crossverify_daily` (one row per
//! run — the divergence-TREND store + the keep-better rerun target). Mirrors
//! `tf_consistency_audit_persistence.rs`. Cold path, best-effort — never on
//! the tick hot path, never on any recovery / REST-capture path.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;

/// QuestDB table — one row per divergent / missing / out-of-session cell.
pub const SPOT_XVERIFY_CELL_AUDIT_TABLE: &str = "spot_crossverify_cell_audit";
/// QuestDB table — one row per run (the daily trend + keep-better target).
pub const SPOT_XVERIFY_DAILY_TABLE: &str = "spot_crossverify_daily";
/// The `feed` in-key wire value (operator override 2026-06-28 — feed in the
/// DEDUP key of EVERY persisted table). These tables are inherently
/// CROSS-FEED (each row spans a Dhan cell vs a Groww cell; `kind` names
/// which side is missing), so a single-feed label would be dishonest — the
/// honest constant names the comparison scope. Constant ⇒ a no-op in dedup
/// terms (one row per logical key), it satisfies the feed-in-key guard
/// without falsely tagging a cross-feed row as `dhan`.
pub const SPOT_XVERIFY_FEED_SCOPE: &str = "dhan_x_groww";

/// Cell-audit DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `exchange_segment` in-key (I-P1-11 + dedup_segment_meta_guard); `field` +
/// `kind` in-key so a diverged AND a missing transition on the same minute
/// both survive (phase-0 rule 3). The identity is the canonical `index_name`
/// (a cell is cross-feed by construction — `feed` is the SELECT filter, not
/// an audit key). Deterministic run `ts` (target day 15:47:00 IST) ⇒ reruns
/// UPSERT in place.
pub const DEDUP_KEY_SPOT_XVERIFY_CELL_AUDIT: &str =
    "ts, trading_date_ist, feed, index_name, exchange_segment, minute_ts_ist, kind, field";

/// Daily DEDUP key — `outcome` in-key so a measured verdict is never erased
/// by a later blind rerun (the brutex/tf keep-better precedent; the caller's
/// keep-better guard additionally suppresses a downgrade).
pub const DEDUP_KEY_SPOT_XVERIFY_DAILY: &str = "ts, trading_date_ist, feed, outcome";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format cell kind (the `kind` SYMBOL column + the
/// `tv_spot_xverify_findings_total{kind}` counter label). Never reworded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpotXverifyCellKind {
    /// Both feeds have the minute; an OHLC field differs beyond tolerance.
    Diverged,
    /// The minute is present in Groww only (Dhan `spot_1m_rest` lacks it).
    MissingDhan,
    /// The minute is present in Dhan only (Groww `spot_1m_rest` lacks it).
    MissingGroww,
    /// A row outside [09:15, 15:30) IST — recorded, not classified.
    OutOfSession,
}

impl SpotXverifyCellKind {
    /// Stable wire label (audit SYMBOL + counter label). Never reworded.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diverged => "diverged",
            Self::MissingDhan => "missing_dhan",
            Self::MissingGroww => "missing_groww",
            Self::OutOfSession => "out_of_session",
        }
    }
}

/// Stable wire-format daily outcome (the `outcome` SYMBOL column + the
/// `tv_spot_xverify_runs_total{status}` counter label). Never reworded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpotXverifyOutcome {
    /// compared > 0, zero divergences.
    Clean,
    /// ≥1 divergence.
    Diverged,
    /// a degraded leg but partial comparison landed.
    Partial,
    /// both feeds empty — a non-trading / feed-off day (Info, log-only).
    NoData,
    /// one feed had rows + zero overlap ⇒ never a silent clean (Rule 11).
    Blind,
    /// a run leg failed hard.
    Degraded,
}

impl SpotXverifyOutcome {
    /// Stable wire label. Never reworded.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Diverged => "diverged",
            Self::Partial => "partial",
            Self::NoData => "no_data",
            Self::Blind => "blind",
            Self::Degraded => "degraded",
        }
    }
    /// True for a MEASURED verdict — the keep-better guard refuses to
    /// overwrite one of these with a later `no_data`/`blind`/`degraded`.
    #[must_use]
    pub const fn is_measured(self) -> bool {
        matches!(self, Self::Clean | Self::Diverged | Self::Partial)
    }
}

/// One cross-broker cell finding, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct SpotXverifyCellFinding {
    /// Designated timestamp — the DETERMINISTIC run stamp (target trading
    /// day 15:47:00 IST, nanoseconds). Reruns UPSERT in place.
    pub run_ts_ist_nanos: i64,
    /// The verified trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Canonical index name (`NIFTY` / `BANKNIFTY` / `SENSEX` / `INDIA VIX`).
    pub index_name: String,
    /// Segment SYMBOL string (`IDX_I`).
    pub exchange_segment: String,
    /// The compared minute — bucket OPEN time (IST nanoseconds).
    pub minute_ts_ist_nanos: i64,
    pub kind: SpotXverifyCellKind,
    /// `open` / `high` / `low` / `close` / `volume` / `none`.
    pub field: &'static str,
    /// Dhan-side price cell (rupees; 0.0 for presence rows).
    pub dhan_value: f64,
    /// Groww-side price cell (rupees; 0.0 for presence rows).
    pub groww_value: f64,
    /// Stored Dhan volume (exact i64 — never classified).
    pub dhan_volume: i64,
    /// Stored Groww volume (exact i64 — never classified).
    pub groww_volume: i64,
    /// `|dhan - groww|` in paise (0 for presence rows).
    pub diff_paise: i64,
}

/// One daily summary row, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct SpotXverifyDailyRow {
    pub run_ts_ist_nanos: i64,
    pub trading_date_ist_nanos: i64,
    pub indices: i64,
    pub minutes_compared: i64,
    pub mismatches: i64,
    pub missing_dhan: i64,
    pub missing_groww: i64,
    pub out_of_session: i64,
    pub outcome: SpotXverifyOutcome,
}

/// Idempotent `CREATE TABLE` DDL for `spot_crossverify_cell_audit`. Pure.
#[must_use]
pub fn spot_xverify_cell_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {SPOT_XVERIFY_CELL_AUDIT_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            feed             SYMBOL, \
            index_name       SYMBOL, \
            exchange_segment SYMBOL, \
            minute_ts_ist    TIMESTAMP, \
            kind             SYMBOL, \
            field            SYMBOL, \
            dhan_value       DOUBLE, \
            groww_value      DOUBLE, \
            dhan_volume      LONG, \
            groww_volume     LONG, \
            diff_paise       LONG\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_SPOT_XVERIFY_CELL_AUDIT});"
    )
}

/// Idempotent `CREATE TABLE` DDL for `spot_crossverify_daily`. Pure.
#[must_use]
pub fn spot_xverify_daily_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {SPOT_XVERIFY_DAILY_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            feed             SYMBOL, \
            indices          LONG, \
            minutes_compared LONG, \
            mismatches       LONG, \
            missing_dhan     LONG, \
            missing_groww    LONG, \
            out_of_session   LONG, \
            outcome          SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_SPOT_XVERIFY_DAILY});"
    )
}

/// Create both audit tables if absent (CREATE → per-column `ALTER ADD COLUMN
/// IF NOT EXISTS` → DEDUP ENABLE — never a drop). Failures log at `error!`
/// (SPOT-XVERIFY-02, stage `ensure_ddl`) but never block; a failed ensure
/// leaves the first ILP write to auto-create WITHOUT dedup — a duplicate-row
/// window until a later ensure succeeds (HTTP-CLIENT-01 class).
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via the create_ddl tests).
pub async fn ensure_spot_crossverify_tables(questdb_config: &QuestDbConfig) {
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
            metrics::counter!(
                "tv_spot_xverify_query_failures_total",
                "stage" => "ensure_client_build"
            )
            .increment(1);
            error!(
                code = ErrorCode::SpotXverify02RunDegraded.code_str(),
                stage = "ensure_client_build",
                ?err,
                "SPOT-XVERIFY-02: HTTP client build failed — audit tables not \
                 ensured (first ILP write may auto-create them WITHOUT dedup)"
            );
            return;
        }
    };
    let mut statements = vec![
        spot_xverify_cell_audit_create_ddl(),
        spot_xverify_daily_create_ddl(),
    ];
    for (col, ty) in [
        ("trading_date_ist", "TIMESTAMP"),
        ("feed", "SYMBOL"),
        ("index_name", "SYMBOL"),
        ("exchange_segment", "SYMBOL"),
        ("minute_ts_ist", "TIMESTAMP"),
        ("kind", "SYMBOL"),
        ("field", "SYMBOL"),
        ("dhan_value", "DOUBLE"),
        ("groww_value", "DOUBLE"),
        ("dhan_volume", "LONG"),
        ("groww_volume", "LONG"),
        ("diff_paise", "LONG"),
    ] {
        statements.push(format!(
            "ALTER TABLE {SPOT_XVERIFY_CELL_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    for (col, ty) in [
        ("trading_date_ist", "TIMESTAMP"),
        ("feed", "SYMBOL"),
        ("indices", "LONG"),
        ("minutes_compared", "LONG"),
        ("mismatches", "LONG"),
        ("missing_dhan", "LONG"),
        ("missing_groww", "LONG"),
        ("out_of_session", "LONG"),
        ("outcome", "SYMBOL"),
    ] {
        statements.push(format!(
            "ALTER TABLE {SPOT_XVERIFY_DAILY_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {SPOT_XVERIFY_CELL_AUDIT_TABLE} DEDUP ENABLE \
         UPSERT KEYS({DEDUP_KEY_SPOT_XVERIFY_CELL_AUDIT});"
    ));
    statements.push(format!(
        "ALTER TABLE {SPOT_XVERIFY_DAILY_TABLE} DEDUP ENABLE \
         UPSERT KEYS({DEDUP_KEY_SPOT_XVERIFY_DAILY});"
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
                metrics::counter!("tv_spot_xverify_query_failures_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::SpotXverify02RunDegraded.code_str(),
                    stage = "ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "SPOT-XVERIFY-02: cross-verify DDL returned non-2xx (dedup \
                     may be missing — duplicate-row window until a later ensure)"
                );
            }
            Err(err) => {
                metrics::counter!("tv_spot_xverify_query_failures_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::SpotXverify02RunDegraded.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "SPOT-XVERIFY-02: cross-verify DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf — per-flush server ACK + the shadow-writer knobs
/// (`retry_timeout=0`, `request_timeout=5000` ms).
fn spot_xverify_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for the two cross-verify tables. Unreachable
/// QuestDB at construction still builds (rows buffer locally); `flush`
/// returns `Err` — incl. server-side rejects via the HTTP ACK — and the
/// pending buffer is DISCARDED on failure (poisoned-buffer defense; findings
/// are recomputable + reruns are DEDUP-idempotent).
pub struct SpotXverifyAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl SpotXverifyAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via the new-is-lazy test); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = spot_xverify_ilp_http_conf(config);
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
                    "spot_crossverify writer: QuestDB unreachable — buffering locally"
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

    /// Appends one cell finding row (cold path, once a day). Symbols BEFORE
    /// columns (ILP tags-before-fields rule).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_cell(&mut self, f: &SpotXverifyCellFinding) -> Result<()> {
        self.buffer
            .table(SPOT_XVERIFY_CELL_AUDIT_TABLE)
            .context("table")?
            .symbol("feed", SPOT_XVERIFY_FEED_SCOPE)
            .context("feed")?
            .symbol("index_name", f.index_name.as_str())
            .context("index_name")?
            .symbol("exchange_segment", f.exchange_segment.as_str())
            .context("exchange_segment")?
            .symbol("kind", f.kind.as_str())
            .context("kind")?
            .symbol("field", f.field)
            .context("field")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(f.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_ts("minute_ts_ist", TimestampNanos::new(f.minute_ts_ist_nanos))
            .context("minute_ts_ist")?
            .column_f64("dhan_value", f.dhan_value)
            .context("dhan_value")?
            .column_f64("groww_value", f.groww_value)
            .context("groww_value")?
            .column_i64("dhan_volume", f.dhan_volume)
            .context("dhan_volume")?
            .column_i64("groww_volume", f.groww_volume)
            .context("groww_volume")?
            .column_i64("diff_paise", f.diff_paise)
            .context("diff_paise")?
            .at(TimestampNanos::new(f.run_ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Appends the daily summary row.
    ///
    /// # Errors
    /// Propagates ILP buffer errors.
    pub fn append_daily(&mut self, r: &SpotXverifyDailyRow) -> Result<()> {
        self.buffer
            .table(SPOT_XVERIFY_DAILY_TABLE)
            .context("table")?
            .symbol("feed", SPOT_XVERIFY_FEED_SCOPE)
            .context("feed")?
            .symbol("outcome", r.outcome.as_str())
            .context("outcome")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("indices", r.indices)
            .context("indices")?
            .column_i64("minutes_compared", r.minutes_compared)
            .context("minutes_compared")?
            .column_i64("mismatches", r.mismatches)
            .context("mismatches")?
            .column_i64("missing_dhan", r.missing_dhan)
            .context("missing_dhan")?
            .column_i64("missing_groww", r.missing_groww)
            .context("missing_groww")?
            .column_i64("out_of_session", r.out_of_session)
            .context("out_of_session")?
            .at(TimestampNanos::new(r.run_ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK). On ANY
    /// failed flush the pending buffer is DISCARDED (poisoned-buffer defense;
    /// the caller emits SPOT-XVERIFY-02 stage=flush + the run reads degraded).
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
                "spot_crossverify: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending row(s) discarded (recomputable, DEDUP-idempotent rerun)"
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
                    "spot_crossverify ILP flush failed — {dropped} pending row(s) \
                     discarded (poisoned-buffer defense; rerun is DEDUP-idempotent)"
                )))
            }
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("spot_crossverify: ILP sender vanished — {dropped} row(s) discarded");
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted by
    /// `tv_spot_xverify_audit_rows_discarded_total` so a discard is never
    /// silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_spot_xverify_audit_rows_discarded_total")
                .increment(dropped as u64);
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

    fn sample_cell() -> SpotXverifyCellFinding {
        SpotXverifyCellFinding {
            run_ts_ist_nanos: 1,
            trading_date_ist_nanos: 2,
            index_name: "NIFTY".to_string(),
            exchange_segment: "IDX_I".to_string(),
            minute_ts_ist_nanos: 3,
            kind: SpotXverifyCellKind::Diverged,
            field: "high",
            dhan_value: 100.5,
            groww_value: 100.55,
            dhan_volume: 0,
            groww_volume: 0,
            diff_paise: 5,
        }
    }

    #[test]
    fn cell_ddl_contains_columns_and_dedup_key_with_segment_field_kind() {
        let ddl = spot_xverify_cell_audit_create_ddl();
        for tok in [
            "ts",
            "trading_date_ist",
            "feed",
            "index_name",
            "exchange_segment",
            "minute_ts_ist",
            "kind",
            "field",
            "dhan_value",
            "groww_value",
            "dhan_volume",
            "groww_volume",
            "diff_paise",
            "timestamp(ts)",
            "PARTITION BY DAY",
            "DEDUP UPSERT KEYS",
        ] {
            assert!(ddl.contains(tok), "cell DDL missing {tok}: {ddl}");
        }
        // dedup_segment_meta_guard + I-P1-11 + phase-0 rule 3.
        assert!(DEDUP_KEY_SPOT_XVERIFY_CELL_AUDIT.contains("feed"));
        assert!(DEDUP_KEY_SPOT_XVERIFY_CELL_AUDIT.contains("exchange_segment"));
        assert!(DEDUP_KEY_SPOT_XVERIFY_CELL_AUDIT.contains("field"));
        assert!(DEDUP_KEY_SPOT_XVERIFY_CELL_AUDIT.contains("kind"));
        assert!(DEDUP_KEY_SPOT_XVERIFY_CELL_AUDIT.starts_with("ts,"));
    }

    #[test]
    fn daily_ddl_dedup_key_ends_with_outcome() {
        let ddl = spot_xverify_daily_create_ddl();
        assert!(ddl.contains("DEDUP UPSERT KEYS"));
        assert!(ddl.contains("outcome"));
        assert!(DEDUP_KEY_SPOT_XVERIFY_DAILY.contains("feed"));
        assert!(DEDUP_KEY_SPOT_XVERIFY_DAILY.ends_with("outcome"));
        assert!(DEDUP_KEY_SPOT_XVERIFY_DAILY.starts_with("ts,"));
    }

    #[test]
    fn kind_and_outcome_wire_labels_are_stable() {
        assert_eq!(SpotXverifyCellKind::Diverged.as_str(), "diverged");
        assert_eq!(SpotXverifyCellKind::MissingDhan.as_str(), "missing_dhan");
        assert_eq!(SpotXverifyCellKind::MissingGroww.as_str(), "missing_groww");
        assert_eq!(SpotXverifyCellKind::OutOfSession.as_str(), "out_of_session");
        assert_eq!(SpotXverifyOutcome::Clean.as_str(), "clean");
        assert_eq!(SpotXverifyOutcome::Blind.as_str(), "blind");
        assert!(SpotXverifyOutcome::Diverged.is_measured());
        assert!(!SpotXverifyOutcome::Blind.is_measured());
        assert!(!SpotXverifyOutcome::NoData.is_measured());
        assert!(!SpotXverifyOutcome::Degraded.is_measured());
    }

    #[test]
    fn append_cell_buffers_symbols_and_columns() {
        let mut w = SpotXverifyAuditWriter::for_test();
        w.append_cell(&sample_cell()).unwrap();
        assert_eq!(w.pending(), 1);
        let buf = w.buffer_utf8();
        assert!(buf.contains("spot_crossverify_cell_audit"));
        assert!(buf.contains("index_name=NIFTY"));
        assert!(buf.contains("kind=diverged"));
    }

    #[test]
    fn append_daily_buffers_outcome_symbol() {
        let mut w = SpotXverifyAuditWriter::for_test();
        w.append_daily(&SpotXverifyDailyRow {
            run_ts_ist_nanos: 1,
            trading_date_ist_nanos: 2,
            indices: 3,
            minutes_compared: 300,
            mismatches: 4,
            missing_dhan: 1,
            missing_groww: 2,
            out_of_session: 0,
            outcome: SpotXverifyOutcome::Diverged,
        })
        .unwrap();
        assert_eq!(w.pending(), 1);
        assert!(w.buffer_utf8().contains("outcome=diverged"));
    }

    #[test]
    fn discard_pending_clears_buffer_and_count() {
        let mut w = SpotXverifyAuditWriter::for_test();
        w.append_cell(&sample_cell()).unwrap();
        w.append_cell(&sample_cell()).unwrap();
        assert_eq!(w.pending(), 2);
        assert_eq!(w.discard_pending(), 2);
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty());
    }

    #[test]
    fn flush_without_sender_discards_and_errs() {
        let mut w = SpotXverifyAuditWriter::for_test();
        w.append_cell(&sample_cell()).unwrap();
        assert!(w.flush().is_err());
        assert_eq!(w.pending(), 0);
    }

    #[test]
    fn test_spot_xverify_cell_audit_create_ddl_is_idempotent_and_embeds_full_dedup_key() {
        let ddl = spot_xverify_cell_audit_create_ddl();
        // Idempotent CREATE — safe to run on every boot, never a drop.
        assert!(
            ddl.starts_with(&format!(
                "CREATE TABLE IF NOT EXISTS {SPOT_XVERIFY_CELL_AUDIT_TABLE} ("
            )),
            "cell DDL must be an idempotent CREATE for the named table: {ddl}"
        );
        // The FULL dedup clause is embedded verbatim (not a rephrased copy).
        assert!(
            ddl.contains(&format!(
                "DEDUP UPSERT KEYS({DEDUP_KEY_SPOT_XVERIFY_CELL_AUDIT})"
            )),
            "cell DDL must embed the exact DEDUP key constant: {ddl}"
        );
        assert!(!ddl.contains("DROP"), "cell DDL must never drop: {ddl}");
    }

    #[test]
    fn test_spot_xverify_daily_create_ddl_is_idempotent_and_embeds_full_dedup_key() {
        let ddl = spot_xverify_daily_create_ddl();
        assert!(
            ddl.starts_with(&format!(
                "CREATE TABLE IF NOT EXISTS {SPOT_XVERIFY_DAILY_TABLE} ("
            )),
            "daily DDL must be an idempotent CREATE for the named table: {ddl}"
        );
        assert!(
            ddl.contains(&format!(
                "DEDUP UPSERT KEYS({DEDUP_KEY_SPOT_XVERIFY_DAILY})"
            )),
            "daily DDL must embed the exact DEDUP key constant: {ddl}"
        );
        // Every summary column the writer appends exists in the schema.
        for col in [
            "indices",
            "minutes_compared",
            "mismatches",
            "missing_dhan",
            "missing_groww",
            "out_of_session",
        ] {
            assert!(ddl.contains(&format!("{col} ")), "daily DDL missing {col}");
        }
        assert!(!ddl.contains("DROP"), "daily DDL must never drop: {ddl}");
    }

    #[test]
    fn ilp_conf_uses_http_not_tcp() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = spot_xverify_ilp_http_conf(&cfg);
        assert!(conf.starts_with("http::addr=tv-questdb:9000;"));
        assert!(!conf.contains("tcp::"), "must not use ILP TCP: {conf}");
    }
}
