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

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
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

/// Capacity of the in-memory rescue ring (DB-1).
///
/// When QuestDB is down or the flush fails, up to this many buffered
/// snapshots are held in memory so they can be drained FIFO on reconnect.
/// At ~160 bytes per snapshot, a 20,000-entry ring is ~3.2 MB, bounded and
/// affordable. 20K snapshots covers ~40 minutes of ingestion at the
/// normal rate of 500 rows/minute — more than enough to survive a QuestDB
/// restart without data loss. On overflow, the OLDEST snapshots are
/// evicted (FIFO) and counted against `rows_dropped_total`.
const INDICATOR_RESCUE_RING_CAPACITY: usize = 20_000;

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
// Rescue ring record (DB-1)
// ---------------------------------------------------------------------------

/// A buffered indicator snapshot, kept in an in-memory FIFO rescue ring
/// so it can be re-sent to QuestDB after a flush failure or reconnect.
///
/// **All 21 columns are stored verbatim** so the re-send path can
/// reconstruct the ILP row byte-for-byte identical to the original.
/// Dedup keys `(ts, security_id, segment)` make the re-send idempotent
/// at the QuestDB layer, so over-sending on spurious reconnect is safe.
///
/// `Copy` is implemented so the ring push is zero-alloc. Size is
/// 4 + 4 + 1 + 17*8 + 2*1 + 8 = 151 bytes (padded to ~160 bytes by the
/// compiler's natural alignment), so a 20,000-entry ring is ~3.2 MB —
/// bounded and affordable.
#[derive(Clone, Copy)]
struct BufferedSnapshot {
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
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Batched writer for indicator snapshots to QuestDB via ILP.
///
/// # Resilience (DB-1/DB-6/DB-7)
/// - **Bounded rescue ring** (`INDICATOR_RESCUE_RING_CAPACITY = 20_000`):
///   every successfully-appended snapshot is pushed to an in-memory FIFO
///   ring in parallel with the ILP buffer. On successful flush, the
///   corresponding prefix is popped (those rows are durable). On failed
///   flush, the ring retains the rows; on reconnect, the ring is drained
///   FIFO and each snapshot is re-sent, so no rows are dropped during a
///   transient QuestDB outage.
/// - **Reconnect throttle** (30s): a tight `Sender::from_conf` loop is
///   prevented when QuestDB flaps.
/// - **Drop observability**: on ring overflow, the oldest row is evicted
///   and counted as `rows_dropped_total` + `tv_indicator_snapshot_dropped_total`
///   + ERROR log (Telegram alert path). In healthy ops this must be zero.
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
    /// Bounded FIFO ring of buffered snapshots for rescue on flush failure
    /// and reconnect drain. DB-1.
    // O(1) EXEMPT: begin — VecDeque allocation bounded by INDICATOR_RESCUE_RING_CAPACITY
    rescue_ring: VecDeque<BufferedSnapshot>,
    // O(1) EXEMPT: end
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
            rescue_ring: VecDeque::with_capacity(INDICATOR_RESCUE_RING_CAPACITY),
        })
    }

    /// Current rescue-ring occupancy (for tests + observability).
    pub fn rescue_ring_len(&self) -> usize {
        self.rescue_ring.len()
    }

    /// Pushes a snapshot into the rescue ring, evicting the oldest entry
    /// if the ring is full. Overflow eviction routes through
    /// [`Self::record_drop`] so the observability path is centralized
    /// (one counter, one metric, one ERROR log for every drop reason).
    /// DB-1.
    fn push_to_rescue_ring(&mut self, snapshot: BufferedSnapshot) {
        if self.rescue_ring.len() >= INDICATOR_RESCUE_RING_CAPACITY {
            // Overflow: evict oldest.
            self.rescue_ring.pop_front();
            let err = anyhow::anyhow!(
                "indicator snapshot rescue ring full at capacity {} — oldest row evicted",
                INDICATOR_RESCUE_RING_CAPACITY
            );
            self.record_drop(1, "ring_overflow", &err);
            // Also increment the specialized overflow counter so dashboards
            // can distinguish ring-overflow from other drop reasons.
            metrics::counter!("tv_indicator_snapshot_ring_overflow_total").increment(1);
        }
        self.rescue_ring.push_back(snapshot);
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

    /// Writes a `BufferedSnapshot` into an ILP `Buffer` (helper for both
    /// the live append path and the rescue-drain path). DB-1.
    ///
    /// Factored out so `append_snapshot` and `drain_rescue_ring_to_buffer`
    /// cannot drift apart on column order/rounding policy. If you edit one,
    /// you MUST edit the other; the single source of truth is this helper.
    fn append_snapshot_row_to_buffer(buffer: &mut Buffer, snap: &BufferedSnapshot) -> Result<()> {
        let ts = TimestampNanos::new(snap.ts_nanos);
        buffer
            .table(QUESTDB_TABLE_INDICATOR_SNAPSHOTS)
            .context("table")?
            .symbol("segment", segment_code_to_str(snap.segment_code))
            .context("segment")?
            .column_i64("security_id", i64::from(snap.security_id))
            .context("security_id")?
            // All indicator values are rounded to 2 decimal places to match
            // Dhan's display precision; re-rounding on rescue drain is a
            // no-op (round2(round2(x)) == round2(x)).
            .column_f64("ema_fast", round2(snap.ema_fast))
            .context("ema_fast")?
            .column_f64("ema_slow", round2(snap.ema_slow))
            .context("ema_slow")?
            .column_f64("sma", round2(snap.sma))
            .context("sma")?
            .column_f64("rsi", round2(snap.rsi))
            .context("rsi")?
            .column_f64("macd_line", round2(snap.macd_line))
            .context("macd_line")?
            .column_f64("macd_signal", round2(snap.macd_signal))
            .context("macd_signal")?
            .column_f64("macd_histogram", round2(snap.macd_histogram))
            .context("macd_histogram")?
            .column_f64("bollinger_upper", round2(snap.bollinger_upper))
            .context("bollinger_upper")?
            .column_f64("bollinger_middle", round2(snap.bollinger_middle))
            .context("bollinger_middle")?
            .column_f64("bollinger_lower", round2(snap.bollinger_lower))
            .context("bollinger_lower")?
            .column_f64("atr", round2(snap.atr))
            .context("atr")?
            .column_f64("supertrend", round2(snap.supertrend))
            .context("supertrend")?
            .column_bool("supertrend_bullish", snap.supertrend_bullish)
            .context("supertrend_bullish")?
            .column_f64("adx", round2(snap.adx))
            .context("adx")?
            .column_f64("obv", round2(snap.obv))
            .context("obv")?
            .column_f64("vwap", round2(snap.vwap))
            .context("vwap")?
            .column_f64("ltp", round2(snap.ltp))
            .context("ltp")?
            .column_bool("is_warm", snap.is_warm)
            .context("is_warm")?
            .at(ts)
            .context("timestamp")?;
        Ok(())
    }

    /// Appends a single indicator snapshot row to the ILP buffer.
    ///
    /// Also pushes the same snapshot into the rescue ring so it can be
    /// re-sent on reconnect after a flush failure. DB-1.
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
        let snapshot = BufferedSnapshot {
            ts_nanos,
            security_id,
            segment_code,
            ema_fast,
            ema_slow,
            sma,
            rsi,
            macd_line,
            macd_signal,
            macd_histogram,
            bollinger_upper,
            bollinger_middle,
            bollinger_lower,
            atr,
            supertrend,
            supertrend_bullish,
            adx,
            obv,
            vwap,
            ltp,
            is_warm,
        };

        // DB-1: push to rescue ring first. If the ring is full, the oldest
        // row is evicted (and counted as a drop). The ring push is O(1)
        // amortized and never allocates on the hot path (ring capacity is
        // pre-allocated in the constructor).
        self.push_to_rescue_ring(snapshot);

        // Construct the ILP row. If the buffer call fails (very rare —
        // would indicate a protocol-level error), we return Err to the
        // caller but the snapshot is still safe in the rescue ring.
        Self::append_snapshot_row_to_buffer(&mut self.buffer, &snapshot)?;

        self.pending_count = self.pending_count.saturating_add(1);

        // Auto-flush on batch boundary. If this fails, the ring still has
        // the rows and the flush path has recorded the drop+metric+ERROR
        // log; we return Ok here because the append itself succeeded and
        // the data is still durable-in-memory.
        if self.pending_count >= INDICATOR_FLUSH_BATCH_SIZE
            && let Err(err) = self.flush()
        {
            error!(
                ?err,
                "indicator snapshot auto-flush failed at batch boundary"
            );
        }

        Ok(())
    }

    /// Drains the rescue ring by re-appending every buffered snapshot to
    /// a fresh ILP buffer and flushing. Called after a successful
    /// reconnect. FIFO order — oldest first. DB-1.
    ///
    /// On flush failure mid-drain, the remaining rows stay in the ring
    /// (`pop_front` only pops on successful row-append) and the caller's
    /// normal failure path handles the rest.
    fn drain_rescue_ring_to_sender(&mut self) -> Result<()> {
        if self.rescue_ring.is_empty() {
            return Ok(());
        }
        let drained = self.rescue_ring.len();
        info!(
            buffered = drained,
            "draining indicator snapshot rescue ring after reconnect"
        );

        // Build a single ILP batch from the ring.
        let sender = self
            .sender
            .as_mut()
            .context("sender required for rescue ring drain")?;
        let mut rescue_buffer = Buffer::new(ProtocolVersion::V1);
        for snap in self.rescue_ring.iter() {
            Self::append_snapshot_row_to_buffer(&mut rescue_buffer, snap)?;
        }

        // Flush the rescue batch. On success, we pop the entire range and
        // the ring is empty. On failure, we leave the ring untouched so
        // the next reconnect can try again.
        sender
            .flush(&mut rescue_buffer)
            .context("flush indicator snapshot rescue batch to QuestDB")?;

        // Successful — clear the ring.
        self.rescue_ring.clear();
        info!(
            drained,
            "indicator snapshot rescue ring drained after reconnect"
        );
        Ok(())
    }

    /// Flushes buffered rows to QuestDB.
    ///
    /// # Resilience contract (DB-1/DB-6/DB-7)
    /// - **Ring rescue (DB-1)**: every append also lands in the rescue
    ///   ring. On successful flush, the corresponding prefix is popped
    ///   (durable now). On failed flush, the ring is UNTOUCHED — rows
    ///   survive the flush failure and will be replayed on reconnect.
    /// - **Reconnect throttle (DB-6)**: one attempt per
    ///   `INDICATOR_RECONNECT_THROTTLE_SECS` (30s). After a successful
    ///   reconnect, the rescue ring is drained before the current batch
    ///   is flushed, so oldest rows reach QuestDB first.
    /// - **Drop observability (DB-7)**: every data-loss path increments
    ///   `tv_indicator_snapshot_dropped_total` + logs at ERROR. In
    ///   healthy operation this counter is zero — any non-zero value
    ///   fires a Telegram alert.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending_count == 0 && self.rescue_ring.is_empty() {
            return Ok(());
        }

        // Reconnect path — throttled.
        if self.sender.is_none() {
            if !self.reconnect_allowed_now() {
                // Throttle window is still closed. Clear the in-flight
                // ILP buffer and let the NEXT flush attempt deliver both
                // the in-flight rows (which are still in the rescue ring)
                // and any new appends. We do NOT record a drop here,
                // because the rows are still durable in the rescue ring.
                self.buffer.clear();
                self.pending_count = 0;
                debug!(
                    ring_depth = self.rescue_ring.len(),
                    "indicator snapshot flush deferred — reconnect throttled, \
                     rows retained in rescue ring"
                );
                return Ok(());
            }
            // Attempt reconnect. Bump throttle BEFORE the attempt so a
            // failure blocks the next try by the full window.
            self.bump_reconnect_throttle();
            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(s) => {
                    info!("indicator snapshot writer reconnected to QuestDB");
                    self.sender = Some(s);
                }
                Err(err) => {
                    let wrapped = anyhow::Error::from(err);
                    // Reconnect failed — rows remain in the rescue ring.
                    // We do NOT record_drop here (no data is lost yet;
                    // overflow is the only drop path in the ring-enabled
                    // design). We just log and return.
                    warn!(
                        ?wrapped,
                        ring_depth = self.rescue_ring.len(),
                        "indicator snapshot reconnect failed — rows retained \
                         in rescue ring; will retry after throttle window"
                    );
                    self.buffer.clear();
                    self.pending_count = 0;
                    return Ok(());
                }
            }
        }

        // Connected. FIRST drain the rescue ring (oldest rows first), so
        // on a bursty restart the oldest missed snapshots reach QuestDB
        // before the in-flight batch.
        if !self.rescue_ring.is_empty()
            && let Err(err) = self.drain_rescue_ring_to_sender()
        {
            // Ring drain failed (flush error during drain). Keep the
            // sender set to None so the next flush retries reconnect
            // + drain. Rows are still in the ring.
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            warn!(
                ?err,
                ring_depth = self.rescue_ring.len(),
                "indicator snapshot rescue ring drain failed — will retry \
                 on next flush attempt"
            );
            return Ok(());
        }

        // Now flush the in-flight ILP batch.
        if self.pending_count == 0 {
            return Ok(());
        }
        let sender = self
            .sender
            .as_mut()
            .context("sender present after reconnect branch")?;
        let count = self.pending_count;
        if let Err(err) = sender.flush(&mut self.buffer) {
            let wrapped = anyhow::Error::from(err);
            // Flush failed. The rows are STILL in the rescue ring (we
            // don't pop until success), so they'll be replayed on the
            // next reconnect. We just need to clear the in-flight ILP
            // buffer and drop the sender so the reconnect path fires.
            // Do NOT record_drop — no data is lost yet.
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            warn!(
                ?wrapped,
                count,
                ring_depth = self.rescue_ring.len(),
                "indicator snapshot flush failed — rows retained in rescue ring"
            );
            return Ok(());
        }

        // Successful flush. The rows that were in the ILP buffer are now
        // durable, so we pop the first `count` entries from the rescue
        // ring. If drain_rescue_ring_to_sender was called above, the
        // ring was already cleared — in which case only the in-flight
        // rows need to be popped, but they were appended AFTER the drain
        // completed, so pop_front up to `count` is still correct.
        for _ in 0..count {
            self.rescue_ring.pop_front();
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
                    drop(
                        client
                            .get(&base_url)
                            .query(&[("query", &drop_sql)])
                            .send()
                            .await,
                    );
                    drop(
                        client
                            .get(&base_url)
                            .query(&[("query", INDICATOR_SNAPSHOTS_DDL)])
                            .send()
                            .await,
                    );
                    drop(
                        client
                            .get(&base_url)
                            .query(&[("query", &dedup_sql)])
                            .send()
                            .await,
                    );
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
                    rescue_ring: VecDeque::new(),
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
                rescue_ring: VecDeque::new(),
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
                rescue_ring: VecDeque::new(),
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
                rescue_ring: VecDeque::new(),
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

    // -----------------------------------------------------------------------
    // DB-1: rescue ring (indicator_snapshot)
    // -----------------------------------------------------------------------

    /// Helper: synthesize an `IndicatorSnapshotWriter` with a pre-allocated
    /// rescue ring and no sender. Used by the DB-1 tests below.
    fn synth_writer_for_ring_tests() -> IndicatorSnapshotWriter {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        IndicatorSnapshotWriter {
            sender: None,
            buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
            pending_count: 0,
            ilp_conf_string: config.build_ilp_conf_string(),
            next_reconnect_allowed: Instant::now(),
            rows_dropped_total: 0,
            rescue_ring: VecDeque::with_capacity(INDICATOR_RESCUE_RING_CAPACITY),
        }
    }

    /// Pin the ring capacity constant. The audit gap explicitly bounds
    /// in-memory rescue to prevent unbounded growth under a sustained
    /// QuestDB outage.
    #[test]
    fn test_db1_rescue_ring_capacity_is_bounded() {
        assert!(
            INDICATOR_RESCUE_RING_CAPACITY >= 1_000,
            "rescue ring must hold at least 1,000 snapshots — covers a \
             short QuestDB restart"
        );
        assert!(
            INDICATOR_RESCUE_RING_CAPACITY <= 100_000,
            "rescue ring must be bounded at most at 100,000 — at ~160 \
             bytes per entry that's ~16 MB, the hard ceiling for an \
             in-memory rescue structure before we need disk spill (DB-1 \
             follow-up item)"
        );
    }

    /// `rescue_ring_len` must start at zero and increment on each push.
    /// Covers the pub-fn-test-guard contract for `rescue_ring_len`.
    #[test]
    fn test_db1_rescue_ring_len_starts_zero_and_increments() {
        let mut writer = synth_writer_for_ring_tests();
        assert_eq!(writer.rescue_ring_len(), 0);

        let snap = BufferedSnapshot {
            ts_nanos: 1,
            security_id: 42,
            segment_code: 1,
            ema_fast: 0.0,
            ema_slow: 0.0,
            sma: 0.0,
            rsi: 0.0,
            macd_line: 0.0,
            macd_signal: 0.0,
            macd_histogram: 0.0,
            bollinger_upper: 0.0,
            bollinger_middle: 0.0,
            bollinger_lower: 0.0,
            atr: 0.0,
            supertrend: 0.0,
            supertrend_bullish: false,
            adx: 0.0,
            obv: 0.0,
            vwap: 0.0,
            ltp: 0.0,
            is_warm: false,
        };
        writer.push_to_rescue_ring(snap);
        assert_eq!(writer.rescue_ring_len(), 1);
        writer.push_to_rescue_ring(snap);
        assert_eq!(writer.rescue_ring_len(), 2);
    }

    /// FIFO ordering: the first snapshot pushed must be the first
    /// snapshot popped on drain. This is the core invariant for
    /// time-ordered replay to QuestDB on reconnect.
    #[test]
    fn test_db1_rescue_ring_is_fifo() {
        let mut writer = synth_writer_for_ring_tests();
        let mk = |ts: i64| BufferedSnapshot {
            ts_nanos: ts,
            security_id: 1,
            segment_code: 1,
            ema_fast: 0.0,
            ema_slow: 0.0,
            sma: 0.0,
            rsi: 0.0,
            macd_line: 0.0,
            macd_signal: 0.0,
            macd_histogram: 0.0,
            bollinger_upper: 0.0,
            bollinger_middle: 0.0,
            bollinger_lower: 0.0,
            atr: 0.0,
            supertrend: 0.0,
            supertrend_bullish: false,
            adx: 0.0,
            obv: 0.0,
            vwap: 0.0,
            ltp: 0.0,
            is_warm: false,
        };
        writer.push_to_rescue_ring(mk(100));
        writer.push_to_rescue_ring(mk(200));
        writer.push_to_rescue_ring(mk(300));
        assert_eq!(writer.rescue_ring_len(), 3);

        // Pop front must see 100, 200, 300 in order.
        let first = writer.rescue_ring.pop_front().unwrap();
        let second = writer.rescue_ring.pop_front().unwrap();
        let third = writer.rescue_ring.pop_front().unwrap();
        assert_eq!(first.ts_nanos, 100);
        assert_eq!(second.ts_nanos, 200);
        assert_eq!(third.ts_nanos, 300);
    }

    /// Overflow eviction: pushing past the capacity must evict the OLDEST
    /// entry (FIFO) and increment `rows_dropped_total`. This is the only
    /// data-loss path in the ring-enabled design; it must be loud.
    #[test]
    fn test_db1_rescue_ring_overflow_evicts_oldest_and_counts_drop() {
        let mut writer = synth_writer_for_ring_tests();
        // Shrink the ring for the test by filling to capacity — but
        // INDICATOR_RESCUE_RING_CAPACITY is too large for a fast unit
        // test, so we use a smaller synthetic ring. We cannot change the
        // constant, so instead we manually simulate the overflow by
        // pre-filling the ring with dummy entries up to capacity-1,
        // pushing one more to hit capacity, then pushing a final entry
        // to trigger eviction.
        let dummy = BufferedSnapshot {
            ts_nanos: 0,
            security_id: 0,
            segment_code: 0,
            ema_fast: 0.0,
            ema_slow: 0.0,
            sma: 0.0,
            rsi: 0.0,
            macd_line: 0.0,
            macd_signal: 0.0,
            macd_histogram: 0.0,
            bollinger_upper: 0.0,
            bollinger_middle: 0.0,
            bollinger_lower: 0.0,
            atr: 0.0,
            supertrend: 0.0,
            supertrend_bullish: false,
            adx: 0.0,
            obv: 0.0,
            vwap: 0.0,
            ltp: 0.0,
            is_warm: false,
        };
        // Fast-path: bypass push_to_rescue_ring and fill directly so we
        // don't run the overflow check N=20,000 times during setup.
        for i in 0..INDICATOR_RESCUE_RING_CAPACITY {
            let mut entry = dummy;
            entry.ts_nanos = i64::try_from(i).unwrap_or(i64::MAX);
            writer.rescue_ring.push_back(entry);
        }
        assert_eq!(writer.rescue_ring_len(), INDICATOR_RESCUE_RING_CAPACITY);
        assert_eq!(writer.rows_dropped_total(), 0);

        // Push one more — must evict the oldest (ts=0) and increment drop.
        let mut new_entry = dummy;
        new_entry.ts_nanos = i64::MAX; // Distinguishable sentinel.
        writer.push_to_rescue_ring(new_entry);

        assert_eq!(
            writer.rescue_ring_len(),
            INDICATOR_RESCUE_RING_CAPACITY,
            "ring must stay at capacity after eviction"
        );
        assert_eq!(
            writer.rows_dropped_total(),
            1,
            "overflow must increment rows_dropped_total by exactly 1"
        );
        // The newest entry must be at the back.
        let back = writer.rescue_ring.back().unwrap();
        assert_eq!(back.ts_nanos, i64::MAX);
        // The oldest (ts=0) must be gone; new front is ts=1.
        let front = writer.rescue_ring.front().unwrap();
        assert_eq!(
            front.ts_nanos, 1,
            "oldest entry (ts=0) must have been evicted, new front is ts=1"
        );
    }

    /// BufferedSnapshot must be Copy so ring pushes are zero-alloc and
    /// iteration for drain doesn't require references (&) across an
    /// async boundary.
    #[test]
    fn test_db1_buffered_snapshot_is_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<BufferedSnapshot>();
    }

    /// BufferedSnapshot size sanity check. If this ever balloons, the
    /// ring's memory footprint grows linearly — fail loudly so the
    /// capacity constant can be re-tuned.
    #[test]
    fn test_db1_buffered_snapshot_size_is_bounded() {
        let size = std::mem::size_of::<BufferedSnapshot>();
        // Expected: 8 (ts_nanos) + 4 (security_id) + 1 (segment_code) +
        // 17*8 (f64s) + 2*1 (bools) + padding = ~160 bytes.
        assert!(
            size <= 200,
            "BufferedSnapshot size is {size} bytes — if this grows, \
             re-tune INDICATOR_RESCUE_RING_CAPACITY to stay under 16 MB"
        );
    }
}
