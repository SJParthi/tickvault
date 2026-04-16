//! QuestDB ILP persistence for indicator snapshots.
//!
//! Stores computed indicator values (SMA, EMA, RSI, MACD, BB, ATR, etc.)
//! alongside 1-minute candle boundaries for Grafana visualization and
//! indicator warmup on restart.
//!
//! # Table Written
//! - `indicator_snapshots` — one row per security per minute with all indicator values
//!
//! # Idempotency
//! DEDUP UPSERT KEYS on `(ts, security_id, segment)` prevent duplicates on
//! restart. `segment` is required because the same `security_id` exists in
//! multiple exchange segments (IDX_I, NSE_EQ, NSE_FNO) — omitting it causes
//! silent cross-segment UPSERT collision. See audit gap DB-5.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::segment::segment_code_to_str;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// QuestDB table name for indicator snapshots.
pub const QUESTDB_TABLE_INDICATOR_SNAPSHOTS: &str = "indicator_snapshots";

/// DEDUP key for indicator snapshots.
const DEDUP_KEY_INDICATORS: &str = "security_id, segment";

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Flush batch size for indicator snapshots.
const INDICATOR_FLUSH_BATCH_SIZE: usize = 500;

/// Minimum interval between reconnect attempts when QuestDB is down.
///
/// Audit gap DB-6: the previous implementation called `Sender::from_conf`
/// on every failed flush with zero backoff, creating a tight reconnect loop
/// whenever QuestDB was unreachable. This constant caps reconnect attempts
/// at one per 30 seconds per writer, matching the pattern used by
/// `LiveCandleWriter` (see `LIVE_CANDLE_RECONNECT_THROTTLE_SECS`).
const INDICATOR_RECONNECT_THROTTLE_SECS: u64 = 30;

/// Rounds an f64 to 2 decimal places, matching Dhan's display precision.
/// Uses multiply-round-divide to avoid string conversion overhead.
/// O(1) — pure arithmetic, zero allocation.
#[inline]
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// DDL for the `indicator_snapshots` table.
///
/// Stores 1-minute indicator snapshots for all tracked instruments.
/// Partitioned by DAY (~25K instruments × 375 minutes = ~9.4M rows/day).
const INDICATOR_SNAPSHOTS_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS indicator_snapshots (\
        segment SYMBOL,\
        security_id LONG,\
        ema_fast DOUBLE,\
        ema_slow DOUBLE,\
        sma DOUBLE,\
        rsi DOUBLE,\
        macd_line DOUBLE,\
        macd_signal DOUBLE,\
        macd_histogram DOUBLE,\
        bollinger_upper DOUBLE,\
        bollinger_middle DOUBLE,\
        bollinger_lower DOUBLE,\
        atr DOUBLE,\
        supertrend DOUBLE,\
        supertrend_bullish BOOLEAN,\
        adx DOUBLE,\
        obv DOUBLE,\
        vwap DOUBLE,\
        ltp DOUBLE,\
        is_warm BOOLEAN,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Batched writer for indicator snapshots to QuestDB via ILP.
///
/// # Resilience (DB-1/DB-6/DB-7)
/// - Reconnect attempts are throttled to one per `INDICATOR_RECONNECT_THROTTLE_SECS`
///   (30s). A tight `Sender::from_conf` loop when QuestDB is down is prevented.
/// - Flush failures increment `tv_indicator_snapshot_flush_failures_total` and
///   log at ERROR level (triggers Telegram alert per `rust-code.md`).
/// - Dropped batches increment `tv_indicator_snapshot_dropped_total` as the
///   observability floor for data loss.
pub struct IndicatorSnapshotWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
    /// Earliest time (monotonic) at which the next reconnect attempt is allowed.
    /// Starts at `Instant::now()` (immediately eligible), bumped to
    /// `now + INDICATOR_RECONNECT_THROTTLE_SECS` after every failed reconnect.
    /// DB-6.
    next_reconnect_allowed: Instant,
    /// Total rows dropped (never written to QuestDB). Exposed via metric;
    /// non-zero in healthy ops = data loss alarm. DB-7.
    rows_dropped_total: u64,
}

impl IndicatorSnapshotWriter {
    /// Creates a new indicator snapshot writer connected to QuestDB via ILP TCP.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = config.build_ilp_conf_string();
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for indicator snapshots")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
            next_reconnect_allowed: Instant::now(),
            rows_dropped_total: 0,
        })
    }

    /// Total rows dropped since startup (never persisted to QuestDB).
    /// Exposed for tests and observability; in prod this is also exported
    /// as a Prometheus metric `tv_indicator_snapshot_dropped_total`.
    pub fn rows_dropped_total(&self) -> u64 {
        self.rows_dropped_total
    }

    /// Records a dropped batch — increments counter + metric + ERROR log.
    fn record_drop(&mut self, count: usize, reason: &'static str, err: &anyhow::Error) {
        let dropped = u64::try_from(count).unwrap_or(u64::MAX);
        self.rows_dropped_total = self.rows_dropped_total.saturating_add(dropped);
        metrics::counter!("tv_indicator_snapshot_dropped_total").absolute(self.rows_dropped_total);
        metrics::counter!("tv_indicator_snapshot_flush_failures_total").increment(1);
        error!(
            ?err,
            dropped_rows = count,
            total_dropped = self.rows_dropped_total,
            reason,
            "CRITICAL: indicator snapshot batch dropped — data loss"
        );
    }

    /// Returns whether a reconnect attempt is currently allowed (throttle window).
    fn reconnect_allowed_now(&self) -> bool {
        Instant::now() >= self.next_reconnect_allowed
    }

    /// Bumps the reconnect throttle forward by `INDICATOR_RECONNECT_THROTTLE_SECS`.
    fn bump_reconnect_throttle(&mut self) {
        self.next_reconnect_allowed =
            Instant::now() + Duration::from_secs(INDICATOR_RECONNECT_THROTTLE_SECS);
    }

    /// Appends a single indicator snapshot row to the ILP buffer.
    // TEST-EXEMPT: ILP buffer append — requires live QuestDB
    #[allow(clippy::too_many_arguments)] // APPROVED: 21 params — each maps to a QuestDB column for all indicator values
    pub fn append_snapshot(
        &mut self,
        ts_nanos: i64,
        security_id: u32,
        segment_code: u8,
        ema_fast: f64,
        ema_slow: f64,
        sma: f64,
        rsi: f64,
        macd_line: f64,
        macd_signal: f64,
        macd_histogram: f64,
        bollinger_upper: f64,
        bollinger_middle: f64,
        bollinger_lower: f64,
        atr: f64,
        supertrend: f64,
        supertrend_bullish: bool,
        adx: f64,
        obv: f64,
        vwap: f64,
        ltp: f64,
        is_warm: bool,
    ) -> Result<()> {
        let ts = TimestampNanos::new(ts_nanos);

        self.buffer
            .table(QUESTDB_TABLE_INDICATOR_SNAPSHOTS)
            .context("table")?
            .symbol("segment", segment_code_to_str(segment_code))
            .context("segment")?
            .column_i64("security_id", i64::from(security_id))
            .context("security_id")?
            // Round all indicator values to 2 decimal places to match Dhan's
            // display precision. Raw f64 computation produces many decimal digits
            // (e.g. 20923.60785379...) but Dhan shows max 2 decimals.
            .column_f64("ema_fast", round2(ema_fast))
            .context("ema_fast")?
            .column_f64("ema_slow", round2(ema_slow))
            .context("ema_slow")?
            .column_f64("sma", round2(sma))
            .context("sma")?
            .column_f64("rsi", round2(rsi))
            .context("rsi")?
            .column_f64("macd_line", round2(macd_line))
            .context("macd_line")?
            .column_f64("macd_signal", round2(macd_signal))
            .context("macd_signal")?
            .column_f64("macd_histogram", round2(macd_histogram))
            .context("macd_histogram")?
            .column_f64("bollinger_upper", round2(bollinger_upper))
            .context("bollinger_upper")?
            .column_f64("bollinger_middle", round2(bollinger_middle))
            .context("bollinger_middle")?
            .column_f64("bollinger_lower", round2(bollinger_lower))
            .context("bollinger_lower")?
            .column_f64("atr", round2(atr))
            .context("atr")?
            .column_f64("supertrend", round2(supertrend))
            .context("supertrend")?
            .column_bool("supertrend_bullish", supertrend_bullish)
            .context("supertrend_bullish")?
            .column_f64("adx", round2(adx))
            .context("adx")?
            .column_f64("obv", round2(obv))
            .context("obv")?
            .column_f64("vwap", round2(vwap))
            .context("vwap")?
            .column_f64("ltp", round2(ltp))
            .context("ltp")?
            .column_bool("is_warm", is_warm)
            .context("is_warm")?
            .at(ts)
            .context("timestamp")?;

        self.pending_count = self.pending_count.saturating_add(1);

        // Auto-flush on batch boundary. If this fails, the row is already
        // buffered but the flush path has already recorded the drop + metric
        // + ERROR log; we return Ok here because the append itself succeeded
        // and the error has been observably recorded. DB-7.
        if self.pending_count >= INDICATOR_FLUSH_BATCH_SIZE
            && let Err(err) = self.flush()
        {
            // flush() already called record_drop + ERROR-logged. This is a
            // breadcrumb for the auto-flush boundary specifically.
            error!(
                ?err,
                "indicator snapshot auto-flush failed at batch boundary"
            );
        }

        Ok(())
    }

    /// Flushes buffered rows to QuestDB.
    ///
    /// # Resilience contract (DB-1/DB-6/DB-7)
    /// - Reconnect attempts are throttled to one per
    ///   `INDICATOR_RECONNECT_THROTTLE_SECS`. When the throttle is engaged,
    ///   the current batch is dropped (as before) but the drop is recorded
    ///   against `tv_indicator_snapshot_dropped_total` + ERROR log.
    /// - Every drop path increments `tv_indicator_snapshot_flush_failures_total`.
    /// - Drops log at ERROR level (triggers Telegram alert) instead of the
    ///   previous WARN.
    ///
    /// A follow-up audit item (DB-1) will add a bounded in-memory ring that
    /// rescues dropped batches instead of discarding them outright. Until
    /// that lands, this function's best behaviour is "make every drop loud
    /// and observable" — which is strictly better than the prior "silently
    /// return Ok(()) on WARN".
    pub fn flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        // Reconnect path — throttled.
        if self.sender.is_none() {
            if !self.reconnect_allowed_now() {
                // Throttle window is still closed. Drop the batch loudly.
                let count = self.pending_count;
                let err = anyhow::anyhow!(
                    "reconnect throttled ({}s window not elapsed)",
                    INDICATOR_RECONNECT_THROTTLE_SECS
                );
                self.record_drop(count, "reconnect_throttled", &err);
                self.buffer.clear();
                self.pending_count = 0;
                return Ok(());
            }
            // Attempt reconnect. Bump the throttle BEFORE the attempt so a
            // failure immediately holds off the next try by the full window.
            self.bump_reconnect_throttle();
            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(s) => {
                    info!("indicator snapshot writer reconnected to QuestDB");
                    self.sender = Some(s);
                }
                Err(err) => {
                    let count = self.pending_count;
                    let wrapped = anyhow::Error::from(err);
                    self.record_drop(count, "reconnect_failed", &wrapped);
                    self.buffer.clear();
                    self.pending_count = 0;
                    return Ok(());
                }
            }
        }

        let sender = self
            .sender
            .as_mut()
            .context("sender present after reconnect branch")?;
        let count = self.pending_count;
        if let Err(err) = sender.flush(&mut self.buffer) {
            let wrapped = anyhow::Error::from(err);
            self.record_drop(count, "flush_failed", &wrapped);
            // Drop the sender so the next flush attempts a reconnect
            // (throttled by the window above).
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            return Ok(());
        }

        self.pending_count = 0;
        debug!(
            flushed_rows = count,
            "indicator snapshots flushed to QuestDB"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DDL Setup
// ---------------------------------------------------------------------------

/// Creates the `indicator_snapshots` table with DEDUP UPSERT KEYS.
pub async fn ensure_indicator_snapshot_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    // Create table
    match client
        .get(&base_url)
        .query(&[("query", INDICATOR_SNAPSHOTS_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("indicator_snapshots table ensured");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(%status, body = body.chars().take(200).collect::<String>(), "indicator_snapshots DDL non-success");
            }
        }
        Err(err) => {
            warn!(?err, "indicator_snapshots DDL request failed");
            return;
        }
    }

    // Enable DEDUP
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_INDICATOR_SNAPSHOTS, DEDUP_KEY_INDICATORS
    );
    match client
        .get(&base_url)
        .query(&[("query", &dedup_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("indicator_snapshots DEDUP UPSERT KEYS enabled");
            } else {
                let body = response.text().await.unwrap_or_default();
                if body.contains("deduplicate key column not found") {
                    warn!("indicator_snapshots has stale schema — dropping and recreating");
                    let drop_sql =
                        format!("DROP TABLE IF EXISTS {QUESTDB_TABLE_INDICATOR_SNAPSHOTS}");
                    let _ = client
                        .get(&base_url)
                        .query(&[("query", &drop_sql)])
                        .send()
                        .await;
                    let _ = client
                        .get(&base_url)
                        .query(&[("query", INDICATOR_SNAPSHOTS_DDL)])
                        .send()
                        .await;
                    let _ = client
                        .get(&base_url)
                        .query(&[("query", &dedup_sql)])
                        .send()
                        .await;
                    info!("indicator_snapshots recreated with correct schema");
                } else {
                    warn!(
                        body = body.chars().take(200).collect::<String>(),
                        "indicator_snapshots DEDUP non-success"
                    );
                }
            }
        }
        Err(err) => {
            warn!(?err, "indicator_snapshots DEDUP request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_indicator_snapshots_ddl_contains_all_columns() {
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("ema_fast DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("ema_slow DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("sma DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("rsi DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("macd_line DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("macd_signal DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("macd_histogram DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("bollinger_upper DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("bollinger_middle DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("bollinger_lower DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("atr DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("supertrend DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("supertrend_bullish BOOLEAN"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("adx DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("obv DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("vwap DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("ltp DOUBLE"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("is_warm BOOLEAN"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("ts TIMESTAMP"));
    }

    #[test]
    fn test_indicator_snapshots_ddl_has_partition_and_wal() {
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("PARTITION BY DAY"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("WAL"));
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_indicator_snapshots_ddl_idempotent() {
        assert!(INDICATOR_SNAPSHOTS_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_table_name_stable() {
        assert_eq!(QUESTDB_TABLE_INDICATOR_SNAPSHOTS, "indicator_snapshots");
    }

    #[test]
    fn test_dedup_key_includes_security_id() {
        assert!(DEDUP_KEY_INDICATORS.contains("security_id"));
    }

    #[test]
    fn test_flush_batch_size_reasonable() {
        assert!(INDICATOR_FLUSH_BATCH_SIZE >= 100);
        assert!(INDICATOR_FLUSH_BATCH_SIZE <= 2000);
    }

    #[test]
    fn test_writer_invalid_host() {
        let config = QuestDbConfig {
            host: "nonexistent-host-99999".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let _result = IndicatorSnapshotWriter::new(&config);
    }

    // -----------------------------------------------------------------------
    // DB-5: doc-comment truthfulness
    // -----------------------------------------------------------------------

    /// The module-level doc comment claims the DEDUP key is
    /// `(ts, security_id, segment)`. The actual constant must match.
    /// Prevents the doc-drift regression bait flagged in audit gap DB-5.
    #[test]
    fn test_db5_dedup_key_matches_doc_comment() {
        assert!(
            DEDUP_KEY_INDICATORS.contains("security_id")
                && DEDUP_KEY_INDICATORS.contains("segment"),
            "DEDUP_KEY_INDICATORS must include security_id AND segment to \
             match the module doc comment (DB-5). Got: {DEDUP_KEY_INDICATORS}"
        );
    }

    #[test]
    fn test_db5_dedup_key_exact_format() {
        assert_eq!(
            DEDUP_KEY_INDICATORS, "security_id, segment",
            "DEDUP_KEY_INDICATORS regression — if you need to change this, \
             also update the doc comment and add a migration plan."
        );
    }

    // -----------------------------------------------------------------------
    // DB-6: reconnect throttle
    // -----------------------------------------------------------------------

    /// The reconnect throttle constant must be non-zero. A zero throttle =
    /// tight reconnect loop = the original DB-6 bug.
    #[test]
    fn test_db6_reconnect_throttle_is_nonzero() {
        assert!(
            INDICATOR_RECONNECT_THROTTLE_SECS >= 1,
            "reconnect throttle must be >= 1s to prevent tight reconnect loops"
        );
    }

    /// The throttle must be bounded sanely (at least 1s, at most 5 min).
    /// Too low = DoS QuestDB with reconnects; too high = bad recovery.
    #[test]
    fn test_db6_reconnect_throttle_bounded() {
        assert!(INDICATOR_RECONNECT_THROTTLE_SECS >= 1);
        assert!(INDICATOR_RECONNECT_THROTTLE_SECS <= 300);
    }

    // -----------------------------------------------------------------------
    // DB-7: flush-failure observability (drop counter + reconnect throttle state)
    // -----------------------------------------------------------------------

    /// Proves `next_reconnect_allowed` advances forward on a failed
    /// reconnect and that `reconnect_allowed_now()` returns false inside
    /// the throttle window. No live QuestDB needed — operates on the
    /// struct's internal state.
    #[test]
    fn test_db7_reconnect_throttle_blocks_within_window() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        // We don't actually connect — use a test-only construction path
        // that mirrors `new` but skips the Sender. We do this by calling
        // `new` with a valid host-port; if it happens to succeed we
        // immediately drop the sender. If it fails, we synthesize the
        // struct directly below.
        let writer_result = IndicatorSnapshotWriter::new(&config);
        let mut writer = match writer_result {
            Ok(mut w) => {
                // Drop the real sender so the reconnect path engages.
                w.sender = None;
                w
            }
            Err(_) => {
                // Build the writer struct manually for test purposes.
                // This is a test-only synthesis path — production code
                // cannot do this because the fields are private.
                IndicatorSnapshotWriter {
                    sender: None,
                    buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                    pending_count: 0,
                    ilp_conf_string: config.build_ilp_conf_string(),
                    next_reconnect_allowed: Instant::now(),
                    rows_dropped_total: 0,
                }
            }
        };

        // Initially allowed.
        assert!(
            writer.reconnect_allowed_now(),
            "freshly-constructed writer must allow immediate reconnect"
        );

        // Bump the throttle — now disallowed for ~30s.
        writer.bump_reconnect_throttle();
        assert!(
            !writer.reconnect_allowed_now(),
            "after bump, reconnect must be blocked inside the throttle window \
             (DB-6 invariant)"
        );
    }

    /// `record_drop` must increment the rows_dropped_total counter by the
    /// exact dropped-row count — proves no off-by-one and no lossy cast.
    #[test]
    fn test_db7_record_drop_increments_counter() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let mut writer = match IndicatorSnapshotWriter::new(&config) {
            Ok(w) => w,
            Err(_) => IndicatorSnapshotWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: 0,
            },
        };
        assert_eq!(writer.rows_dropped_total(), 0);
        let err = anyhow::anyhow!("synthetic flush failure");
        writer.record_drop(500, "test", &err);
        assert_eq!(
            writer.rows_dropped_total(),
            500,
            "record_drop must increment the counter by exactly the dropped count"
        );
        writer.record_drop(250, "test", &err);
        assert_eq!(writer.rows_dropped_total(), 750);
    }

    /// Pub-fn coverage: the `rows_dropped_total()` getter starts at zero
    /// on a freshly-constructed (or test-synthesized) writer.
    #[test]
    fn test_db7_rows_dropped_total_starts_zero() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let writer = match IndicatorSnapshotWriter::new(&config) {
            Ok(w) => w,
            Err(_) => IndicatorSnapshotWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: 0,
            },
        };
        assert_eq!(writer.rows_dropped_total(), 0);
    }

    /// Saturating arithmetic: an impossibly-large drop count must not
    /// overflow `rows_dropped_total`.
    #[test]
    fn test_db7_record_drop_saturates_on_overflow() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let mut writer = match IndicatorSnapshotWriter::new(&config) {
            Ok(w) => w,
            Err(_) => IndicatorSnapshotWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: u64::MAX - 10,
            },
        };
        // Force start near the ceiling.
        writer.rows_dropped_total = u64::MAX - 10;
        let err = anyhow::anyhow!("synthetic overflow");
        writer.record_drop(100, "overflow", &err);
        assert_eq!(
            writer.rows_dropped_total(),
            u64::MAX,
            "saturating_add must clamp at u64::MAX, not wrap"
        );
    }
}
