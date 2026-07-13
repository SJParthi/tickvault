//! `tf_consistency_audit` table — daily timeframe-consistency verifier
//! (operator directive 2026-07-13: *"how will you guarantee that all our
//! defined timeframes internally are correct — how do you identify whether
//! any miscalculation or data issues"*; runbook
//! `.claude/rules/project/tf-consistency-error-codes.md`).
//!
//! ONE row per finding cell produced by the 15:40 IST verifier
//! (`crates/app/src/tf_consistency_boot.rs`): a stored higher-TF candle
//! that disagrees with its recomputed-from-1m value, a missing/phantom TF
//! row, an off-grid timestamp, or a duplicate DEDUP key. The designated
//! `ts` is the DETERMINISTIC run stamp (the target trading day's 15:40:00
//! IST), so a rerun/backfill of the same day UPSERTs in place (DEDUP
//! idempotency by construction — the scoreboard mechanic).
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS tf_consistency_audit (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP, feed SYMBOL,
//!     security_id LONG, segment SYMBOL, tf SYMBOL,
//!     bucket_ts_ist TIMESTAMP, category SYMBOL, field SYMBOL,
//!     stored_value DOUBLE, recomputed_value DOUBLE,
//!     stored_volume LONG, recomputed_volume LONG, one_m_rows LONG
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, security_id, segment,
//!                     tf, bucket_ts_ist, category, field);
//! ```
//!
//! Volume mismatches carry the exact i64 values in the dedicated
//! `stored_volume` / `recomputed_volume` LONG columns (never a lossy
//! i64→f64 widening); the DOUBLE value columns are 0.0 on those rows.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;

/// QuestDB table name — one row per verifier finding cell.
pub const TF_CONSISTENCY_AUDIT_TABLE: &str = "tf_consistency_audit";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `segment` alongside `security_id` (I-P1-11); `feed` in-key (operator
/// override 2026-06-28 — feed-in-key EVERYWHERE). The deterministic run
/// `ts` (target day 15:40:00 IST) makes reruns UPSERT in place.
pub const DEDUP_KEY_TF_CONSISTENCY_AUDIT: &str =
    "ts, trading_date_ist, feed, security_id, segment, tf, bucket_ts_ist, category, field";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format finding category (the `category` SYMBOL column +
/// the `tv_tf_verify_findings_total{category}` counter label).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FindingCategory {
    /// A strict field (open/high/low/close/volume) of a stored higher-TF
    /// candle disagrees with the value recomputed from its 1m members.
    Mismatch,
    /// 1m members exist for the window but the stored TF row is absent —
    /// the dead-seal-leg / lost-candle signature.
    MissingTfRow,
    /// A stored TF row exists over ZERO member 1m rows — either the 1m leg
    /// lost data or the TF row is phantom.
    No1mCoverage,
    /// A stored TF row's `ts` is not on the 09:15-anchored bucket grid —
    /// the purest anchoring-bug signal.
    OffGridTs,
    /// More than one row shares one DEDUP key in a read response — DEDUP
    /// did not engage on that table (the HTTP-CLIENT-01 DDL-skip window).
    DuplicateKey,
}

impl FindingCategory {
    /// Stable wire label (audit SYMBOL + counter label). Never reworded.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mismatch => "mismatch",
            Self::MissingTfRow => "missing_tf_row",
            Self::No1mCoverage => "no_1m_coverage",
            Self::OffGridTs => "off_grid_ts",
            Self::DuplicateKey => "duplicate_key",
        }
    }
}

/// One verifier finding cell, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct TfConsistencyFinding {
    /// Designated timestamp — the DETERMINISTIC run stamp (target trading
    /// day 15:40:00 IST, nanoseconds). Reruns UPSERT in place.
    pub run_ts_ist_nanos: i64,
    /// The verified trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Feed whose candles were verified (`dhan` / `groww`).
    pub feed: &'static str,
    pub security_id: i64,
    /// Segment SYMBOL string (`IDX_I`, `NSE_EQ`, ...). Owned — comes from
    /// the discovery query (cold path, once a day).
    pub segment: String,
    /// Timeframe display label (`2m`..`4h`; `1m` for 1m-side duplicates).
    pub tf: &'static str,
    /// The bucket OPEN time (IST nanoseconds).
    pub bucket_ts_ist_nanos: i64,
    pub category: FindingCategory,
    /// `open` / `high` / `low` / `close` / `volume` / `n/a`.
    pub field: &'static str,
    /// Stored price-cell value (0.0 for volume / presence findings).
    pub stored_value: f64,
    /// Recomputed price-cell value (0.0 for volume / presence findings).
    pub recomputed_value: f64,
    /// Stored volume (exact i64 — never widened to f64).
    pub stored_volume: i64,
    /// Recomputed Σ(1m volume) (exact i64).
    pub recomputed_volume: i64,
    /// 1m member rows inside the window (coverage context).
    pub one_m_rows: i64,
}

/// The idempotent `CREATE TABLE` DDL for `tf_consistency_audit`. Pure.
#[must_use]
pub fn tf_consistency_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {TF_CONSISTENCY_AUDIT_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            feed             SYMBOL, \
            security_id      LONG, \
            segment          SYMBOL, \
            tf               SYMBOL, \
            bucket_ts_ist    TIMESTAMP, \
            category         SYMBOL, \
            field            SYMBOL, \
            stored_value     DOUBLE, \
            recomputed_value DOUBLE, \
            stored_volume    LONG, \
            recomputed_volume LONG, \
            one_m_rows       LONG\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_TF_CONSISTENCY_AUDIT});"
    )
}

/// Create the `tf_consistency_audit` table if absent (idempotent
/// schema-self-heal order: CREATE → per-column `ALTER ADD COLUMN IF NOT
/// EXISTS` → DEDUP ENABLE — never a table drop). Greenfield table, the
/// writer always stamps `feed` ⇒ no NULL-feed backfill UPDATE (the
/// spot_1m_rest precedent).
///
/// Failures log at `error!` (code TF-VERIFY-02) but never block — NOTE the
/// HTTP-CLIENT-01-class consequence: a failed ensure leaves the table to be
/// auto-created by the first ILP write WITHOUT DEDUP UPSERT KEYS — a
/// duplicate-row window until a later ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via the create_ddl tests; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
pub async fn ensure_tf_consistency_audit_table(questdb_config: &QuestDbConfig) {
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
            metrics::counter!("tv_tf_verify_query_failures_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = ErrorCode::TfVerify02RunDegraded.code_str(),
                stage = "ensure_client_build",
                ?err,
                "TF-VERIFY-02: HTTP client build failed — tf_consistency_audit \
                 table not ensured (first ILP write may auto-create it WITHOUT \
                 dedup — duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![tf_consistency_audit_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern). QuestDB
    // ignores ADDs that already exist, so running every boot is free.
    for (col, ty) in [
        ("trading_date_ist", "TIMESTAMP"),
        ("feed", "SYMBOL"),
        ("security_id", "LONG"),
        ("segment", "SYMBOL"),
        ("tf", "SYMBOL"),
        ("bucket_ts_ist", "TIMESTAMP"),
        ("category", "SYMBOL"),
        ("field", "SYMBOL"),
        ("stored_value", "DOUBLE"),
        ("recomputed_value", "DOUBLE"),
        ("stored_volume", "LONG"),
        ("recomputed_volume", "LONG"),
        ("one_m_rows", "LONG"),
    ] {
        statements.push(format!(
            "ALTER TABLE {TF_CONSISTENCY_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {TF_CONSISTENCY_AUDIT_TABLE} DEDUP ENABLE \
         UPSERT KEYS({DEDUP_KEY_TF_CONSISTENCY_AUDIT});"
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
                metrics::counter!("tv_tf_verify_query_failures_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(code = ErrorCode::TfVerify02RunDegraded.code_str(),
                    stage = "ensure_ddl",
                    %status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "TF-VERIFY-02: tf_consistency_audit DDL returned non-2xx \
                     (dedup may be missing — duplicate-row window until a \
                     later ensure succeeds)");
            }
            Err(err) => {
                metrics::counter!("tv_tf_verify_query_failures_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::TfVerify02RunDegraded.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "TF-VERIFY-02: tf_consistency_audit DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf for the audit writer — per-flush server ACK (the
/// 2026-07-05 fire-and-forget lesson) with the shadow-candle-writer knobs:
/// `retry_timeout=0` (the questdb-rs internal 10s sleep-and-resend loop is
/// disabled — the once-a-day run owns retry cadence) + `request_timeout=
/// 5000` ms (bounds a hung flush).
fn tf_consistency_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `tf_consistency_audit`. Mirrors
/// `Spot1mRestWriter`: unreachable QuestDB at construction still builds
/// (rows buffer locally); `flush` returns `Err` — incl. server-side rejects
/// via the HTTP ACK — and the pending buffer is DISCARDED on failure
/// (poisoned-buffer defense; the findings are recomputable + reruns are
/// DEDUP-idempotent).
pub struct TfConsistencyAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl TfConsistencyAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_tf_verify_writer_new_is_lazy...); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = tf_consistency_ilp_http_conf(config);
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
                    "tf_consistency_audit writer: QuestDB unreachable — buffering locally"
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

    /// Appends one finding row (cold path, once a day, capped upstream at
    /// `TF_VERIFY_MAX_AUDIT_ROWS_PER_RUN`).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_finding(&mut self, f: &TfConsistencyFinding) -> Result<()> {
        self.buffer
            .table(TF_CONSISTENCY_AUDIT_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", f.feed)
            .context("feed")?
            .symbol("segment", f.segment.as_str())
            .context("segment")?
            .symbol("tf", f.tf)
            .context("tf")?
            .symbol("category", f.category.as_str())
            .context("category")?
            .symbol("field", f.field)
            .context("field")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(f.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", f.security_id)
            .context("security_id")?
            .column_ts("bucket_ts_ist", TimestampNanos::new(f.bucket_ts_ist_nanos))
            .context("bucket_ts_ist")?
            .column_f64("stored_value", f.stored_value)
            .context("stored_value")?
            .column_f64("recomputed_value", f.recomputed_value)
            .context("recomputed_value")?
            .column_i64("stored_volume", f.stored_volume)
            .context("stored_volume")?
            .column_i64("recomputed_volume", f.recomputed_volume)
            .context("recomputed_volume")?
            .column_i64("one_m_rows", f.one_m_rows)
            .context("one_m_rows")?
            .at(TimestampNanos::new(f.run_ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK).
    ///
    /// On ANY failed flush the pending buffer is DISCARDED (the shadow-
    /// writer `discard_pending` precedent): a server-REJECTED row retained
    /// across flushes would replay forever. Findings are pure
    /// recomputations of stored data, so the durable floor is a rerun
    /// (DEDUP-idempotent) and the miss is LOUD (the caller emits
    /// TF-VERIFY-02 stage=flush_failed + the run reads Degraded).
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
                "tf_consistency_audit: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending finding(s) discarded (recomputable, DEDUP-idempotent rerun)"
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
                    "tf_consistency_audit ILP flush failed — {dropped} pending \
                     finding(s) discarded (poisoned-buffer defense; rerun is \
                     DEDUP-idempotent)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!(
                    "tf_consistency_audit: ILP sender vanished — {dropped} finding(s) discarded"
                );
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted by
    /// `tv_tf_verify_audit_rows_discarded_total` so a discard is never
    /// silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_tf_verify_audit_rows_discarded_total").increment(dropped as u64);
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

    fn sample_finding() -> TfConsistencyFinding {
        TfConsistencyFinding {
            // 2026-07-13 15:40:00 IST-as-epoch — the deterministic run ts
            // (2026-07-13 00:00:00 IST-as-epoch midnight = 1_783_900_800s;
            // + 56_400s = 15:40:00). Corrected refuter round 3: the prior
            // pair was neither the claimed instant nor an IST midnight.
            run_ts_ist_nanos: 1_783_957_200_000_000_000,
            trading_date_ist_nanos: 1_783_900_800_000_000_000,
            feed: "dhan",
            security_id: 13,
            segment: "IDX_I".to_string(),
            tf: "5m",
            bucket_ts_ist_nanos: 1_783_900_800_000_000_000 + 33_300 * 1_000_000_000,
            category: FindingCategory::Mismatch,
            field: "high",
            stored_value: 25_647.5,
            recomputed_value: 25_647.0,
            stored_volume: 0,
            recomputed_volume: 0,
            one_m_rows: 5,
        }
    }

    #[test]
    fn test_tf_consistency_audit_create_ddl_contains_expected_columns() {
        let ddl = tf_consistency_audit_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "feed",
            "security_id",
            "segment",
            "tf ",
            "bucket_ts_ist",
            "category",
            "field",
            "stored_value",
            "recomputed_value",
            "stored_volume",
            "recomputed_volume",
            "one_m_rows",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_TF_CONSISTENCY_AUDIT})"
        )));
    }

    /// DEDUP-key discipline: designated `ts` FIRST (2026-04-28 regression
    /// rule for `*_AUDIT` tables); `segment` alongside `security_id`
    /// (I-P1-11); `feed` in-key (operator override 2026-06-28) — whole-token
    /// matches; category + field keep one row per finding cell.
    #[test]
    fn test_tf_consistency_dedup_key_ts_first_segment_and_feed_in_key() {
        assert!(
            DEDUP_KEY_TF_CONSISTENCY_AUDIT
                .trim_start()
                .starts_with("ts,")
        );
        let has_token = |t: &str| {
            DEDUP_KEY_TF_CONSISTENCY_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        for tok in [
            "ts",
            "trading_date_ist",
            "feed",
            "security_id",
            "segment",
            "tf",
            "bucket_ts_ist",
            "category",
            "field",
        ] {
            assert!(has_token(tok), "DEDUP key missing token {tok}");
        }
        // Exactly the 9 tokens above.
        assert_eq!(DEDUP_KEY_TF_CONSISTENCY_AUDIT.matches(',').count() + 1, 9);
    }

    #[test]
    fn test_finding_category_as_str_labels_are_stable() {
        assert_eq!(FindingCategory::Mismatch.as_str(), "mismatch");
        assert_eq!(FindingCategory::MissingTfRow.as_str(), "missing_tf_row");
        assert_eq!(FindingCategory::No1mCoverage.as_str(), "no_1m_coverage");
        assert_eq!(FindingCategory::OffGridTs.as_str(), "off_grid_ts");
        assert_eq!(FindingCategory::DuplicateKey.as_str(), "duplicate_key");
    }

    #[test]
    fn test_append_finding_writes_symbols_and_columns() {
        let mut w = TfConsistencyAuditWriter::for_test();
        w.append_finding(&sample_finding())
            .expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(TF_CONSISTENCY_AUDIT_TABLE));
        assert!(line.contains(",feed=dhan"), "feed tag missing: {line}");
        assert!(
            line.contains(",segment=IDX_I"),
            "segment tag missing: {line}"
        );
        assert!(line.contains(",tf=5m"), "tf tag missing: {line}");
        assert!(
            line.contains(",category=mismatch"),
            "category tag missing: {line}"
        );
        assert!(line.contains(",field=high"), "field tag missing: {line}");
        assert!(line.contains("security_id=13i"), "sid missing: {line}");
        assert!(line.contains("one_m_rows=5i"), "context missing: {line}");
        assert!(
            line.contains("stored_volume=0i") && line.contains("recomputed_volume=0i"),
            "exact-i64 volume columns missing: {line}"
        );
    }

    #[test]
    fn test_append_finding_both_feeds_coexist_in_one_buffer() {
        let mut w = TfConsistencyAuditWriter::for_test();
        let dhan = sample_finding();
        let mut groww = sample_finding();
        groww.feed = "groww";
        groww.category = FindingCategory::MissingTfRow;
        groww.field = "n/a";
        w.append_finding(&dhan).expect("dhan append");
        w.append_finding(&groww).expect("groww append");
        assert_eq!(w.pending(), 2);
        let line = w.buffer_utf8();
        assert!(line.contains(",feed=dhan"));
        assert!(line.contains(",feed=groww"));
        assert!(line.contains(",category=missing_tf_row"));
    }

    #[test]
    fn test_tf_verify_flush_when_disconnected_errors_and_discards_pending() {
        // Poisoned-buffer defense: a failed flush DISCARDS the pending
        // buffer — one rejected row can never wedge later runs; findings
        // are recomputable + reruns are DEDUP-idempotent.
        let mut w = TfConsistencyAuditWriter::for_test();
        w.append_finding(&sample_finding()).expect("append");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert!(err.to_string().contains("discarded"));
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
        // Empty flush is a no-op Ok (the skip-when-empty final-flush rule).
        let mut empty = TfConsistencyAuditWriter::for_test();
        assert!(empty.flush().is_ok());
    }

    #[test]
    fn test_tf_verify_discard_pending_clears_buffer_and_count() {
        let mut w = TfConsistencyAuditWriter::for_test();
        w.append_finding(&sample_finding()).expect("append");
        w.append_finding(&sample_finding()).expect("append");
        assert_eq!(w.pending(), 2);
        assert!(!w.buffer_utf8().is_empty());
        assert_eq!(w.discard_pending(), 2, "returns the discarded count");
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty());
        // Idempotent: a second discard drops nothing.
        assert_eq!(w.discard_pending(), 0);
    }

    /// Transport ratchet (2026-07-05 fire-and-forget lesson): ILP-over-HTTP
    /// with the shadow-candle-writer knobs — never ILP TCP 9009.
    #[test]
    fn test_tf_verify_writer_uses_ilp_http_conf_with_bounded_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = tf_consistency_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
        assert!(!conf.contains("tcp::"), "must not use ILP TCP: {conf}");
    }

    // ========================================================================
    // Ensure-DDL tests — mock QuestDB /exec HTTP server + unreachable host
    // (the spot_1m_rest_persistence P2C pattern). These exercise the real
    // ensure code paths: success (200), non-2xx (500) and transport-error.
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
    async fn test_ensure_tf_consistency_audit_table_mock_200_completes() {
        // Success path: the CREATE + every ADD COLUMN self-heal + the DEDUP
        // ENABLE all take the Ok(2xx) arm.
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_tf_consistency_audit_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_tf_consistency_audit_table_mock_500_degrades_without_panic() {
        // Non-2xx path: every DDL statement takes the log-and-continue arm
        // (best-effort degrade — TF-VERIFY-02, never a panic, never blocks).
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_tf_consistency_audit_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_tf_consistency_audit_table_unreachable_degrades_without_panic() {
        // Transport-error path: every DDL send Err arm logs and continues.
        ensure_tf_consistency_audit_table(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_tf_verify_writer_new_is_lazy_and_buffers_without_network() {
        // `Sender::from_conf` with `http::` does not dial at construction
        // (the ws_event_audit precedent), so new() against an unreachable
        // host still builds a sender-backed writer whose appends land in
        // the local buffer — the lazy-construction contract.
        let mut w = TfConsistencyAuditWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_finding(&sample_finding())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }
}
