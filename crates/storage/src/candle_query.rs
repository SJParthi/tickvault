//! Historical candle queries via QuestDB HTTP REST API.
//!
//! Builds SAMPLE BY queries against the `ticks` table to compute OHLCV candles
//! at any time-based interval on-demand. This avoids pre-computing candle tables —
//! raw ticks are the single source of truth.
//!
//! # Performance
//! - QuestDB SAMPLE BY is highly optimized for time-series aggregation
//! - IST alignment via `ALIGN TO CALENDAR WITH OFFSET '05:30'`
//! - For sub-second queries on large datasets, use the `candles_1s` materialized view
//!
//! # Tick-Based Candles
//! Tick-count candles (1T, 10T, etc.) cannot use SAMPLE BY since they're not
//! time-aligned. Instead, we fetch raw ticks and aggregate client-side.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use tracing::{debug, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::QUESTDB_TABLE_TICKS;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB candle query HTTP requests.
const QUESTDB_QUERY_TIMEOUT_SECS: u64 = 15;

/// Maximum number of candles to return in a single query.
const MAX_CANDLE_RESULTS: usize = 5000;

/// IST offset string for QuestDB SAMPLE BY alignment.
const IST_CALENDAR_OFFSET: &str = "05:30";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single OHLCV candle row returned from QuestDB.
#[derive(Debug, Clone)]
pub struct CandleRow {
    /// Candle open time as Unix epoch seconds (UTC).
    pub timestamp: i64,
    /// Open price (first LTP in the interval).
    pub open: f64,
    /// High price (max LTP in the interval).
    pub high: f64,
    /// Low price (min LTP in the interval).
    pub low: f64,
    /// Close price (last LTP in the interval).
    pub close: f64,
    /// Volume delta for this interval.
    pub volume: i64,
}

/// QuestDB HTTP `/exec` JSON response envelope.
#[derive(Debug, Deserialize)]
struct QuestDbResponse {
    /// Column metadata.
    #[allow(dead_code)]
    columns: Vec<QuestDbColumn>,
    /// Row data — each row is a JSON array.
    dataset: Vec<Vec<serde_json::Value>>,
    /// Number of rows returned.
    #[allow(dead_code)]
    count: usize,
}

/// Column metadata in QuestDB response.
#[derive(Debug, Deserialize)]
struct QuestDbColumn {
    /// Column name.
    #[allow(dead_code)]
    name: String,
    /// Column type (e.g., "TIMESTAMP", "DOUBLE", "LONG").
    #[allow(dead_code)]
    #[serde(rename = "type")]
    column_type: String,
}

// ---------------------------------------------------------------------------
// QuestDB Candle Querier
// ---------------------------------------------------------------------------

/// Queries QuestDB for historical OHLCV candles via the HTTP REST API.
///
/// Uses SAMPLE BY for time-based aggregation of raw tick data.
/// Thread-safe: uses `reqwest::Client` which is internally `Arc`-wrapped.
#[derive(Clone)]
pub struct QuestDbCandleQuerier {
    client: Client,
    base_url: String,
}

impl QuestDbCandleQuerier {
    /// Creates a new candle querier for the given QuestDB config.
    ///
    /// # Errors
    /// Returns error if the HTTP client cannot be constructed.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(QUESTDB_QUERY_TIMEOUT_SECS))
            .build()
            .context("failed to build HTTP client for QuestDB candle queries")?;

        let base_url = format!("http://{}:{}/exec", config.host, config.http_port);

        Ok(Self { client, base_url })
    }

    /// Queries OHLCV candles for a security at a given time-based interval.
    ///
    /// # Arguments
    /// * `security_id` — Dhan security identifier
    /// * `interval_label` — SAMPLE BY interval (e.g., "1s", "5m", "1h", "1d")
    /// * `from_epoch` — Start time as Unix epoch seconds (UTC)
    /// * `to_epoch` — End time as Unix epoch seconds (UTC)
    ///
    /// # Returns
    /// Vector of `CandleRow` sorted by timestamp ascending.
    ///
    /// # Errors
    /// Returns error on network failure, invalid SQL, or response parse failure.
    pub async fn query_candles(
        &self,
        security_id: u32,
        interval_label: &str,
        from_epoch: i64,
        to_epoch: i64,
    ) -> Result<Vec<CandleRow>> {
        let sample_by = timeframe_to_sample_by(interval_label)?;

        // Build the SAMPLE BY query.
        // Uses first(ltp)/last(ltp) for open/close, max/min for high/low.
        // Volume delta = last(volume) - first(volume).
        // Volume delta: last(volume) - first(volume).
        // Use max(..., 0) to protect against volume resets mid-day
        // where cumulative volume drops (exchange correction).
        let sql = format!(
            "SELECT ts, first(ltp) as open, max(ltp) as high, min(ltp) as low, \
             last(ltp) as close, \
             case when last(volume) > first(volume) then last(volume) - first(volume) else 0 end as vol \
             FROM {table} \
             WHERE security_id = {sid} \
             AND ts >= cast({from_us} as timestamp) \
             AND ts <= cast({to_us} as timestamp) \
             SAMPLE BY {sample_by} ALIGN TO CALENDAR WITH OFFSET '{offset}' \
             LIMIT {limit}",
            table = QUESTDB_TABLE_TICKS,
            sid = security_id,
            from_us = from_epoch.saturating_mul(1_000_000), // epoch seconds → microseconds for QuestDB
            to_us = to_epoch.saturating_mul(1_000_000),
            sample_by = sample_by,
            offset = IST_CALENDAR_OFFSET,
            limit = MAX_CANDLE_RESULTS,
        );

        debug!(
            security_id,
            interval = interval_label,
            sample_by = sample_by,
            "querying QuestDB for candles"
        );

        let response = self
            .client
            .get(&self.base_url)
            .query(&[("query", &sql)])
            .send()
            .await
            .context("QuestDB candle query HTTP request failed")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "QuestDB candle query returned {}: {}",
                status,
                body.chars().take(200).collect::<String>()
            );
        }

        let questdb_response: QuestDbResponse = response
            .json()
            .await
            .context("failed to parse QuestDB candle query response")?;

        let candles = parse_candle_rows(&questdb_response.dataset)?;

        debug!(
            security_id,
            interval = interval_label,
            candle_count = candles.len(),
            "candle query complete"
        );

        Ok(candles)
    }

    /// Queries raw tick data for tick-count candle aggregation.
    ///
    /// Returns raw (timestamp, ltp, volume) tuples that the caller
    /// aggregates into tick-count candles client-side.
    ///
    /// # Arguments
    /// * `security_id` — Dhan security identifier
    /// * `from_epoch` — Start time as Unix epoch seconds (UTC)
    /// * `to_epoch` — End time as Unix epoch seconds (UTC)
    /// * `limit` — Maximum number of ticks to fetch
    pub async fn query_raw_ticks(
        &self,
        security_id: u32,
        from_epoch: i64,
        to_epoch: i64,
        limit: usize,
    ) -> Result<Vec<RawTickRow>> {
        let sql = format!(
            "SELECT ts, ltp, volume FROM {table} \
             WHERE security_id = {sid} \
             AND ts >= cast({from_us} as timestamp) \
             AND ts <= cast({to_us} as timestamp) \
             ORDER BY ts ASC \
             LIMIT {limit}",
            table = QUESTDB_TABLE_TICKS,
            sid = security_id,
            from_us = from_epoch.saturating_mul(1_000_000),
            to_us = to_epoch.saturating_mul(1_000_000),
            limit = limit,
        );

        let response = self
            .client
            .get(&self.base_url)
            .query(&[("query", &sql)])
            .send()
            .await
            .context("QuestDB raw tick query failed")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "QuestDB raw tick query returned {}: {}",
                status,
                body.chars().take(200).collect::<String>()
            );
        }

        let questdb_response: QuestDbResponse = response
            .json()
            .await
            .context("failed to parse QuestDB raw tick response")?;

        let ticks = parse_raw_tick_rows(&questdb_response.dataset)?;

        debug!(
            security_id,
            tick_count = ticks.len(),
            "raw tick query complete"
        );

        Ok(ticks)
    }

    /// Aggregates raw ticks into tick-count candles.
    ///
    /// Groups every `ticks_per_candle` ticks into one OHLCV candle.
    pub fn aggregate_tick_candles(ticks: &[RawTickRow], ticks_per_candle: u32) -> Vec<CandleRow> {
        if ticks.is_empty() || ticks_per_candle == 0 {
            return Vec::new();
        }

        let mut candles = Vec::new();
        let n = ticks_per_candle as usize;

        for chunk in ticks.chunks(n) {
            if chunk.is_empty() {
                continue;
            }

            let open = chunk[0].ltp;
            let close = chunk[chunk.len() - 1].ltp;
            let high = chunk
                .iter()
                .map(|t| t.ltp)
                .fold(f64::NEG_INFINITY, f64::max);
            let low = chunk.iter().map(|t| t.ltp).fold(f64::INFINITY, f64::min);
            let volume = chunk[chunk.len() - 1].volume - chunk[0].volume;
            let timestamp = chunk[0].timestamp;

            candles.push(CandleRow {
                timestamp,
                open,
                high,
                low,
                close,
                volume,
            });
        }

        candles
    }
}

/// A raw tick row for tick-count candle aggregation.
#[derive(Debug, Clone)]
pub struct RawTickRow {
    /// Tick time as Unix epoch seconds (UTC).
    pub timestamp: i64,
    /// Last traded price.
    pub ltp: f64,
    /// Cumulative volume.
    pub volume: i64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Converts a timeframe label to a QuestDB SAMPLE BY clause.
///
/// QuestDB SAMPLE BY syntax:
/// - `1s`, `5s`, `30s` → `1s`, `5s`, `30s`
/// - `1m`, `5m`, `30m` → `1m`, `5m`, `30m`
/// - `1h`, `4h` → `1h`, `4h`
/// - `1D` → `1d` (QuestDB uses lowercase `d`)
/// - `1W` → `7d`
/// - `1M` → `1M` (QuestDB supports month intervals)
///
/// # Errors
/// Returns error for unrecognized timeframe labels.
fn timeframe_to_sample_by(label: &str) -> Result<String> {
    match label {
        // Seconds
        "1s" => Ok("1s".to_string()),
        "5s" => Ok("5s".to_string()),
        "10s" => Ok("10s".to_string()),
        "15s" => Ok("15s".to_string()),
        "30s" => Ok("30s".to_string()),
        "45s" => Ok("45s".to_string()),
        // Minutes
        "1m" => Ok("1m".to_string()),
        "2m" => Ok("2m".to_string()),
        "3m" => Ok("3m".to_string()),
        "4m" => Ok("4m".to_string()),
        "5m" => Ok("5m".to_string()),
        "10m" => Ok("10m".to_string()),
        "15m" => Ok("15m".to_string()),
        "25m" => Ok("25m".to_string()),
        "30m" => Ok("30m".to_string()),
        "45m" => Ok("45m".to_string()),
        // Hours
        "1h" => Ok("1h".to_string()),
        "2h" => Ok("2h".to_string()),
        "3h" => Ok("3h".to_string()),
        "4h" => Ok("4h".to_string()),
        // Days+
        "1D" => Ok("1d".to_string()),
        "5D" => Ok("5d".to_string()),
        "1W" => Ok("7d".to_string()),
        "1M" => Ok("1M".to_string()),
        "3M" => Ok("3M".to_string()),
        "6M" => Ok("6M".to_string()),
        "1Y" => Ok("12M".to_string()),
        _ => {
            // Try to parse custom time format (e.g., "7m", "90s")
            let trimmed = label.trim();
            if trimmed.len() < 2 {
                bail!("unrecognized timeframe: '{}'", label);
            }
            let (num_str, unit) = trimmed.split_at(trimmed.len() - 1);
            let n: i64 = num_str
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid timeframe number: '{}'", label))?;
            if n <= 0 {
                bail!("timeframe must be positive: '{}'", label);
            }
            match unit {
                "s" => Ok(format!("{n}s")),
                "m" => Ok(format!("{n}m")),
                "h" => Ok(format!("{n}h")),
                _ => bail!("unrecognized timeframe unit: '{}'", label),
            }
        }
    }
}

/// Parses QuestDB dataset rows into `CandleRow` structs.
///
/// Expected column order: ts, open, high, low, close, vol
fn parse_candle_rows(dataset: &[Vec<serde_json::Value>]) -> Result<Vec<CandleRow>> {
    let mut candles = Vec::with_capacity(dataset.len());

    for (idx, row) in dataset.iter().enumerate() {
        if row.len() < 6 {
            warn!(row_index = idx, columns = row.len(), "skipping short row");
            continue;
        }

        let timestamp = parse_questdb_timestamp(&row[0])
            .with_context(|| format!("row {idx}: invalid timestamp"))?;

        let open = extract_f64(&row[1]).with_context(|| format!("row {idx}: invalid open"))?;
        let high = extract_f64(&row[2]).with_context(|| format!("row {idx}: invalid high"))?;
        let low = extract_f64(&row[3]).with_context(|| format!("row {idx}: invalid low"))?;
        let close = extract_f64(&row[4]).with_context(|| format!("row {idx}: invalid close"))?;
        let volume = extract_i64(&row[5]).with_context(|| format!("row {idx}: invalid volume"))?;

        // Skip empty candles where all OHLC are null/zero
        if open == 0.0 && high == 0.0 && low == 0.0 && close == 0.0 {
            continue;
        }

        candles.push(CandleRow {
            timestamp,
            open,
            high,
            low,
            close,
            volume,
        });
    }

    Ok(candles)
}

/// Parses QuestDB dataset rows into `RawTickRow` structs.
///
/// Expected column order: ts, ltp, volume
fn parse_raw_tick_rows(dataset: &[Vec<serde_json::Value>]) -> Result<Vec<RawTickRow>> {
    let mut ticks = Vec::with_capacity(dataset.len());

    for (idx, row) in dataset.iter().enumerate() {
        if row.len() < 3 {
            warn!(
                row_index = idx,
                columns = row.len(),
                "skipping short tick row"
            );
            continue;
        }

        let timestamp = parse_questdb_timestamp(&row[0])
            .with_context(|| format!("tick row {idx}: invalid timestamp"))?;
        let ltp = extract_f64(&row[1]).with_context(|| format!("tick row {idx}: invalid ltp"))?;
        let volume =
            extract_i64(&row[2]).with_context(|| format!("tick row {idx}: invalid volume"))?;

        ticks.push(RawTickRow {
            timestamp,
            ltp,
            volume,
        });
    }

    Ok(ticks)
}

/// Parses a QuestDB timestamp string to Unix epoch seconds.
///
/// QuestDB returns timestamps as ISO 8601 strings: "2026-02-26T09:15:00.000000Z"
fn parse_questdb_timestamp(value: &serde_json::Value) -> Result<i64> {
    match value {
        serde_json::Value::String(s) => {
            // Parse ISO 8601 timestamp
            let dt = chrono::DateTime::parse_from_rfc3339(s)
                .or_else(|_| {
                    // QuestDB sometimes returns without timezone suffix
                    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ")
                        .map(|ndt| ndt.and_utc().fixed_offset())
                })
                .with_context(|| format!("cannot parse timestamp: '{s}'"))?;
            Ok(dt.timestamp())
        }
        serde_json::Value::Number(n) => {
            // Microseconds since epoch (QuestDB internal format)
            let us = n.as_i64().context("timestamp number not i64")?;
            Ok(us / 1_000_000) // microseconds → seconds
        }
        _ => bail!("unexpected timestamp value type: {:?}", value),
    }
}

/// Extracts an f64 from a JSON value (number or null → 0.0).
fn extract_f64(value: &serde_json::Value) -> Result<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64().context("number not convertible to f64"),
        serde_json::Value::Null => Ok(0.0),
        _ => bail!("expected number, got: {:?}", value),
    }
}

/// Extracts an i64 from a JSON value (number or null → 0).
fn extract_i64(value: &serde_json::Value) -> Result<i64> {
    match value {
        serde_json::Value::Number(n) => n.as_i64().context("number not convertible to i64"),
        serde_json::Value::Null => Ok(0),
        _ => bail!("expected number, got: {:?}", value),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- timeframe_to_sample_by ---

    #[test]
    fn test_sample_by_standard_seconds() {
        assert_eq!(timeframe_to_sample_by("1s").unwrap(), "1s");
        assert_eq!(timeframe_to_sample_by("5s").unwrap(), "5s");
        assert_eq!(timeframe_to_sample_by("10s").unwrap(), "10s");
        assert_eq!(timeframe_to_sample_by("15s").unwrap(), "15s");
        assert_eq!(timeframe_to_sample_by("30s").unwrap(), "30s");
        assert_eq!(timeframe_to_sample_by("45s").unwrap(), "45s");
    }

    #[test]
    fn test_sample_by_standard_minutes() {
        assert_eq!(timeframe_to_sample_by("1m").unwrap(), "1m");
        assert_eq!(timeframe_to_sample_by("5m").unwrap(), "5m");
        assert_eq!(timeframe_to_sample_by("15m").unwrap(), "15m");
        assert_eq!(timeframe_to_sample_by("25m").unwrap(), "25m");
        assert_eq!(timeframe_to_sample_by("30m").unwrap(), "30m");
        assert_eq!(timeframe_to_sample_by("45m").unwrap(), "45m");
    }

    #[test]
    fn test_sample_by_standard_hours() {
        assert_eq!(timeframe_to_sample_by("1h").unwrap(), "1h");
        assert_eq!(timeframe_to_sample_by("2h").unwrap(), "2h");
        assert_eq!(timeframe_to_sample_by("3h").unwrap(), "3h");
        assert_eq!(timeframe_to_sample_by("4h").unwrap(), "4h");
    }

    #[test]
    fn test_sample_by_daily_and_above() {
        assert_eq!(timeframe_to_sample_by("1D").unwrap(), "1d");
        assert_eq!(timeframe_to_sample_by("5D").unwrap(), "5d");
        assert_eq!(timeframe_to_sample_by("1W").unwrap(), "7d");
        assert_eq!(timeframe_to_sample_by("1M").unwrap(), "1M");
        assert_eq!(timeframe_to_sample_by("3M").unwrap(), "3M");
        assert_eq!(timeframe_to_sample_by("6M").unwrap(), "6M");
        assert_eq!(timeframe_to_sample_by("1Y").unwrap(), "12M");
    }

    #[test]
    fn test_sample_by_custom_intervals() {
        assert_eq!(timeframe_to_sample_by("7m").unwrap(), "7m");
        assert_eq!(timeframe_to_sample_by("90s").unwrap(), "90s");
        assert_eq!(timeframe_to_sample_by("6h").unwrap(), "6h");
    }

    #[test]
    fn test_sample_by_invalid_label() {
        assert!(timeframe_to_sample_by("").is_err());
        assert!(timeframe_to_sample_by("x").is_err());
        assert!(timeframe_to_sample_by("abc").is_err());
        assert!(timeframe_to_sample_by("10T").is_err()); // tick-based not supported
    }

    #[test]
    fn test_sample_by_zero_or_negative() {
        assert!(timeframe_to_sample_by("0s").is_err());
        assert!(timeframe_to_sample_by("-1m").is_err());
    }

    // --- parse_questdb_timestamp ---

    #[test]
    fn test_parse_timestamp_iso_string() {
        let val = serde_json::Value::String("2026-02-26T09:15:00.000000Z".to_string());
        let ts = parse_questdb_timestamp(&val).unwrap();
        assert_eq!(ts, 1772097300); // 2026-02-26T09:15:00 UTC
    }

    #[test]
    fn test_parse_timestamp_microseconds_number() {
        // 1772097300 seconds = 1772097300000000 microseconds
        let val = serde_json::json!(1_772_097_300_000_000_i64);
        let ts = parse_questdb_timestamp(&val).unwrap();
        assert_eq!(ts, 1772097300);
    }

    #[test]
    fn test_parse_timestamp_invalid_type() {
        let val = serde_json::json!(true);
        assert!(parse_questdb_timestamp(&val).is_err());
    }

    // --- extract_f64 ---

    #[test]
    fn test_extract_f64_number() {
        let val = serde_json::json!(24500.5);
        assert!((extract_f64(&val).unwrap() - 24500.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_extract_f64_null() {
        let val = serde_json::Value::Null;
        assert!((extract_f64(&val).unwrap() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_extract_f64_integer() {
        let val = serde_json::json!(100);
        assert!((extract_f64(&val).unwrap() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_extract_f64_string_fails() {
        let val = serde_json::json!("not a number");
        assert!(extract_f64(&val).is_err());
    }

    // --- extract_i64 ---

    #[test]
    fn test_extract_i64_number() {
        let val = serde_json::json!(50000);
        assert_eq!(extract_i64(&val).unwrap(), 50000);
    }

    #[test]
    fn test_extract_i64_null() {
        let val = serde_json::Value::Null;
        assert_eq!(extract_i64(&val).unwrap(), 0);
    }

    // --- parse_candle_rows ---

    #[test]
    fn test_parse_candle_rows_valid() {
        let dataset = vec![
            vec![
                serde_json::json!("2026-02-26T09:15:00.000000Z"),
                serde_json::json!(24500.0),
                serde_json::json!(24520.0),
                serde_json::json!(24480.0),
                serde_json::json!(24510.0),
                serde_json::json!(12345),
            ],
            vec![
                serde_json::json!("2026-02-26T09:16:00.000000Z"),
                serde_json::json!(24510.0),
                serde_json::json!(24530.0),
                serde_json::json!(24500.0),
                serde_json::json!(24525.0),
                serde_json::json!(8000),
            ],
        ];

        let candles = parse_candle_rows(&dataset).unwrap();
        assert_eq!(candles.len(), 2);
        assert!((candles[0].open - 24500.0).abs() < f64::EPSILON);
        assert!((candles[0].high - 24520.0).abs() < f64::EPSILON);
        assert!((candles[0].low - 24480.0).abs() < f64::EPSILON);
        assert!((candles[0].close - 24510.0).abs() < f64::EPSILON);
        assert_eq!(candles[0].volume, 12345);
    }

    #[test]
    fn test_parse_candle_rows_skips_short_rows() {
        let dataset = vec![vec![
            serde_json::json!("2026-02-26T09:15:00.000000Z"),
            serde_json::json!(24500.0),
        ]];

        let candles = parse_candle_rows(&dataset).unwrap();
        assert!(candles.is_empty());
    }

    #[test]
    fn test_parse_candle_rows_skips_zero_ohlc() {
        let dataset = vec![vec![
            serde_json::json!("2026-02-26T09:15:00.000000Z"),
            serde_json::json!(0.0),
            serde_json::json!(0.0),
            serde_json::json!(0.0),
            serde_json::json!(0.0),
            serde_json::json!(0),
        ]];

        let candles = parse_candle_rows(&dataset).unwrap();
        assert!(candles.is_empty());
    }

    #[test]
    fn test_parse_candle_rows_empty_dataset() {
        let candles = parse_candle_rows(&[]).unwrap();
        assert!(candles.is_empty());
    }

    // --- aggregate_tick_candles ---

    #[test]
    fn test_aggregate_tick_candles_basic() {
        let ticks = vec![
            RawTickRow {
                timestamp: 100,
                ltp: 24500.0,
                volume: 0,
            },
            RawTickRow {
                timestamp: 101,
                ltp: 24510.0,
                volume: 100,
            },
            RawTickRow {
                timestamp: 102,
                ltp: 24490.0,
                volume: 200,
            },
            RawTickRow {
                timestamp: 103,
                ltp: 24505.0,
                volume: 300,
            },
        ];

        let candles = QuestDbCandleQuerier::aggregate_tick_candles(&ticks, 2);
        assert_eq!(candles.len(), 2);

        // First candle: ticks 0-1
        assert!((candles[0].open - 24500.0).abs() < f64::EPSILON);
        assert!((candles[0].close - 24510.0).abs() < f64::EPSILON);
        assert!((candles[0].high - 24510.0).abs() < f64::EPSILON);
        assert!((candles[0].low - 24500.0).abs() < f64::EPSILON);
        assert_eq!(candles[0].volume, 100);

        // Second candle: ticks 2-3
        assert!((candles[1].open - 24490.0).abs() < f64::EPSILON);
        assert!((candles[1].close - 24505.0).abs() < f64::EPSILON);
        assert_eq!(candles[1].volume, 100);
    }

    #[test]
    fn test_aggregate_tick_candles_partial_last() {
        let ticks = vec![
            RawTickRow {
                timestamp: 100,
                ltp: 24500.0,
                volume: 0,
            },
            RawTickRow {
                timestamp: 101,
                ltp: 24510.0,
                volume: 100,
            },
            RawTickRow {
                timestamp: 102,
                ltp: 24490.0,
                volume: 200,
            },
        ];

        // 3 ticks with ticks_per_candle=2 → 2 candles (last is partial with 1 tick)
        let candles = QuestDbCandleQuerier::aggregate_tick_candles(&ticks, 2);
        assert_eq!(candles.len(), 2);
        assert!((candles[1].open - 24490.0).abs() < f64::EPSILON);
        assert!((candles[1].close - 24490.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_aggregate_tick_candles_empty() {
        let candles = QuestDbCandleQuerier::aggregate_tick_candles(&[], 10);
        assert!(candles.is_empty());
    }

    #[test]
    fn test_aggregate_tick_candles_zero_per_candle() {
        let ticks = vec![RawTickRow {
            timestamp: 100,
            ltp: 24500.0,
            volume: 0,
        }];
        let candles = QuestDbCandleQuerier::aggregate_tick_candles(&ticks, 0);
        assert!(candles.is_empty());
    }

    #[test]
    fn test_aggregate_tick_candles_one_per_candle() {
        let ticks = vec![
            RawTickRow {
                timestamp: 100,
                ltp: 24500.0,
                volume: 0,
            },
            RawTickRow {
                timestamp: 101,
                ltp: 24510.0,
                volume: 100,
            },
        ];

        let candles = QuestDbCandleQuerier::aggregate_tick_candles(&ticks, 1);
        assert_eq!(candles.len(), 2);
        // Each tick is its own candle — OHLC all equal to LTP
        assert!((candles[0].open - 24500.0).abs() < f64::EPSILON);
        assert!((candles[0].high - 24500.0).abs() < f64::EPSILON);
        assert!((candles[0].low - 24500.0).abs() < f64::EPSILON);
        assert!((candles[0].close - 24500.0).abs() < f64::EPSILON);
    }

    // --- QuestDbCandleQuerier::new ---

    #[test]
    fn test_querier_new_creates_valid_instance() {
        let config = QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let querier = QuestDbCandleQuerier::new(&config).unwrap();
        assert_eq!(querier.base_url, "http://localhost:9000/exec");
    }

    // --- Integration-style: query against unreachable QuestDB ---

    #[tokio::test]
    async fn test_query_candles_unreachable_questdb_returns_error() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(), // RFC 5737 TEST-NET
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };

        let querier = QuestDbCandleQuerier::new(&config).unwrap();
        let result = querier.query_candles(13, "5m", 0, i64::MAX).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_query_raw_ticks_unreachable_questdb_returns_error() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };

        let querier = QuestDbCandleQuerier::new(&config).unwrap();
        let result = querier.query_raw_ticks(13, 0, i64::MAX, 1000).await;
        assert!(result.is_err());
    }
}
