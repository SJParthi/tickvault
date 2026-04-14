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
//! DEDUP UPSERT KEYS on `(ts, security_id)` prevent duplicates on restart.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

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
pub struct IndicatorSnapshotWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
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
        })
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

        if self.pending_count >= INDICATOR_FLUSH_BATCH_SIZE
            && let Err(err) = self.flush()
        {
            warn!(?err, "indicator snapshot auto-flush failed");
        }

        Ok(())
    }

    /// Flushes buffered rows to QuestDB.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        let sender = match self.sender.as_mut() {
            Some(s) => s,
            None => match Sender::from_conf(&self.ilp_conf_string) {
                Ok(s) => {
                    self.sender = Some(s);
                    self.sender.as_mut().context("sender after reconnect")?
                }
                Err(err) => {
                    warn!(?err, "indicator snapshot reconnect failed — dropping batch");
                    self.buffer.clear();
                    self.pending_count = 0;
                    return Ok(());
                }
            },
        };

        let count = self.pending_count;
        if let Err(err) = sender.flush(&mut self.buffer) {
            warn!(
                ?err,
                count, "indicator snapshot flush failed — dropping batch"
            );
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
}
