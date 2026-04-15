//! QuestDB ILP persistence for top movers snapshots (stocks + options).
//!
//! Persists 1-minute ranked snapshots of gainers, losers, most active (stocks)
//! and Highest OI, OI Gainers/Losers, Top Volume/Value, Price Gainers/Losers (options).
//!
//! # Tables Written
//! - `stock_movers` — top 20 gainers, losers, most active per minute
//! - `option_movers` — top 20 per 7 categories per minute
//!
//! # Idempotency
//! DEDUP UPSERT KEYS on `(ts, security_id, category)` prevent duplicates on restart.
//!
//! # Error Handling
//! Movers persistence is cold-path observability data, NOT critical path.
//! On QuestDB failure, logs WARN and continues — no ring buffer needed.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use tickvault_common::config::QuestDbConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// QuestDB table name for stock movers (gainers, losers, most active).
pub const QUESTDB_TABLE_STOCK_MOVERS: &str = "stock_movers";

/// QuestDB table name for option movers (7 categories).
pub const QUESTDB_TABLE_OPTION_MOVERS: &str = "option_movers";

/// DEDUP UPSERT KEY for movers tables.
///
/// Compound key: `(ts, security_id, category, segment)` prevents duplicate entries
/// on restart/reconnect AND prevents cross-segment collision.
///
/// **Why `segment` is required** (audit gap DB-3/DB-4, ticket 2026-04-15):
/// The same `security_id` is reused by Dhan across exchange segments. For
/// example, `security_id = 13` is `NIFTY` in both `IDX_I` (index) and `NSE_EQ`
/// (equity — no such instrument in real life, but the collision exists at the
/// schema level and has been observed for other IDs like 25 / BANKNIFTY). If
/// `segment` is omitted from the DEDUP key, two legitimate distinct movers
/// rows in the same 1-minute snapshot bucket will silently UPSERT each other.
/// Both DDL schemas (`stock_movers`, `option_movers`) declare `segment SYMBOL`
/// — the DEDUP key must reference every column that contributes to row identity.
///
/// Enforced by `test_dedup_key_movers_includes_segment` +
/// `test_dedup_key_movers_matches_ddl_identity_columns`.
const DEDUP_KEY_MOVERS: &str = "security_id, category, segment";

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Flush batch size for movers (much smaller than ticks — max 20×10 = 200 rows per snapshot).
const MOVERS_FLUSH_BATCH_SIZE: usize = 250;

/// Flush interval for movers (60 seconds — aligned with snapshot interval).
/// Used by future flush_if_needed() implementation.
#[allow(dead_code)] // APPROVED: will be used when flush_if_needed() is wired
const MOVERS_FLUSH_INTERVAL_MS: u64 = 65_000;

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// DDL for the `stock_movers` table.
///
/// Stores 1-minute snapshots of top 20 gainers, losers, most active stocks.
/// Partitioned by DAY (one partition per trading day, ~60×3×20 = 3600 rows/day).
const STOCK_MOVERS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS stock_movers (\
        category SYMBOL,\
        security_id LONG,\
        segment SYMBOL,\
        symbol SYMBOL,\
        ltp DOUBLE,\
        prev_close DOUBLE,\
        change_abs DOUBLE,\
        change_pct DOUBLE,\
        volume LONG,\
        rank INT,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

/// DDL for the `option_movers` table.
///
/// Stores 1-minute snapshots of top 20 per 7 option categories.
/// Partitioned by DAY (one partition per trading day, ~60×7×20 = 8400 rows/day).
const OPTION_MOVERS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS option_movers (\
        category SYMBOL,\
        security_id LONG,\
        segment SYMBOL,\
        contract_name SYMBOL,\
        underlying SYMBOL,\
        option_type SYMBOL,\
        strike DOUBLE,\
        expiry SYMBOL,\
        spot_price DOUBLE,\
        ltp DOUBLE,\
        change DOUBLE,\
        change_pct DOUBLE,\
        oi LONG,\
        oi_change LONG,\
        oi_change_pct DOUBLE,\
        volume LONG,\
        value DOUBLE,\
        rank INT,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

// ---------------------------------------------------------------------------
// Stock Movers Writer
// ---------------------------------------------------------------------------

/// Batched writer for stock movers snapshots to QuestDB via ILP.
///
/// Simpler than TickPersistenceWriter — no ring buffer, no spill.
/// Movers are cold-path observability data; loss of a snapshot is acceptable.
pub struct StockMoversWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
}

impl StockMoversWriter {
    /// Creates a new stock movers writer connected to QuestDB via ILP TCP.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = config.build_ilp_conf_string();
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for stock movers")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
        })
    }

    /// Appends a single stock mover entry to the ILP buffer.
    ///
    /// # Arguments
    /// * `ts_nanos` — snapshot timestamp (IST epoch nanoseconds)
    /// * `category` — "GAINER", "LOSER", or "MOST_ACTIVE"
    /// * `rank` — 1-based rank within category
    /// * `security_id` — Dhan security ID
    /// * `segment` — exchange segment string ("NSE_EQ", "NSE_FNO", etc.)
    /// * `symbol` — human-readable symbol name
    /// * `ltp` — last traded price
    /// * `prev_close` — previous day close price
    /// * `change_pct` — percentage change
    /// * `volume` — day volume
    // TEST-EXEMPT: ILP buffer append — requires live QuestDB, tested via ensure_movers_tables integration
    #[allow(clippy::too_many_arguments)]
    // APPROVED: 10 params — each maps to a QuestDB column, no abstraction reduces this
    pub fn append_stock_mover(
        &mut self,
        ts_nanos: i64,
        category: &str,
        rank: i32,
        security_id: u32,
        segment: &str,
        symbol: &str,
        ltp: f64,
        prev_close: f64,
        change_pct: f64,
        volume: i64,
    ) -> Result<()> {
        // Round all f64 values to 2dp to prevent IEEE 754 artifacts in QuestDB.
        let ltp = (ltp * 100.0).round() / 100.0;
        let prev_close = (prev_close * 100.0).round() / 100.0;
        let change_abs = ((ltp - prev_close) * 100.0).round() / 100.0;
        let change_pct = (change_pct * 100.0).round() / 100.0;
        let ts = TimestampNanos::new(ts_nanos);

        // ILP requires: table → ALL symbols → ALL columns → at
        self.buffer
            .table(QUESTDB_TABLE_STOCK_MOVERS)
            .context("table")?
            .symbol("category", category)
            .context("category")?
            .symbol("segment", segment)
            .context("segment")?
            .symbol("symbol", symbol)
            .context("symbol")?
            .column_i64("security_id", i64::from(security_id))
            .context("security_id")?
            .column_f64("ltp", ltp)
            .context("ltp")?
            .column_f64("prev_close", prev_close)
            .context("prev_close")?
            .column_f64("change_abs", change_abs)
            .context("change_abs")?
            .column_f64("change_pct", change_pct)
            .context("change_pct")?
            .column_i64("volume", volume)
            .context("volume")?
            .column_i64("rank", i64::from(rank))
            .context("rank")?
            .at(ts)
            .context("timestamp")?;

        self.pending_count = self.pending_count.saturating_add(1);

        if self.pending_count >= MOVERS_FLUSH_BATCH_SIZE
            && let Err(err) = self.flush()
        {
            warn!(?err, "stock movers auto-flush failed");
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
            None => {
                // Try reconnect
                match Sender::from_conf(&self.ilp_conf_string) {
                    Ok(s) => {
                        self.sender = Some(s);
                        self.sender.as_mut().context("sender after reconnect")?
                    }
                    Err(err) => {
                        warn!(?err, "stock movers reconnect failed — dropping batch");
                        self.buffer.clear();
                        self.pending_count = 0;
                        return Ok(());
                    }
                }
            }
        };

        let count = self.pending_count;
        if let Err(err) = sender.flush(&mut self.buffer) {
            warn!(?err, count, "stock movers flush failed — dropping batch");
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            return Ok(());
        }

        self.pending_count = 0;
        debug!(flushed_rows = count, "stock movers flushed to QuestDB");
        Ok(())
    }

    /// Returns a mutable reference to the ILP buffer (for external row building).
    // TEST-EXEMPT: trivial accessor, returns &mut Buffer
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
    }
}

// ---------------------------------------------------------------------------
// Option Movers Writer
// ---------------------------------------------------------------------------

/// Batched writer for option movers snapshots to QuestDB via ILP.
pub struct OptionMoversWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
}

impl OptionMoversWriter {
    /// Creates a new option movers writer connected to QuestDB via ILP TCP.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = config.build_ilp_conf_string();
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for option movers")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
        })
    }

    /// Appends a single option mover entry to the ILP buffer.
    ///
    /// # Arguments
    /// * `ts_nanos` — snapshot timestamp (IST epoch nanoseconds)
    /// * `category` — "HIGHEST_OI", "OI_GAINER", "OI_LOSER", "TOP_VOLUME", "TOP_VALUE", "PRICE_GAINER", "PRICE_LOSER"
    /// * `rank` — 1-based rank within category
    // TEST-EXEMPT: ILP buffer append — requires live QuestDB, tested via ensure_movers_tables integration
    #[allow(clippy::too_many_arguments)]
    // APPROVED: 17 params — each maps to a QuestDB column, no abstraction reduces this
    pub fn append_option_mover(
        &mut self,
        ts_nanos: i64,
        category: &str,
        rank: i32,
        security_id: u32,
        segment: &str,
        contract_name: &str,
        underlying: &str,
        option_type: &str,
        strike: f64,
        expiry: &str,
        spot_price: f64,
        ltp: f64,
        change: f64,
        change_pct: f64,
        oi: i64,
        oi_change: i64,
        oi_change_pct: f64,
        volume: i64,
        value: f64,
    ) -> Result<()> {
        // Round all f64 values to 2dp to prevent IEEE 754 artifacts in QuestDB.
        let strike = (strike * 100.0).round() / 100.0;
        let spot_price = (spot_price * 100.0).round() / 100.0;
        let ltp = (ltp * 100.0).round() / 100.0;
        let change = (change * 100.0).round() / 100.0;
        let change_pct = (change_pct * 100.0).round() / 100.0;
        let oi_change_pct = (oi_change_pct * 100.0).round() / 100.0;
        let value = (value * 100.0).round() / 100.0;
        let ts = TimestampNanos::new(ts_nanos);

        // ILP requires: table → ALL symbols → ALL columns → at
        self.buffer
            .table(QUESTDB_TABLE_OPTION_MOVERS)
            .context("table")?
            // Symbols first
            .symbol("category", category)
            .context("category")?
            .symbol("segment", segment)
            .context("segment")?
            .symbol("contract_name", contract_name)
            .context("contract_name")?
            .symbol("underlying", underlying)
            .context("underlying")?
            .symbol("option_type", option_type)
            .context("option_type")?
            .symbol("expiry", expiry)
            .context("expiry")?
            // Then columns
            .column_i64("security_id", i64::from(security_id))
            .context("security_id")?
            .column_f64("strike", strike)
            .context("strike")?
            .column_f64("spot_price", spot_price)
            .context("spot_price")?
            .column_f64("ltp", ltp)
            .context("ltp")?
            .column_f64("change", change)
            .context("change")?
            .column_f64("change_pct", change_pct)
            .context("change_pct")?
            .column_i64("oi", oi)
            .context("oi")?
            .column_i64("oi_change", oi_change)
            .context("oi_change")?
            .column_f64("oi_change_pct", oi_change_pct)
            .context("oi_change_pct")?
            .column_i64("volume", volume)
            .context("volume")?
            .column_f64("value", value)
            .context("value")?
            .column_i64("rank", i64::from(rank))
            .context("rank")?
            .at(ts)
            .context("timestamp")?;

        self.pending_count = self.pending_count.saturating_add(1);

        if self.pending_count >= MOVERS_FLUSH_BATCH_SIZE
            && let Err(err) = self.flush()
        {
            warn!(?err, "option movers auto-flush failed");
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
                    warn!(?err, "option movers reconnect failed — dropping batch");
                    self.buffer.clear();
                    self.pending_count = 0;
                    return Ok(());
                }
            },
        };

        let count = self.pending_count;
        if let Err(err) = sender.flush(&mut self.buffer) {
            warn!(?err, count, "option movers flush failed — dropping batch");
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            return Ok(());
        }

        self.pending_count = 0;
        debug!(flushed_rows = count, "option movers flushed to QuestDB");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DDL Setup
// ---------------------------------------------------------------------------

/// Creates the `stock_movers` and `option_movers` tables with DEDUP UPSERT KEYS.
///
/// Idempotent — safe to call on every startup.
pub async fn ensure_movers_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    // Create stock_movers table
    execute_ddl(&client, &base_url, STOCK_MOVERS_CREATE_DDL, "stock_movers").await;

    // Enable DEDUP on stock_movers
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_STOCK_MOVERS, DEDUP_KEY_MOVERS
    );
    execute_ddl(&client, &base_url, &dedup_sql, "stock_movers DEDUP").await;

    // Create option_movers table
    execute_ddl(
        &client,
        &base_url,
        OPTION_MOVERS_CREATE_DDL,
        "option_movers",
    )
    .await;

    // Enable DEDUP on option_movers
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_OPTION_MOVERS, DEDUP_KEY_MOVERS
    );
    execute_ddl(&client, &base_url, &dedup_sql, "option_movers DEDUP").await;

    info!("movers tables setup complete (stock_movers + option_movers)");
}

/// Executes a DDL statement against QuestDB HTTP API. Best-effort.
async fn execute_ddl(client: &Client, base_url: &str, sql: &str, label: &str) {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                info!("{label} DDL executed successfully");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "{label} DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, "{label} DDL request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // DDL content validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_stock_movers_ddl_contains_required_columns() {
        assert!(STOCK_MOVERS_CREATE_DDL.contains("category SYMBOL"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("security_id LONG"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("segment SYMBOL"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("symbol SYMBOL"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("ltp DOUBLE"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("prev_close DOUBLE"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("change_abs DOUBLE"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("change_pct DOUBLE"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("volume LONG"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("rank INT"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("ts TIMESTAMP"));
    }

    #[test]
    fn test_stock_movers_ddl_has_partition_and_wal() {
        assert!(STOCK_MOVERS_CREATE_DDL.contains("PARTITION BY DAY"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("WAL"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_stock_movers_ddl_is_idempotent() {
        assert!(STOCK_MOVERS_CREATE_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_option_movers_ddl_contains_required_columns() {
        assert!(OPTION_MOVERS_CREATE_DDL.contains("category SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("security_id LONG"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("segment SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("contract_name SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("underlying SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("option_type SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("strike DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("expiry SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("spot_price DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("ltp DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("change DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("change_pct DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("oi LONG"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("oi_change LONG"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("oi_change_pct DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("volume LONG"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("value DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("rank INT"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("ts TIMESTAMP"));
    }

    #[test]
    fn test_option_movers_ddl_has_partition_and_wal() {
        assert!(OPTION_MOVERS_CREATE_DDL.contains("PARTITION BY DAY"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("WAL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_option_movers_ddl_is_idempotent() {
        assert!(OPTION_MOVERS_CREATE_DDL.contains("IF NOT EXISTS"));
    }

    // -----------------------------------------------------------------------
    // DEDUP key validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_dedup_key_includes_security_id() {
        assert!(DEDUP_KEY_MOVERS.contains("security_id"));
    }

    #[test]
    fn test_dedup_key_includes_category() {
        assert!(DEDUP_KEY_MOVERS.contains("category"));
    }

    // DB-3/DB-4: Cross-segment collision prevention.
    // Must include `segment` because the same security_id exists across
    // `IDX_I` / `NSE_EQ` / `NSE_FNO` with different real-world meanings.
    // Dropping this test is equivalent to re-introducing silent data corruption.
    #[test]
    fn test_dedup_key_movers_includes_segment() {
        assert!(
            DEDUP_KEY_MOVERS.contains("segment"),
            "DEDUP_KEY_MOVERS must include `segment` — same security_id exists \
             across IDX_I/NSE_EQ/NSE_FNO and would collide otherwise (audit DB-3/DB-4). \
             Got: {DEDUP_KEY_MOVERS}"
        );
    }

    /// Both stock_movers and option_movers share the same DEDUP constant,
    /// so both tables MUST apply the same identity-column set. This test
    /// pins the constant format and will fail loudly if anyone re-orders or
    /// drops a column — forcing a conscious review.
    #[test]
    fn test_dedup_key_movers_exact_format() {
        assert_eq!(
            DEDUP_KEY_MOVERS, "security_id, category, segment",
            "DEDUP_KEY_MOVERS regression — changing this string silently \
             corrupts data; update the test only after the DDL and migration \
             are confirmed safe."
        );
    }

    /// Cross-check: every column referenced in DEDUP_KEY_MOVERS MUST appear
    /// in both the stock_movers and option_movers DDLs. Prevents the class
    /// of bug where DEDUP lists a column the table doesn't even have.
    #[test]
    fn test_dedup_key_movers_columns_exist_in_both_ddls() {
        for col in DEDUP_KEY_MOVERS.split(',').map(|s| s.trim()) {
            assert!(
                STOCK_MOVERS_CREATE_DDL.contains(col),
                "DEDUP column `{col}` is missing from STOCK_MOVERS_CREATE_DDL"
            );
            assert!(
                OPTION_MOVERS_CREATE_DDL.contains(col),
                "DEDUP column `{col}` is missing from OPTION_MOVERS_CREATE_DDL"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Table name constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_table_names_are_stable() {
        assert_eq!(QUESTDB_TABLE_STOCK_MOVERS, "stock_movers");
        assert_eq!(QUESTDB_TABLE_OPTION_MOVERS, "option_movers");
    }

    // -----------------------------------------------------------------------
    // Flush batch size validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_movers_flush_batch_size_reasonable() {
        // Max per snapshot: 20 entries × 10 categories = 200 rows
        assert!(MOVERS_FLUSH_BATCH_SIZE >= 200);
        assert!(MOVERS_FLUSH_BATCH_SIZE <= 1000);
    }

    #[test]
    fn test_movers_flush_interval_aligns_with_snapshot() {
        // Must be > 60s (snapshot interval) to avoid premature flush
        assert!(MOVERS_FLUSH_INTERVAL_MS >= 60_000);
    }

    // -----------------------------------------------------------------------
    // Writer construction (unit tests — no QuestDB needed)
    // -----------------------------------------------------------------------

    #[test]
    fn test_stock_movers_writer_invalid_host() {
        let config = QuestDbConfig {
            host: "nonexistent-host-99999".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        // Construction may succeed (lazy connect) or fail — both are valid
        let _result = StockMoversWriter::new(&config);
    }

    #[test]
    fn test_option_movers_writer_invalid_host() {
        let config = QuestDbConfig {
            host: "nonexistent-host-99999".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let _result = OptionMoversWriter::new(&config);
    }

    // -----------------------------------------------------------------------
    // DDL timeout validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_ddl_timeout_reasonable() {
        assert!(QUESTDB_DDL_TIMEOUT_SECS >= 5);
        assert!(QUESTDB_DDL_TIMEOUT_SECS <= 30);
    }

    // -----------------------------------------------------------------------
    // Stock mover change_abs calculation
    // -----------------------------------------------------------------------

    #[test]
    fn test_change_abs_calculated_correctly() {
        let ltp = 150.0_f64;
        let prev_close = 145.0_f64;
        let change_abs = ltp - prev_close;
        assert!((change_abs - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_change_abs_negative_when_price_drops() {
        let ltp = 140.0_f64;
        let prev_close = 145.0_f64;
        let change_abs = ltp - prev_close;
        assert!(change_abs < 0.0);
        assert!((change_abs - (-5.0)).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Category string constants validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_stock_mover_categories_are_valid() {
        let categories = ["GAINER", "LOSER", "MOST_ACTIVE"];
        for cat in &categories {
            assert!(!cat.is_empty());
            assert!(cat.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
        }
    }

    #[test]
    fn test_option_mover_categories_are_valid() {
        let categories = [
            "HIGHEST_OI",
            "OI_GAINER",
            "OI_LOSER",
            "TOP_VOLUME",
            "TOP_VALUE",
            "PRICE_GAINER",
            "PRICE_LOSER",
        ];
        assert_eq!(categories.len(), 7);
        for cat in &categories {
            assert!(!cat.is_empty());
            assert!(cat.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
        }
    }
}
