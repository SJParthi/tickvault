//! QuestDB persistence for OHLCV candles from Dhan historical API.
//!
//! Stores candles in `historical_candles` table with DEDUP UPSERT KEYS
//! on `(ts, security_id, timeframe, segment)` to ensure idempotent re-ingestion.
//! Supports multiple timeframes: 1m, 5m, 15m, 60m, and daily.
//!
//! # Table Schema
//! - `ts` TIMESTAMP (designated) — candle open time (IST epoch for live, UTC+19800s for historical)
//! - `security_id` LONG — Dhan security identifier
//! - `segment` SYMBOL — exchange segment (NSE_FNO, NSE_EQ, etc.)
//! - `timeframe` SYMBOL — candle interval: 1m, 5m, 15m, 60m, 1d
//! - OHLCV + OI as DOUBLE/LONG columns
//!
//! # Deduplication
//! Server-side: QuestDB DEDUP UPSERT KEYS(ts, security_id, timeframe) prevents
//! duplicate candles from re-fetches.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    CANDLE_FLUSH_BATCH_SIZE, IST_UTC_OFFSET_SECONDS_I64, QUESTDB_TABLE_HISTORICAL_CANDLES,
};
use tickvault_common::segment::segment_code_to_str;
use tickvault_common::tick_types::HistoricalCandle;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY columns for the historical candles table.
/// Compound key: (ts, security_id, timeframe, segment) ensures uniqueness per candle.
/// `segment` is required because security IDs 13 (NIFTY) and 25 (BANKNIFTY) exist
/// in both `IDX_I` and `NSE_EQ` segments with different data.
const DEDUP_KEY_CANDLES: &str = "security_id, timeframe, segment";

// ---------------------------------------------------------------------------
// Pure helper functions (testable without DB)
// ---------------------------------------------------------------------------

/// Converts a UTC epoch-seconds timestamp to IST-as-UTC nanoseconds.
///
/// Adds `IST_UTC_OFFSET_SECONDS_I64` (19800s) so QuestDB displays IST wall-clock
/// time, then multiplies by 1_000_000_000 for nanosecond precision.
///
/// Uses saturating arithmetic to prevent overflow.
fn compute_ist_nanos_from_utc_secs(utc_secs: i64) -> i64 {
    utc_secs
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64)
        .saturating_mul(1_000_000_000)
}

/// Returns `true` if `pending_count` has reached or exceeded the given batch size threshold.
fn should_flush(pending_count: usize, batch_size: usize) -> bool {
    pending_count >= batch_size
}

/// Builds the QuestDB HTTP exec URL from host and port.
fn build_questdb_exec_url(host: &str, http_port: u16) -> String {
    format!("http://{}:{}/exec", host, http_port)
}

/// Builds the ALTER TABLE DEDUP ENABLE UPSERT KEYS SQL statement.
fn build_dedup_sql(table_name: &str, dedup_key: &str) -> String {
    format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        table_name, dedup_key
    )
}

/// Builds the ILP TCP connection string from host and port.
///
/// **Used for the hot path only** (live tick/candle streaming). The TCP transport
/// holds a persistent socket — efficient for continuous writes but vulnerable to
/// broken-pipe between batches when QuestDB rotates/releases idle writers.
/// The hot path compensates with a ring buffer + disk spill.
fn build_ilp_conf_string(host: &str, ilp_port: u16) -> String {
    format!("tcp::addr={}:{};", host, ilp_port)
}

/// Builds the ILP **HTTP** connection string from host and HTTP port.
///
/// **Used for the cold/historical path** ([`CandlePersistenceWriter`]).
///
/// HTTP ILP is stateless: every `flush()` is a single transactional POST to
/// QuestDB's `/write` endpoint. There is no persistent TCP socket between
/// batches, which architecturally eliminates the "broken pipe after every
/// successful flush" failure mode observed with TCP ILP on bursty cold-path
/// workloads (historical candle backfill has natural pauses between REST calls
/// to Dhan that cause QuestDB's line.tcp writer to release idle sockets).
///
/// Parameters:
/// - `auto_flush=off` — we control batching explicitly via `CANDLE_FLUSH_BATCH_SIZE`.
/// - `retry_timeout=30000` — HTTP sender retries transient 5xx / network errors
///   for up to 30s internally, so callers rarely see transient failures.
///
/// Reference: QuestDB ILP transport comparison
/// <https://questdb.io/docs/clients/ingest-rust/#transport-selection>.
fn build_historical_ilp_conf_string(host: &str, http_port: u16) -> String {
    format!(
        "http::addr={}:{};auto_flush=off;retry_timeout=30000;",
        host, http_port
    )
}

// ---------------------------------------------------------------------------
// Candle Persistence Writer
// ---------------------------------------------------------------------------

/// Max reconnection attempts for historical candle writer.
const HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS: u32 = 3;
/// Initial backoff delay (ms) for historical candle reconnection.
const HISTORICAL_CANDLE_RECONNECT_INITIAL_DELAY_MS: u64 = 1000;

/// Batched candle writer for QuestDB via ILP.
///
/// Used for historical candle ingestion (cold path). On disconnect,
/// propagates errors to the caller (which decides whether to retry or skip).
pub struct CandlePersistenceWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    /// ILP connection config string, retained for reconnection.
    ilp_conf_string: String,
}

impl CandlePersistenceWriter {
    /// Creates a new candle writer connected to QuestDB via **TCP ILP**.
    ///
    /// # Deprecated for production
    /// Production code MUST use [`CandlePersistenceWriter::new_http`] instead.
    /// TCP ILP is only retained here for the extensive unit-test suite, which
    /// uses an in-process `TcpListener` drain server as a cheap fake. On the
    /// real historical backfill workload (bursty writes with natural pauses
    /// between Dhan REST calls), QuestDB `line.tcp` rotates idle sockets,
    /// causing every subsequent batch to fail with `Broken pipe (os error 32)`.
    ///
    /// # Errors
    /// Returns error if the ILP connection cannot be established.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = build_ilp_conf_string(&config.host, config.ilp_port);
        let sender =
            Sender::from_conf(&conf_string).context("failed to connect to QuestDB via ILP")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
        })
    }

    /// Creates a new historical candle writer connected to QuestDB via **HTTP ILP**.
    ///
    /// **This is the production constructor for the historical cold-path writer.**
    ///
    /// HTTP ILP is the correct transport for this workload: historical backfill
    /// has natural pauses between Dhan REST calls, during which QuestDB's
    /// `line.tcp` writer releases idle sockets. With TCP ILP, every subsequent
    /// batch then fails with `Broken pipe (os error 32)` — the failure mode
    /// observed in prod logs at 2026-04-15T19:42+ (every 500-row batch failed
    /// immediately after a successful flush, forcing a reconnect storm). HTTP
    /// ILP is stateless: each `flush()` is a single transactional POST to
    /// QuestDB's `/write` endpoint, so there is no persistent socket that can
    /// break between batches.
    ///
    /// Parameters baked into the conf string:
    /// - `auto_flush=off` — we control batching via `CANDLE_FLUSH_BATCH_SIZE`.
    /// - `retry_timeout=30000` — internal retries for transient 5xx / network
    ///   errors, so callers rarely see transient failures surface through the API.
    ///
    /// # Errors
    /// Returns error if the HTTP ILP connection cannot be established (invalid
    /// host, DNS failure, TLS handshake failure, auth failure, etc.).
    ///
    /// # Testing
    /// Thin wrapper over the pure helper `build_historical_ilp_conf_string`,
    /// which is covered by 7 unit tests (http-not-tcp, docker defaults, custom
    /// port, auto_flush=off, retry_timeout=30000, terminator, distinct-from-tcp).
    /// The only non-helper line is `Sender::from_conf(conf)` which belongs to
    /// questdb-rs. A live-server smoke test is not feasible from a unit test
    /// because questdb-rs HTTP `from_conf` validates the endpoint against the
    /// real QuestDB `/ping` handshake, which an in-process mock cannot satisfy
    /// (verified empirically: the naive mock test panicked on `from_conf`).
    /// Integration coverage lives in `spawn_historical_candle_fetch` in
    /// `crates/app/src/main.rs`, exercised by the real backfill workload.
    // TEST-EXEMPT: thin wrapper — see `# Testing` doc above for full rationale.
    pub fn new_http(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = build_historical_ilp_conf_string(&config.host, config.http_port);
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB via HTTP ILP (historical writer)")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
        })
    }

    /// Appends a historical candle to the ILP buffer.
    ///
    /// Converts `candle.timestamp_utc_secs` (UTC epoch from Dhan V2 API) to
    /// IST-as-UTC by adding `IST_UTC_OFFSET_SECONDS_I64`, so QuestDB displays
    /// IST wall-clock time directly (e.g., 09:00 instead of 03:30).
    ///
    /// Writes to the unified `historical_candles` table with a `timeframe` column.
    ///
    /// Auto-flushes if the buffer reaches `CANDLE_FLUSH_BATCH_SIZE`.
    ///
    /// # Performance
    /// O(1) — single ILP row append + conditional flush.
    pub fn append_candle(&mut self, candle: &HistoricalCandle) -> Result<()> {
        // Try reconnect if disconnected (cold path — ok to propagate error).
        self.try_reconnect_on_error()?;

        // UTC epoch → IST-as-UTC: add 19800s so QuestDB shows IST wall-clock time.
        let ts_nanos =
            TimestampNanos::new(compute_ist_nanos_from_utc_secs(candle.timestamp_utc_secs));

        self.buffer
            .table(QUESTDB_TABLE_HISTORICAL_CANDLES)
            .context("table name")?
            .symbol("segment", segment_code_to_str(candle.exchange_segment_code))
            .context("segment")?
            .symbol("timeframe", candle.timeframe)
            .context("timeframe")?
            .column_i64("security_id", i64::from(candle.security_id))
            .context("security_id")?
            .column_f64("open", candle.open)
            .context("open")?
            .column_f64("high", candle.high)
            .context("high")?
            .column_f64("low", candle.low)
            .context("low")?
            .column_f64("close", candle.close)
            .context("close")?
            .column_i64("volume", candle.volume)
            .context("volume")?
            .column_i64("oi", candle.open_interest)
            .context("oi")?
            .at(ts_nanos)
            .context("designated timestamp")?;

        self.pending_count = self.pending_count.saturating_add(1);

        if should_flush(self.pending_count, CANDLE_FLUSH_BATCH_SIZE) {
            self.force_flush()?;
        }

        Ok(())
    }

    /// Forces an immediate flush of all buffered candles to QuestDB.
    pub fn force_flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        let count = self.pending_count;
        let sender = self
            .sender
            .as_mut()
            .context("QuestDB sender disconnected for historical candle writer")?;
        if let Err(err) = sender.flush(&mut self.buffer) {
            self.sender = None;
            self.pending_count = 0;
            // Create fresh buffer instead of clear() to avoid questdb-rs state corruption.
            // sender is None here (set above), so always use standalone buffer.
            self.buffer = Buffer::new(ProtocolVersion::V1);
            return Err(err).context("flush candles to QuestDB");
        }
        self.pending_count = 0;

        debug!(flushed_rows = count, "candle batch flushed to QuestDB");
        Ok(())
    }

    /// Returns the number of candles currently buffered (not yet flushed).
    pub fn pending_count(&self) -> usize {
        self.pending_count
    }

    /// Attempts to reconnect to QuestDB with exponential backoff.
    fn reconnect(&mut self) -> Result<()> {
        let mut delay_ms = HISTORICAL_CANDLE_RECONNECT_INITIAL_DELAY_MS;

        for attempt in 1..=HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS {
            warn!(
                attempt,
                max_attempts = HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS,
                delay_ms,
                "attempting QuestDB ILP reconnection for historical candle writer"
            );

            std::thread::sleep(Duration::from_millis(delay_ms));

            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(new_sender) => {
                    self.buffer = new_sender.new_buffer();
                    self.sender = Some(new_sender);
                    self.pending_count = 0;
                    info!(
                        attempt,
                        "QuestDB ILP reconnection succeeded for historical candle writer"
                    );
                    return Ok(());
                }
                Err(err) => {
                    error!(
                        attempt,
                        max_attempts = HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS,
                        ?err,
                        "QuestDB ILP reconnection failed for historical candle writer"
                    );
                    delay_ms = delay_ms.saturating_mul(2);
                }
            }
        }

        anyhow::bail!(
            "QuestDB ILP reconnection failed after {} attempts for historical candle writer",
            HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS,
        )
    }

    /// Attempts reconnection only when the sender is `None`.
    fn try_reconnect_on_error(&mut self) -> Result<()> {
        if self.sender.is_some() {
            return Ok(());
        }
        self.reconnect()
    }
}

// ---------------------------------------------------------------------------
// Table DDL + DEDUP Setup
// ---------------------------------------------------------------------------

/// SQL to create the `historical_candles` table with explicit schema.
/// Includes `timeframe` SYMBOL column for multi-timeframe candle storage.
///
/// Idempotent — safe to call every startup.
const HISTORICAL_CANDLES_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS historical_candles (\
        segment SYMBOL,\
        timeframe SYMBOL,\
        security_id LONG,\
        open DOUBLE,\
        high DOUBLE,\
        low DOUBLE,\
        close DOUBLE,\
        volume LONG,\
        oi LONG,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

/// Creates the `historical_candles` table (if not exists) and enables DEDUP UPSERT KEYS.
///
/// Best-effort: if QuestDB is unreachable, logs a warning and continues.
pub async fn ensure_candle_table_dedup_keys(questdb_config: &QuestDbConfig) {
    let base_url = build_questdb_exec_url(&questdb_config.host, questdb_config.http_port);

    // Client::builder().timeout().build() is infallible (no custom TLS).
    // Coverage: unwrap_or_else avoids uncoverable else-return on dead path.
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    // Step 1: Create the new multi-timeframe table.
    match client
        .get(&base_url)
        .query(&[("query", HISTORICAL_CANDLES_CREATE_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!(
                    table = QUESTDB_TABLE_HISTORICAL_CANDLES,
                    "historical_candles table ensured (CREATE TABLE IF NOT EXISTS)"
                );
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(%status, body = body.chars().take(200).collect::<String>(), "historical_candles table CREATE DDL returned non-success");
            }
        }
        Err(err) => {
            warn!(
                ?err,
                "historical_candles table CREATE DDL request failed — table not pre-created"
            );
            return;
        }
    }

    // Step 2: Enable DEDUP UPSERT KEYS on new table (ts, security_id, timeframe).
    let dedup_sql = build_dedup_sql(QUESTDB_TABLE_HISTORICAL_CANDLES, DEDUP_KEY_CANDLES);
    match client
        .get(&base_url)
        .query(&[("query", &dedup_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                debug!(
                    table = QUESTDB_TABLE_HISTORICAL_CANDLES,
                    "DEDUP UPSERT KEYS enabled"
                );
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(%status, body = body.chars().take(200).collect::<String>(), "historical_candles table DEDUP DDL returned non-success");
            }
        }
        Err(err) => {
            warn!(?err, "historical_candles table DEDUP DDL request failed");
        }
    }

    // Phase 0 Item 29 — `candles_source` SYMBOL column ALTER on
    // `historical_candles` via `ALTER ADD COLUMN IF NOT EXISTS` per the
    // PR #690 schema self-heal pattern. Values: `dhan_rest` (default for
    // legacy + gap-fill writes per `live-feed-purity.md` rule 4),
    // `cross_check_correction` (Item 28 bar-correction mirror writes).
    //
    // Column is OUTSIDE the DEDUP key (Items 15+28+29 hot-path-reviewer
    // C1) so corrections REPLACE the wrong row instead of APPENDING a
    // sibling row — DEDUP tuple unchanged.
    let alter_historical = format!(
        "ALTER TABLE {QUESTDB_TABLE_HISTORICAL_CANDLES} \
         ADD COLUMN IF NOT EXISTS candles_source SYMBOL"
    );
    match client
        .get(&base_url)
        .query(&[("query", alter_historical.as_str())])
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_HISTORICAL_CANDLES,
                "candles_source SYMBOL column ensured"
            );
        }
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!(
                %status,
                body = body.chars().take(200).collect::<String>(),
                "historical_candles candles_source ALTER non-success"
            );
        }
        Err(err) => {
            warn!(
                ?err,
                "historical_candles candles_source ALTER request failed"
            );
        }
    }

    info!(
        "historical candle tables setup complete (DDL + DEDUP UPSERT KEYS + candles_source ALTER)"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::{
        EXCHANGE_SEGMENT_BSE_CURRENCY, EXCHANGE_SEGMENT_BSE_EQ, EXCHANGE_SEGMENT_BSE_FNO,
        EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_MCX_COMM, EXCHANGE_SEGMENT_NSE_CURRENCY,
        EXCHANGE_SEGMENT_NSE_EQ, EXCHANGE_SEGMENT_NSE_FNO,
    };

    #[test]
    fn test_segment_code_to_str_all_valid_codes() {
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_IDX_I), "IDX_I");
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_NSE_EQ), "NSE_EQ");
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_NSE_FNO), "NSE_FNO");
        assert_eq!(
            segment_code_to_str(EXCHANGE_SEGMENT_NSE_CURRENCY),
            "NSE_CURRENCY"
        );
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_BSE_EQ), "BSE_EQ");
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_MCX_COMM), "MCX_COMM");
        assert_eq!(
            segment_code_to_str(EXCHANGE_SEGMENT_BSE_CURRENCY),
            "BSE_CURRENCY"
        );
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_BSE_FNO), "BSE_FNO");
    }

    #[test]
    fn test_segment_code_to_str_unknown() {
        assert_eq!(segment_code_to_str(255), "UNKNOWN");
        // Code 6 is unused in Dhan protocol — must map to UNKNOWN
        assert_eq!(segment_code_to_str(6), "UNKNOWN");
    }

    #[test]
    fn test_segment_code_bse_fno_is_8_not_7() {
        // Regression: BSE_FNO is code 8, not 7. Code 6 is skipped in Dhan protocol.
        assert_eq!(EXCHANGE_SEGMENT_BSE_FNO, 8);
        assert_eq!(EXCHANGE_SEGMENT_BSE_CURRENCY, 7);
        assert_eq!(segment_code_to_str(8), "BSE_FNO");
        assert_eq!(segment_code_to_str(7), "BSE_CURRENCY");
    }

    #[test]
    fn test_historical_candles_ddl_contains_table_and_timeframe() {
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("historical_candles"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("timeframe SYMBOL"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("TIMESTAMP(ts)"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("PARTITION BY DAY"));
    }

    #[test]
    fn test_dedup_key_includes_timeframe_and_segment() {
        assert!(DEDUP_KEY_CANDLES.contains("security_id"));
        assert!(DEDUP_KEY_CANDLES.contains("timeframe"));
        assert!(
            DEDUP_KEY_CANDLES.contains("segment"),
            "DEDUP key must include segment — security IDs 13/25 exist in both IDX_I and NSE_EQ"
        );
    }

    #[tokio::test]
    async fn test_ensure_candle_table_does_not_panic_unreachable() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Should not panic — just logs warnings and returns.
        ensure_candle_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // DEDUP key and DDL constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_dedup_key_candles_includes_all_components() {
        assert_eq!(DEDUP_KEY_CANDLES, "security_id, timeframe, segment");
    }

    #[test]
    fn test_questdb_ddl_timeout_is_reasonable_candle() {
        assert!((5..=60).contains(&QUESTDB_DDL_TIMEOUT_SECS));
    }

    // -----------------------------------------------------------------------
    // compute_ist_nanos_from_utc_secs
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_ist_nanos_basic() {
        // UTC 0 → IST is +19800s → 19800 * 1_000_000_000 nanos
        let nanos = compute_ist_nanos_from_utc_secs(0);
        assert_eq!(nanos, 19_800 * 1_000_000_000);
    }

    #[test]
    fn test_compute_ist_nanos_known_timestamp() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        // IST = 1704067200 + 19800 = 1704087000
        // nanos = 1704087000 * 1_000_000_000
        let utc_secs = 1_704_067_200_i64;
        let nanos = compute_ist_nanos_from_utc_secs(utc_secs);
        let expected = (utc_secs + IST_UTC_OFFSET_SECONDS_I64) * 1_000_000_000;
        assert_eq!(nanos, expected);
    }

    #[test]
    fn test_compute_ist_nanos_negative_utc_secs() {
        // Negative UTC secs (before epoch) — should still work with saturating
        let nanos = compute_ist_nanos_from_utc_secs(-100_000);
        let expected = (-100_000_i64 + IST_UTC_OFFSET_SECONDS_I64) * 1_000_000_000;
        assert_eq!(nanos, expected);
    }

    #[test]
    fn test_compute_ist_nanos_saturates_on_overflow() {
        // i64::MAX should saturate, not overflow
        let nanos = compute_ist_nanos_from_utc_secs(i64::MAX);
        assert_eq!(nanos, i64::MAX);
    }

    #[test]
    fn test_compute_ist_nanos_offset_is_19800() {
        // Verify the IST offset constant is correct (5h30m = 19800s)
        assert_eq!(IST_UTC_OFFSET_SECONDS_I64, 5 * 3600 + 30 * 60);
    }

    // -----------------------------------------------------------------------
    // should_flush
    // -----------------------------------------------------------------------

    #[test]
    fn test_should_flush_below_threshold() {
        assert!(!should_flush(0, 500));
        assert!(!should_flush(499, 500));
    }

    #[test]
    fn test_should_flush_at_threshold() {
        assert!(should_flush(500, 500));
    }

    #[test]
    fn test_should_flush_above_threshold() {
        assert!(should_flush(501, 500));
        assert!(should_flush(1000, 500));
    }

    #[test]
    fn test_should_flush_zero_threshold() {
        // Edge case: zero threshold always triggers flush (except empty)
        assert!(should_flush(0, 0));
        assert!(should_flush(1, 0));
    }

    // -----------------------------------------------------------------------
    // build_questdb_exec_url
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_questdb_exec_url_docker() {
        let url = build_questdb_exec_url("tv-questdb", 9000);
        assert_eq!(url, "http://tv-questdb:9000/exec");
    }

    #[test]
    fn test_build_questdb_exec_url_custom_port() {
        let url = build_questdb_exec_url("192.168.1.100", 19000);
        assert_eq!(url, "http://192.168.1.100:19000/exec");
    }

    // -----------------------------------------------------------------------
    // build_dedup_sql
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_dedup_sql_historical_candles() {
        let sql = build_dedup_sql(QUESTDB_TABLE_HISTORICAL_CANDLES, DEDUP_KEY_CANDLES);
        assert_eq!(
            sql,
            "ALTER TABLE historical_candles DEDUP ENABLE UPSERT KEYS(ts, security_id, timeframe, segment)"
        );
    }

    #[test]
    fn test_build_dedup_sql_starts_with_alter_table() {
        let sql = build_dedup_sql("any_table", "col_a, col_b");
        assert!(sql.starts_with("ALTER TABLE any_table"));
        assert!(sql.contains("DEDUP ENABLE UPSERT KEYS(ts, col_a, col_b)"));
    }

    // -----------------------------------------------------------------------
    // build_ilp_conf_string
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ilp_conf_string_docker() {
        let conf = build_ilp_conf_string("tv-questdb", 9009);
        assert_eq!(conf, "tcp::addr=tv-questdb:9009;");
    }

    #[test]
    fn test_build_ilp_conf_string_custom() {
        let conf = build_ilp_conf_string("10.0.0.5", 19009);
        assert_eq!(conf, "tcp::addr=10.0.0.5:19009;");
    }

    #[test]
    fn test_build_ilp_conf_string_ends_with_semicolon() {
        let conf = build_ilp_conf_string("host", 1234);
        assert!(
            conf.ends_with(';'),
            "ILP conf string must end with semicolon"
        );
    }

    #[test]
    fn test_build_ilp_conf_string_starts_with_tcp() {
        let conf = build_ilp_conf_string("host", 1234);
        assert!(
            conf.starts_with("tcp::addr="),
            "ILP conf string must start with tcp::addr="
        );
    }

    // -----------------------------------------------------------------------
    // build_historical_ilp_conf_string — cold-path HTTP ILP (broken-pipe fix)
    // -----------------------------------------------------------------------

    /// Historical candle writer MUST use HTTP ILP, not TCP ILP.
    /// TCP ILP causes "broken pipe after every successful flush" on bursty
    /// cold-path workloads because QuestDB rotates idle line.tcp sockets.
    #[test]
    fn test_build_historical_ilp_conf_string_uses_http_not_tcp() {
        let conf = build_historical_ilp_conf_string("tv-questdb", 9000);
        assert!(
            conf.starts_with("http::addr="),
            "historical writer MUST use http:: transport, got: {conf}"
        );
        assert!(
            !conf.contains("tcp::addr="),
            "historical writer MUST NOT use tcp:: transport, got: {conf}"
        );
    }

    #[test]
    fn test_build_historical_ilp_conf_string_docker_defaults() {
        let conf = build_historical_ilp_conf_string("tv-questdb", 9000);
        assert_eq!(
            conf, "http::addr=tv-questdb:9000;auto_flush=off;retry_timeout=30000;",
            "historical ILP conf string format regression"
        );
    }

    /// `auto_flush=off` is critical: we control batching via
    /// `CANDLE_FLUSH_BATCH_SIZE` and do NOT want questdb-rs auto-flushing
    /// half-built batches on its own schedule.
    #[test]
    fn test_build_historical_ilp_conf_string_disables_auto_flush() {
        let conf = build_historical_ilp_conf_string("host", 9000);
        assert!(
            conf.contains("auto_flush=off"),
            "historical writer must disable auto_flush, got: {conf}"
        );
    }

    /// Retry timeout is critical: HTTP ILP retries transient 5xx / network
    /// errors internally before surfacing a failure to the caller, so the
    /// reconnect path stays a rare exception rather than every-batch.
    #[test]
    fn test_build_historical_ilp_conf_string_sets_retry_timeout() {
        let conf = build_historical_ilp_conf_string("host", 9000);
        assert!(
            conf.contains("retry_timeout=30000"),
            "historical writer must set retry_timeout=30000ms, got: {conf}"
        );
    }

    #[test]
    fn test_build_historical_ilp_conf_string_custom_port() {
        let conf = build_historical_ilp_conf_string("10.0.0.5", 19000);
        assert!(
            conf.starts_with("http::addr=10.0.0.5:19000;"),
            "host:port must reflect config, got: {conf}"
        );
    }

    #[test]
    fn test_build_historical_ilp_conf_string_ends_with_semicolon() {
        let conf = build_historical_ilp_conf_string("host", 1234);
        assert!(
            conf.ends_with(';'),
            "ILP conf string must end with semicolon"
        );
    }

    #[test]
    fn test_tcp_and_http_conf_strings_are_distinct() {
        let tcp = build_ilp_conf_string("tv-questdb", 9009);
        let http = build_historical_ilp_conf_string("tv-questdb", 9000);
        assert_ne!(
            tcp, http,
            "hot path (TCP) and cold path (HTTP) conf strings must not collide"
        );
    }

    // -----------------------------------------------------------------------
    // TCP drain server helper (same pattern as tick_persistence tests)
    // -----------------------------------------------------------------------

    /// Spawn a background TCP server that accepts one connection and drains
    /// all data until EOF. Returns the port.
    fn spawn_tcp_drain_server() -> u16 {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 65536];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
        port
    }

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_400: &str = "HTTP/1.1 400 Bad Request\r\nContent-Length: 31\r\n\r\n{\"error\":\"table does not exist\"}";

    /// Spawn an async HTTP mock server that returns a fixed `response` for
    /// every request. Returns the port.
    async fn spawn_mock_http_server(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    fn make_test_candle(security_id: u32) -> HistoricalCandle {
        HistoricalCandle {
            security_id,
            exchange_segment_code: 2, // NSE_FNO
            timeframe: "1m",
            timestamp_utc_secs: 1_704_067_200, // 2024-01-01 00:00:00 UTC
            open: 21000.0,
            high: 21050.0,
            low: 20950.0,
            close: 21025.0,
            volume: 500_000,
            open_interest: 120_000,
        }
    }

    // -----------------------------------------------------------------------
    // CandlePersistenceWriter — new, append_candle, force_flush, pending_count
    // -----------------------------------------------------------------------

    #[test]
    fn test_candle_writer_new_with_tcp_drain() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let writer = CandlePersistenceWriter::new(&config);
        assert!(writer.is_ok(), "must connect to TCP drain server");
        let writer = writer.unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_candle_writer_append_single_candle() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        let candle = make_test_candle(11536);
        let result = writer.append_candle(&candle);
        assert!(result.is_ok());
        assert_eq!(writer.pending_count(), 1);
    }

    #[test]
    fn test_candle_writer_append_multiple_candles() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        for i in 0..5 {
            let candle = make_test_candle(11536 + i);
            writer.append_candle(&candle).unwrap();
        }
        assert_eq!(writer.pending_count(), 5);
    }

    #[test]
    fn test_candle_writer_force_flush_resets_count() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        let candle = make_test_candle(11536);
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);

        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_candle_writer_force_flush_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        // Flushing empty buffer should succeed and remain at 0
        let result = writer.force_flush();
        assert!(result.is_ok());
        assert_eq!(writer.pending_count(), 0);
    }

    // -----------------------------------------------------------------------
    // ensure_candle_table_dedup_keys — HTTP success, HTTP non-success, error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_candle_table_http_200_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Should complete without panic — exercises success paths (lines 364-365, 398)
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_table_http_400_non_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Should complete without panic — exercises non-success paths (lines 374, 407)
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_table_create_ok_dedup_send_error() {
        // Two-phase: first request (CREATE TABLE) succeeds, second (DEDUP) fails
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // First connection: respond with 200
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(MOCK_HTTP_200.as_bytes()).await;
                drop(stream);
            }
            // Second connection: drop immediately → send error on DEDUP DDL
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the Err(err) branch on DEDUP DDL (lines 412-413)
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_table_create_send_error_returns_early() {
        // First request (CREATE TABLE) fails → should return early (line 384)
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Drop connection immediately → send error on CREATE TABLE
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the Err(err) + return branch on CREATE TABLE (lines 379-380, 384)
        ensure_candle_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // CandlePersistenceWriter resilience tests (Phase D)
    // -----------------------------------------------------------------------

    #[test]
    fn test_historical_candle_writer_starts_connected() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let writer = CandlePersistenceWriter::new(&config).unwrap();
        assert!(
            writer.sender.is_some(),
            "historical writer must start connected"
        );
        assert!(
            !writer.ilp_conf_string.is_empty(),
            "must store conf string for reconnect"
        );
    }

    #[test]
    fn test_historical_candle_writer_reconnect_fails_unreachable() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Simulate disconnect + unreachable host.
        writer.sender = None;
        writer.ilp_conf_string = "tcp::addr=127.0.0.1:1;".to_string();

        let result = writer.try_reconnect_on_error();
        assert!(result.is_err(), "reconnect to unreachable host must fail");
        assert!(writer.sender.is_none());
    }

    #[test]
    fn test_historical_candle_writer_reconnect_succeeds() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Simulate disconnect — point to a NEW drain server for reconnect.
        writer.sender = None;
        let reconnect_port = spawn_tcp_drain_server();
        writer.ilp_conf_string = format!("tcp::addr=127.0.0.1:{reconnect_port};");

        let result = writer.try_reconnect_on_error();
        assert!(result.is_ok(), "reconnect to drain server must succeed");
        assert!(
            writer.sender.is_some(),
            "sender must be Some after successful reconnect"
        );
    }

    #[test]
    fn test_historical_candle_reconnect_constants() {
        assert_eq!(HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS, 3);
        assert_eq!(HISTORICAL_CANDLE_RECONNECT_INITIAL_DELAY_MS, 1000);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: CandlePersistenceWriter (historical) reconnect path
    // (candle_persistence lines 262-267 — try_reconnect_on_error calling reconnect)
    // -----------------------------------------------------------------------

    #[test]
    fn test_historical_candle_writer_reconnect_on_append() {
        // Scenario: historical candle writer sender is None, append_candle
        // calls try_reconnect_on_error which calls reconnect().
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Kill sender to simulate disconnect.
        writer.sender = None;

        // Point to a new working server for reconnect.
        let port2 = spawn_tcp_drain_server();
        writer.ilp_conf_string = format!("tcp::addr=127.0.0.1:{port2};");

        // append_candle should trigger try_reconnect_on_error → reconnect.
        let candle = make_test_candle(42);
        let result = writer.append_candle(&candle);
        assert!(result.is_ok(), "append_candle must succeed after reconnect");
        assert!(
            writer.sender.is_some(),
            "sender must be Some after reconnect"
        );
        assert_eq!(writer.pending_count(), 1);
    }

    #[test]
    fn test_historical_candle_writer_reconnect_fails_with_unreachable() {
        // Scenario: sender is None, reconnect fails (unreachable host).
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Kill sender and point to unreachable host.
        writer.sender = None;
        writer.ilp_conf_string = "tcp::addr=127.0.0.1:1;".to_string();

        // append_candle should return error (reconnect failed).
        let candle = make_test_candle(42);
        let result = writer.append_candle(&candle);
        assert!(
            result.is_err(),
            "append_candle must fail when reconnect fails"
        );
        assert!(
            writer.sender.is_none(),
            "sender must remain None after failed reconnect"
        );
    }

    #[test]
    fn test_historical_candle_writer_force_flush_disconnected() {
        // Scenario: force_flush with data when sender is None.
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Append a candle, then kill sender.
        let candle = make_test_candle(42);
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);

        writer.sender = None;

        // force_flush should fail since sender is None.
        let result = writer.force_flush();
        assert!(result.is_err(), "force_flush must fail with None sender");
    }

    #[test]
    fn test_historical_candle_writer_reconnect_fails_all_attempts() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Simulate disconnect and point to unreachable host.
        writer.sender = None;
        writer.ilp_conf_string = "tcp::addr=127.0.0.1:1;".to_string();

        // try_reconnect_on_error triggers reconnect() which should fail.
        let result = writer.try_reconnect_on_error();
        assert!(result.is_err());
        assert!(writer.sender.is_none());
    }

    #[test]
    fn test_historical_candle_writer_force_flush_disconnected_propagates_error() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Append a candle.
        let candle = make_test_candle(42);
        writer.append_candle(&candle).unwrap();
        assert!(writer.pending_count() > 0);

        // Kill sender to simulate disconnect.
        writer.sender = None;
        writer.ilp_conf_string = "tcp::addr=127.0.0.1:1;".to_string();

        // Force flush when disconnected should propagate error.
        let result = writer.force_flush();
        assert!(result.is_err());
    }

    // =======================================================================
    // Coverage: CandlePersistenceWriter force_flush with real sender broken pipe
    // (candle_persistence lines 201-205)
    // =======================================================================

    #[test]
    fn test_historical_candle_force_flush_broken_pipe() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ilp_port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                // Drop immediately to break pipe.
            }
        });

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port,
            http_port: ilp_port,
            pg_port: ilp_port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        assert!(writer.sender.is_some());

        // Append some candles.
        for i in 0..3_u32 {
            let candle = make_test_candle(i);
            writer.append_candle(&candle).unwrap();
        }
        assert!(writer.pending_count() > 0);

        // Wait for TCP drop.
        std::thread::sleep(Duration::from_millis(100));

        // Block reconnect.
        writer.ilp_conf_string = "tcp::addr=127.0.0.1:1;".to_string();

        // Wait for TCP drop.
        std::thread::sleep(Duration::from_millis(200));

        // Append many candles to overflow TCP send buffer.
        for i in 0..500_u32 {
            let candle = HistoricalCandle {
                security_id: i,
                exchange_segment_code: 2,
                timeframe: "1m",
                timestamp_utc_secs: 1_704_067_200 + i64::from(i) * 60,
                open: 21000.0,
                high: 21050.0,
                low: 20950.0,
                close: 21025.0,
                volume: 100_000,
                open_interest: 0,
            };
            let _ = writer.append_candle(&candle);
        }

        // Block reconnect.
        writer.ilp_conf_string = "tcp::addr=127.0.0.1:1;".to_string();

        // Force flush — may or may not detect broken pipe.
        let result = writer.force_flush();
        if result.is_err() {
            // Flush failed — verify error path (lines 201-205).
            assert!(writer.sender.is_none());
            assert_eq!(writer.pending_count(), 0);
        }
    }

    // =======================================================================
    // Coverage: CandlePersistenceWriter append_candle auto-flush at threshold
    // (candle_persistence line 184)
    // =======================================================================

    #[test]
    fn test_historical_candle_append_triggers_auto_flush_at_batch_size() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Append CANDLE_FLUSH_BATCH_SIZE candles — the last one should trigger auto-flush.
        for i in 0..CANDLE_FLUSH_BATCH_SIZE as u32 {
            let candle = HistoricalCandle {
                security_id: i,
                exchange_segment_code: 2,
                timeframe: "1m",
                timestamp_utc_secs: 1_704_067_200 + i64::from(i) * 60,
                open: 21000.0,
                high: 21050.0,
                low: 20950.0,
                close: 21025.0,
                volume: 100_000,
                open_interest: 0,
            };
            let result = writer.append_candle(&candle);
            assert!(result.is_ok(), "append_candle must succeed for candle {i}");
        }
        // After auto-flush, pending should be 0.
        assert_eq!(
            writer.pending_count(),
            0,
            "pending must be 0 after auto-flush triggered at batch size"
        );
    }

    // =======================================================================
    // Coverage: CandlePersistenceWriter reconnect failure (lines 219-258) — 7s test
    // =======================================================================

    #[test]
    fn test_historical_candle_reconnect_failure_with_unreachable_host() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        writer.sender = None;
        writer.ilp_conf_string = "tcp::addr=127.0.0.1:1;".to_string();

        // reconnect() tries 3 times with backoff (1s + 2s + 4s = 7s).
        let result = writer.reconnect();
        assert!(
            result.is_err(),
            "reconnect must fail after 3 attempts to unreachable host"
        );
        assert!(writer.sender.is_none());
    }

    // =======================================================================
    // Coverage: ensure_candle_table_dedup_keys DDL non-success paths
    // (candle_persistence lines 1064, 1085, 1118)
    // =======================================================================

    #[tokio::test]
    async fn test_ensure_candle_table_ddl_non_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_candle_table_dedup_keys(&config).await;
    }

    // =======================================================================
    // Coverage: ensure_candle_table DDL send error (CREATE succeeds, DEDUP fails)
    // (candle_persistence lines 1085, 1118)
    // =======================================================================

    #[tokio::test]
    async fn test_ensure_candle_table_ddl_create_ok_dedup_fail() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // First connection: respond with 200 (CREATE TABLE succeeds).
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(MOCK_HTTP_200.as_bytes()).await;
                drop(stream);
            }
            // Second connection: drop immediately (DEDUP DDL fails).
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_candle_table_dedup_keys(&config).await;
    }

    // =======================================================================
    // Coverage: ensure_candle_table CREATE DDL send error (line 1064)
    // =======================================================================

    #[tokio::test]
    async fn test_ensure_candle_table_create_ddl_send_error() {
        let config = QuestDbConfig {
            host: "unreachable-host-12345".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        ensure_candle_table_dedup_keys(&config).await;
    }

    // =======================================================================
    // Coverage: compute_ist_nanos_from_utc_secs edge cases
    // =======================================================================

    #[test]
    fn test_compute_ist_nanos_zero() {
        let result = compute_ist_nanos_from_utc_secs(0);
        assert_eq!(result, IST_UTC_OFFSET_SECONDS_I64 * 1_000_000_000);
    }

    #[test]
    fn test_compute_ist_nanos_adds_offset_then_multiplies() {
        let utc_secs = 1000;
        let result = compute_ist_nanos_from_utc_secs(utc_secs);
        let expected = (utc_secs + IST_UTC_OFFSET_SECONDS_I64) * 1_000_000_000;
        assert_eq!(result, expected);
    }

    // =======================================================================
    // Coverage: should_flush edge cases
    // =======================================================================

    #[test]
    fn test_should_flush_exactly_one_below() {
        assert!(!should_flush(99, 100));
    }

    #[test]
    fn test_should_flush_exactly_equal() {
        assert!(should_flush(100, 100));
    }

    // =======================================================================
    // Coverage: DDL content checks
    // =======================================================================

    #[test]
    fn test_historical_candles_ddl_has_all_ohlcv_columns() {
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("open DOUBLE"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("high DOUBLE"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("low DOUBLE"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("close DOUBLE"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("volume LONG"));
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("oi LONG"));
    }

    #[test]
    fn test_historical_candles_ddl_partition_by_day() {
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("PARTITION BY DAY WAL"));
    }

    #[test]
    fn test_historical_candles_ddl_has_segment() {
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("segment SYMBOL"));
    }

    #[test]
    fn test_historical_candles_ddl_idempotent() {
        assert!(HISTORICAL_CANDLES_CREATE_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_historical_candles_ddl_no_semicolons() {
        assert!(!HISTORICAL_CANDLES_CREATE_DDL.contains(';'));
    }

    // =======================================================================
    // Coverage: build helpers
    // =======================================================================

    #[test]
    fn test_build_questdb_exec_url_with_ipv4() {
        let url = build_questdb_exec_url("192.168.1.1", 9000);
        assert_eq!(url, "http://192.168.1.1:9000/exec");
    }

    #[test]
    fn test_build_dedup_sql_contains_alter() {
        let sql = build_dedup_sql("my_table", "col_a, col_b");
        assert!(sql.starts_with("ALTER TABLE my_table"));
        assert!(sql.contains("DEDUP ENABLE UPSERT KEYS(ts, col_a, col_b)"));
    }

    #[test]
    fn test_build_ilp_conf_string_format() {
        let conf = build_ilp_conf_string("myhost", 9009);
        assert_eq!(conf, "tcp::addr=myhost:9009;");
    }

    // =======================================================================
    // Coverage: DEDUP key includes segment
    // =======================================================================

    #[test]
    fn test_dedup_key_candles_includes_segment() {
        assert!(DEDUP_KEY_CANDLES.contains("segment"));
    }

    #[test]
    fn test_historical_candle_constants() {
        assert_eq!(HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS, 3);
        assert_eq!(HISTORICAL_CANDLE_RECONNECT_INITIAL_DELAY_MS, 1000);
    }

    // =======================================================================
    // Coverage: historical writer force_flush error path (line 206)
    // =======================================================================

    #[test]
    fn test_historical_force_flush_error_path_sets_sender_none_and_returns_error() {
        // Scenario: Append a candle, drop the TCP server so flush fails,
        // then verify the error path nulls sender and resets pending.
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        assert!(writer.sender.is_some());

        // Append a candle so pending > 0
        let candle = make_test_candle(11536);
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);

        // Disconnect — simulate broken pipe by dropping sender and setting
        // unreachable conf so reconnect fails.
        writer.sender = None;
        writer.ilp_conf_string = "tcp::addr=127.0.0.1:1;".to_string();

        // force_flush with sender=None should error (context "sender disconnected")
        let result = writer.force_flush();
        assert!(
            result.is_err(),
            "force_flush with sender=None must return error"
        );
    }

    // =======================================================================
    // Coverage: CandlePersistenceWriter::force_flush error path (line 206)
    //
    // When sender.flush() fails, sender is set to None. Line 206 then
    // evaluates `if let Some(ref s) = self.sender` — which is None since
    // we just set it to None. This branch is always false, but the code
    // must produce a fallback Buffer::new(ProtocolVersion::V1).
    // =======================================================================

    #[test]
    fn test_candle_persistence_writer_force_flush_sender_none_returns_err() {
        // When sender is None, force_flush returns Err at the .context() call
        // on line 200. pending_count is NOT reset because the error exits early.
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        assert!(writer.sender.is_some());

        let candle = make_test_candle(13);
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);

        // Set sender to None — exercises line 200 (sender disconnected error).
        writer.sender = None;
        let result = writer.force_flush();
        assert!(
            result.is_err(),
            "force_flush with sender=None and pending>0 must return Err"
        );
    }

    #[test]
    fn test_candle_persistence_writer_force_flush_sender_error_path() {
        // To exercise lines 201-210 (sender.flush() fails), we need a sender
        // that's connected but will fail on flush. Strategy: connect to a TCP
        // server that closes immediately, then try to flush.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                // Close immediately
                drop(stream);
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(50));

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let writer_result = CandlePersistenceWriter::new(&config);
        if writer_result.is_err() {
            return; // Connection refused — cannot test this path
        }
        let mut writer = writer_result.unwrap();

        // Write data to buffer
        for _ in 0..10 {
            let candle = make_test_candle(42);
            let _ = writer.append_candle(&candle);
        }

        // Server has closed the connection — wait for broken pipe detection
        std::thread::sleep(std::time::Duration::from_millis(100));

        let result = writer.force_flush();
        // flush may fail with broken pipe → lines 201-210 executed
        // Either way, the function must not panic
        if result.is_err() {
            // Lines 201-210 were hit: sender set to None, pending reset
            assert!(
                writer.sender.is_none(),
                "sender must be None after flush error"
            );
            assert_eq!(
                writer.pending_count(),
                0,
                "pending must be reset on flush error"
            );
        }
    }
}
