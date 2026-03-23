//! QuestDB persistence for computed Options Greeks, PCR snapshots, and
//! cross-verification results.
//!
//! # Tables
//! - `option_greeks` — per-contract Greeks (IV, Delta, Gamma, Theta, Vega) with security_id
//! - `pcr_snapshots` — per-underlying PCR ratio time series
//! - `greeks_verification` — cross-verification results (our Greeks vs Dhan API)
//!
//! # Dedup Strategy
//! All tables use `(security_id, segment)` as DEDUP UPSERT KEYS to prevent
//! duplicate rows on reconnect/restart. Same pattern as tick_persistence.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// DDL timeout for QuestDB HTTP queries.
const DDL_TIMEOUT_SECS: u64 = 10;

/// QuestDB table name for per-contract Greeks.
const TABLE_OPTION_GREEKS: &str = "option_greeks";

/// QuestDB table name for PCR snapshots.
const TABLE_PCR_SNAPSHOTS: &str = "pcr_snapshots";

/// QuestDB table name for cross-verification results.
const TABLE_GREEKS_VERIFICATION: &str = "greeks_verification";

// ---------------------------------------------------------------------------
// DDL — Table Creation
// ---------------------------------------------------------------------------

/// SQL to create the `option_greeks` table.
///
/// Stores per-contract Greeks computed from live tick data using Black-Scholes.
/// `security_id` is the primary identifier for dedup and cross-referencing
/// with ticks, instrument master, and Dhan API.
const OPTION_GREEKS_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS option_greeks (\
        segment SYMBOL,\
        security_id LONG,\
        symbol_name SYMBOL,\
        underlying_security_id LONG,\
        underlying_symbol SYMBOL,\
        strike_price DOUBLE,\
        option_type SYMBOL,\
        expiry_date SYMBOL,\
        iv DOUBLE,\
        delta DOUBLE,\
        gamma DOUBLE,\
        theta DOUBLE,\
        vega DOUBLE,\
        bs_price DOUBLE,\
        intrinsic_value DOUBLE,\
        extrinsic_value DOUBLE,\
        spot_price DOUBLE,\
        option_ltp DOUBLE,\
        oi LONG,\
        volume LONG,\
        buildup_type SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// SQL to create the `pcr_snapshots` table.
///
/// Stores PCR ratio time series per underlying per expiry.
const PCR_SNAPSHOTS_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS pcr_snapshots (\
        underlying_symbol SYMBOL,\
        expiry_date SYMBOL,\
        pcr_oi DOUBLE,\
        pcr_volume DOUBLE,\
        total_put_oi LONG,\
        total_call_oi LONG,\
        total_put_volume LONG,\
        total_call_volume LONG,\
        sentiment SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// SQL to create the `dhan_option_chain_raw` table.
///
/// Stores raw Dhan Option Chain API response data **exactly as received**.
/// NEVER modify, transform, or recalculate any values — store as-is for audit,
/// cross-verification, and backtesting. One row per contract per snapshot.
const DHAN_OPTION_CHAIN_RAW_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS dhan_option_chain_raw (\
        security_id LONG,\
        segment SYMBOL,\
        symbol_name SYMBOL,\
        underlying_symbol SYMBOL,\
        underlying_security_id LONG,\
        underlying_segment SYMBOL,\
        strike_price DOUBLE,\
        option_type SYMBOL,\
        expiry_date SYMBOL,\
        spot_price DOUBLE,\
        last_price DOUBLE,\
        average_price DOUBLE,\
        oi LONG,\
        previous_close_price DOUBLE,\
        previous_oi LONG,\
        previous_volume LONG,\
        volume LONG,\
        top_bid_price DOUBLE,\
        top_bid_quantity LONG,\
        top_ask_price DOUBLE,\
        top_ask_quantity LONG,\
        implied_volatility DOUBLE,\
        delta DOUBLE,\
        theta DOUBLE,\
        gamma DOUBLE,\
        vega DOUBLE,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// QuestDB table name for raw Dhan option chain snapshots.
const TABLE_DHAN_OPTION_CHAIN_RAW: &str = "dhan_option_chain_raw";

/// SQL to create the `greeks_verification` table.
///
/// Stores cross-verification results comparing our computed Greeks
/// against Dhan Option Chain API values. Used for quality assurance.
const GREEKS_VERIFICATION_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS greeks_verification (\
        security_id LONG,\
        segment SYMBOL,\
        symbol_name SYMBOL,\
        underlying_symbol SYMBOL,\
        strike_price DOUBLE,\
        option_type SYMBOL,\
        our_iv DOUBLE,\
        dhan_iv DOUBLE,\
        iv_diff DOUBLE,\
        our_delta DOUBLE,\
        dhan_delta DOUBLE,\
        delta_diff DOUBLE,\
        our_gamma DOUBLE,\
        dhan_gamma DOUBLE,\
        gamma_diff DOUBLE,\
        our_theta DOUBLE,\
        dhan_theta DOUBLE,\
        theta_diff DOUBLE,\
        our_vega DOUBLE,\
        dhan_vega DOUBLE,\
        vega_diff DOUBLE,\
        match_status SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

// ---------------------------------------------------------------------------
// DEDUP UPSERT KEYS — must be separate ALTER TABLE (QuestDB rejects inline)
// ---------------------------------------------------------------------------

/// DEDUP key for `option_greeks`.
const DEDUP_KEY_OPTION_GREEKS: &str = "security_id, segment";

/// DEDUP key for `pcr_snapshots`.
const DEDUP_KEY_PCR_SNAPSHOTS: &str = "underlying_symbol, expiry_date";

/// DEDUP key for `dhan_option_chain_raw`.
const DEDUP_KEY_DHAN_OPTION_CHAIN_RAW: &str = "security_id, segment";

/// DEDUP key for `greeks_verification`.
const DEDUP_KEY_GREEKS_VERIFICATION: &str = "security_id, segment";

// ---------------------------------------------------------------------------
// Public API — Table Setup
// ---------------------------------------------------------------------------

/// Creates all Greeks-related QuestDB tables (idempotent).
///
/// Best-effort: if QuestDB is unreachable, logs a warning and continues.
// TEST-EXEMPT: requires live QuestDB HTTP endpoint (integration test)
pub async fn ensure_greeks_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "failed to build HTTP client for Greeks DDL");
            return;
        }
    };

    // Step 1: CREATE TABLE IF NOT EXISTS (no inline DEDUP — QuestDB rejects it).
    execute_ddl(
        &client,
        &base_url,
        OPTION_GREEKS_DDL,
        "option_greeks CREATE",
    )
    .await;
    execute_ddl(
        &client,
        &base_url,
        PCR_SNAPSHOTS_DDL,
        "pcr_snapshots CREATE",
    )
    .await;
    execute_ddl(
        &client,
        &base_url,
        DHAN_OPTION_CHAIN_RAW_DDL,
        "dhan_option_chain_raw CREATE",
    )
    .await;
    execute_ddl(
        &client,
        &base_url,
        GREEKS_VERIFICATION_DDL,
        "greeks_verification CREATE",
    )
    .await;

    // Step 2: DEDUP UPSERT KEYS via ALTER TABLE (idempotent — re-enabling is a no-op).
    let dedup_statements: &[(&str, &str, &str)] = &[
        (
            TABLE_OPTION_GREEKS,
            DEDUP_KEY_OPTION_GREEKS,
            "option_greeks DEDUP",
        ),
        (
            TABLE_PCR_SNAPSHOTS,
            DEDUP_KEY_PCR_SNAPSHOTS,
            "pcr_snapshots DEDUP",
        ),
        (
            TABLE_DHAN_OPTION_CHAIN_RAW,
            DEDUP_KEY_DHAN_OPTION_CHAIN_RAW,
            "dhan_option_chain_raw DEDUP",
        ),
        (
            TABLE_GREEKS_VERIFICATION,
            DEDUP_KEY_GREEKS_VERIFICATION,
            "greeks_verification DEDUP",
        ),
    ];
    for (table, dedup_key, label) in dedup_statements {
        let dedup_sql = format!("ALTER TABLE {table} DEDUP ENABLE UPSERT KEYS(ts, {dedup_key})");
        execute_ddl(&client, &base_url, &dedup_sql, label).await;
    }

    info!(
        "Greeks tables setup complete (option_greeks, pcr_snapshots, dhan_option_chain_raw, greeks_verification)"
    );
}

/// Executes a DDL statement against QuestDB HTTP, logging warnings on failure.
async fn execute_ddl(client: &Client, base_url: &str, sql: &str, label: &str) {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                debug!(label, "DDL executed successfully");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    label,
                    body = body.chars().take(200).collect::<String>(),
                    "DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, label, "DDL request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// ILP Writer — Batched writes to QuestDB
// ---------------------------------------------------------------------------

/// Batched writer for all greeks-related QuestDB tables via ILP.
///
/// Cold path — no ring buffer, no reconnect logic. If QuestDB is down,
/// writes fail and are skipped (next cycle will write fresh data).
pub struct GreeksPersistenceWriter {
    sender: Sender,
    buffer: Buffer,
    pending_count: usize,
}

impl GreeksPersistenceWriter {
    /// Creates a new writer connected to QuestDB via ILP TCP.
    // TEST-EXEMPT: requires live QuestDB ILP connection (integration test)
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB ILP for greeks")?;
        let buffer = sender.new_buffer();
        Ok(Self {
            sender,
            buffer,
            pending_count: 0,
        })
    }

    /// Writes a raw Dhan option chain row to `dhan_option_chain_raw`.
    ///
    /// Stores the API response exactly as received — no transformation.
    // TEST-EXEMPT: delegates to build_dhan_raw_row which is tested via DDL schema tests
    pub fn write_dhan_raw_row(&mut self, row: &DhanRawRow) -> Result<()> {
        build_dhan_raw_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Writes a computed Greeks row to `option_greeks`.
    // TEST-EXEMPT: delegates to build_option_greeks_row which is tested via DDL schema tests
    pub fn write_option_greeks_row(&mut self, row: &OptionGreeksRow) -> Result<()> {
        build_option_greeks_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Writes a PCR snapshot row to `pcr_snapshots`.
    // TEST-EXEMPT: delegates to build_pcr_snapshot_row which is tested via DDL schema tests
    pub fn write_pcr_snapshot_row(&mut self, row: &PcrSnapshotRow) -> Result<()> {
        build_pcr_snapshot_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Writes a cross-verification row to `greeks_verification`.
    // TEST-EXEMPT: delegates to build_verification_row which is tested via DDL schema tests
    pub fn write_verification_row(&mut self, row: &VerificationRow) -> Result<()> {
        build_verification_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Flushes all pending rows to QuestDB.
    // TEST-EXEMPT: requires live QuestDB ILP connection (integration test)
    pub fn flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }
        let count = self.pending_count;
        self.sender
            .flush(&mut self.buffer)
            .context("flush greeks data to QuestDB")?;
        self.pending_count = 0;
        debug!(rows = count, "greeks data flushed to QuestDB");
        Ok(())
    }

    /// Returns the number of pending (unflushed) rows.
    // TEST-EXEMPT: trivial accessor, tested indirectly via pipeline cycle logs
    pub fn pending_count(&self) -> usize {
        self.pending_count
    }
}

// ---------------------------------------------------------------------------
// Row Types — Input structs for ILP row builders
// ---------------------------------------------------------------------------

/// Raw Dhan Option Chain API response row (stored as-is).
pub struct DhanRawRow<'a> {
    pub security_id: i64,
    pub segment: &'a str,
    pub symbol_name: &'a str,
    pub underlying_symbol: &'a str,
    pub underlying_security_id: i64,
    pub underlying_segment: &'a str,
    pub strike_price: f64,
    pub option_type: &'a str,
    pub expiry_date: &'a str,
    pub spot_price: f64,
    pub last_price: f64,
    pub average_price: f64,
    pub oi: i64,
    pub previous_close_price: f64,
    pub previous_oi: i64,
    pub previous_volume: i64,
    pub volume: i64,
    pub top_bid_price: f64,
    pub top_bid_quantity: i64,
    pub top_ask_price: f64,
    pub top_ask_quantity: i64,
    pub implied_volatility: f64,
    pub delta: f64,
    pub theta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub ts_nanos: i64,
}

/// Computed Greeks row for `option_greeks` table.
pub struct OptionGreeksRow<'a> {
    pub segment: &'a str,
    pub security_id: i64,
    pub symbol_name: &'a str,
    pub underlying_security_id: i64,
    pub underlying_symbol: &'a str,
    pub strike_price: f64,
    pub option_type: &'a str,
    pub expiry_date: &'a str,
    pub iv: f64,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub bs_price: f64,
    pub intrinsic_value: f64,
    pub extrinsic_value: f64,
    pub spot_price: f64,
    pub option_ltp: f64,
    pub oi: i64,
    pub volume: i64,
    pub buildup_type: &'a str,
    pub ts_nanos: i64,
}

/// PCR snapshot row for `pcr_snapshots` table.
pub struct PcrSnapshotRow<'a> {
    pub underlying_symbol: &'a str,
    pub expiry_date: &'a str,
    pub pcr_oi: f64,
    pub pcr_volume: f64,
    pub total_put_oi: i64,
    pub total_call_oi: i64,
    pub total_put_volume: i64,
    pub total_call_volume: i64,
    pub sentiment: &'a str,
    pub ts_nanos: i64,
}

/// Cross-verification row for `greeks_verification` table.
pub struct VerificationRow<'a> {
    pub security_id: i64,
    pub segment: &'a str,
    pub symbol_name: &'a str,
    pub underlying_symbol: &'a str,
    pub strike_price: f64,
    pub option_type: &'a str,
    pub our_iv: f64,
    pub dhan_iv: f64,
    pub iv_diff: f64,
    pub our_delta: f64,
    pub dhan_delta: f64,
    pub delta_diff: f64,
    pub our_gamma: f64,
    pub dhan_gamma: f64,
    pub gamma_diff: f64,
    pub our_theta: f64,
    pub dhan_theta: f64,
    pub theta_diff: f64,
    pub our_vega: f64,
    pub dhan_vega: f64,
    pub vega_diff: f64,
    pub match_status: &'a str,
    pub ts_nanos: i64,
}

// ---------------------------------------------------------------------------
// ILP Row Builders (extracted for testability)
// ---------------------------------------------------------------------------

/// Writes a single `dhan_option_chain_raw` row into the ILP buffer.
fn build_dhan_raw_row(buffer: &mut Buffer, row: &DhanRawRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_DHAN_OPTION_CHAIN_RAW)
        .context("table")?
        .symbol("segment", row.segment)
        .context("segment")?
        .symbol("symbol_name", row.symbol_name)
        .context("symbol_name")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("underlying_segment", row.underlying_segment)
        .context("underlying_segment")?
        .symbol("option_type", row.option_type)
        .context("option_type")?
        .symbol("expiry_date", row.expiry_date)
        .context("expiry_date")?
        .column_i64("security_id", row.security_id)
        .context("security_id")?
        .column_i64("underlying_security_id", row.underlying_security_id)
        .context("underlying_security_id")?
        .column_f64("strike_price", row.strike_price)
        .context("strike_price")?
        .column_f64("spot_price", row.spot_price)
        .context("spot_price")?
        .column_f64("last_price", row.last_price)
        .context("last_price")?
        .column_f64("average_price", row.average_price)
        .context("average_price")?
        .column_i64("oi", row.oi)
        .context("oi")?
        .column_f64("previous_close_price", row.previous_close_price)
        .context("previous_close_price")?
        .column_i64("previous_oi", row.previous_oi)
        .context("previous_oi")?
        .column_i64("previous_volume", row.previous_volume)
        .context("previous_volume")?
        .column_i64("volume", row.volume)
        .context("volume")?
        .column_f64("top_bid_price", row.top_bid_price)
        .context("top_bid_price")?
        .column_i64("top_bid_quantity", row.top_bid_quantity)
        .context("top_bid_quantity")?
        .column_f64("top_ask_price", row.top_ask_price)
        .context("top_ask_price")?
        .column_i64("top_ask_quantity", row.top_ask_quantity)
        .context("top_ask_quantity")?
        .column_f64("implied_volatility", row.implied_volatility)
        .context("implied_volatility")?
        .column_f64("delta", row.delta)
        .context("delta")?
        .column_f64("theta", row.theta)
        .context("theta")?
        .column_f64("gamma", row.gamma)
        .context("gamma")?
        .column_f64("vega", row.vega)
        .context("vega")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

/// Writes a single `option_greeks` row into the ILP buffer.
fn build_option_greeks_row(buffer: &mut Buffer, row: &OptionGreeksRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_OPTION_GREEKS)
        .context("table")?
        .symbol("segment", row.segment)
        .context("segment")?
        .symbol("symbol_name", row.symbol_name)
        .context("symbol_name")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("option_type", row.option_type)
        .context("option_type")?
        .symbol("expiry_date", row.expiry_date)
        .context("expiry_date")?
        .symbol("buildup_type", row.buildup_type)
        .context("buildup_type")?
        .column_i64("security_id", row.security_id)
        .context("security_id")?
        .column_i64("underlying_security_id", row.underlying_security_id)
        .context("underlying_security_id")?
        .column_f64("strike_price", row.strike_price)
        .context("strike_price")?
        .column_f64("iv", row.iv)
        .context("iv")?
        .column_f64("delta", row.delta)
        .context("delta")?
        .column_f64("gamma", row.gamma)
        .context("gamma")?
        .column_f64("theta", row.theta)
        .context("theta")?
        .column_f64("vega", row.vega)
        .context("vega")?
        .column_f64("bs_price", row.bs_price)
        .context("bs_price")?
        .column_f64("intrinsic_value", row.intrinsic_value)
        .context("intrinsic_value")?
        .column_f64("extrinsic_value", row.extrinsic_value)
        .context("extrinsic_value")?
        .column_f64("spot_price", row.spot_price)
        .context("spot_price")?
        .column_f64("option_ltp", row.option_ltp)
        .context("option_ltp")?
        .column_i64("oi", row.oi)
        .context("oi")?
        .column_i64("volume", row.volume)
        .context("volume")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

/// Writes a single `pcr_snapshots` row into the ILP buffer.
fn build_pcr_snapshot_row(buffer: &mut Buffer, row: &PcrSnapshotRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_PCR_SNAPSHOTS)
        .context("table")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("expiry_date", row.expiry_date)
        .context("expiry_date")?
        .symbol("sentiment", row.sentiment)
        .context("sentiment")?
        .column_f64("pcr_oi", row.pcr_oi)
        .context("pcr_oi")?
        .column_f64("pcr_volume", row.pcr_volume)
        .context("pcr_volume")?
        .column_i64("total_put_oi", row.total_put_oi)
        .context("total_put_oi")?
        .column_i64("total_call_oi", row.total_call_oi)
        .context("total_call_oi")?
        .column_i64("total_put_volume", row.total_put_volume)
        .context("total_put_volume")?
        .column_i64("total_call_volume", row.total_call_volume)
        .context("total_call_volume")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

/// Writes a single `greeks_verification` row into the ILP buffer.
fn build_verification_row(buffer: &mut Buffer, row: &VerificationRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_GREEKS_VERIFICATION)
        .context("table")?
        .symbol("segment", row.segment)
        .context("segment")?
        .symbol("symbol_name", row.symbol_name)
        .context("symbol_name")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("option_type", row.option_type)
        .context("option_type")?
        .symbol("match_status", row.match_status)
        .context("match_status")?
        .column_i64("security_id", row.security_id)
        .context("security_id")?
        .column_f64("strike_price", row.strike_price)
        .context("strike_price")?
        .column_f64("our_iv", row.our_iv)
        .context("our_iv")?
        .column_f64("dhan_iv", row.dhan_iv)
        .context("dhan_iv")?
        .column_f64("iv_diff", row.iv_diff)
        .context("iv_diff")?
        .column_f64("our_delta", row.our_delta)
        .context("our_delta")?
        .column_f64("dhan_delta", row.dhan_delta)
        .context("dhan_delta")?
        .column_f64("delta_diff", row.delta_diff)
        .context("delta_diff")?
        .column_f64("our_gamma", row.our_gamma)
        .context("our_gamma")?
        .column_f64("dhan_gamma", row.dhan_gamma)
        .context("dhan_gamma")?
        .column_f64("gamma_diff", row.gamma_diff)
        .context("gamma_diff")?
        .column_f64("our_theta", row.our_theta)
        .context("our_theta")?
        .column_f64("dhan_theta", row.dhan_theta)
        .context("dhan_theta")?
        .column_f64("theta_diff", row.theta_diff)
        .context("theta_diff")?
        .column_f64("our_vega", row.our_vega)
        .context("our_vega")?
        .column_f64("dhan_vega", row.dhan_vega)
        .context("dhan_vega")?
        .column_f64("vega_diff", row.vega_diff)
        .context("vega_diff")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- DDL SQL validation ---

    #[test]
    fn test_option_greeks_ddl_contains_security_id() {
        assert!(
            OPTION_GREEKS_DDL.contains("security_id LONG"),
            "option_greeks must have security_id for dedup"
        );
    }

    #[test]
    fn test_option_greeks_dedup_key_constant() {
        assert!(
            DEDUP_KEY_OPTION_GREEKS.contains("security_id"),
            "option_greeks dedup must include security_id"
        );
        assert!(
            DEDUP_KEY_OPTION_GREEKS.contains("segment"),
            "option_greeks dedup must include segment"
        );
    }

    #[test]
    fn test_option_greeks_ddl_has_all_greeks_columns() {
        for col in [
            "iv DOUBLE",
            "delta DOUBLE",
            "gamma DOUBLE",
            "theta DOUBLE",
            "vega DOUBLE",
        ] {
            assert!(
                OPTION_GREEKS_DDL.contains(col),
                "option_greeks missing column: {col}"
            );
        }
    }

    #[test]
    fn test_option_greeks_ddl_has_underlying_fields() {
        assert!(OPTION_GREEKS_DDL.contains("underlying_security_id LONG"));
        assert!(OPTION_GREEKS_DDL.contains("underlying_symbol SYMBOL"));
        assert!(OPTION_GREEKS_DDL.contains("strike_price DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("option_type SYMBOL"));
        assert!(OPTION_GREEKS_DDL.contains("expiry_date SYMBOL"));
    }

    #[test]
    fn test_option_greeks_ddl_has_market_data() {
        assert!(OPTION_GREEKS_DDL.contains("spot_price DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("option_ltp DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("oi LONG"));
        assert!(OPTION_GREEKS_DDL.contains("volume LONG"));
        assert!(OPTION_GREEKS_DDL.contains("buildup_type SYMBOL"));
    }

    #[test]
    fn test_option_greeks_ddl_has_bs_values() {
        assert!(OPTION_GREEKS_DDL.contains("bs_price DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("intrinsic_value DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("extrinsic_value DOUBLE"));
    }

    #[test]
    fn test_pcr_snapshots_dedup_key_constant() {
        assert!(
            DEDUP_KEY_PCR_SNAPSHOTS.contains("underlying_symbol"),
            "pcr_snapshots dedup must include underlying_symbol"
        );
        assert!(
            DEDUP_KEY_PCR_SNAPSHOTS.contains("expiry_date"),
            "pcr_snapshots dedup must include expiry_date"
        );
    }

    #[test]
    fn test_pcr_snapshots_ddl_has_all_fields() {
        for col in [
            "pcr_oi DOUBLE",
            "pcr_volume DOUBLE",
            "total_put_oi LONG",
            "total_call_oi LONG",
            "total_put_volume LONG",
            "total_call_volume LONG",
            "sentiment SYMBOL",
        ] {
            assert!(
                PCR_SNAPSHOTS_DDL.contains(col),
                "pcr_snapshots missing: {col}"
            );
        }
    }

    #[test]
    fn test_verification_ddl_has_both_sources() {
        // Must have both our values and Dhan values for comparison.
        for prefix in ["our_", "dhan_"] {
            for greek in ["iv", "delta", "gamma", "theta", "vega"] {
                let col = format!("{prefix}{greek} DOUBLE");
                assert!(
                    GREEKS_VERIFICATION_DDL.contains(&col),
                    "greeks_verification missing: {col}"
                );
            }
        }
    }

    #[test]
    fn test_verification_ddl_has_diff_columns() {
        for greek in [
            "iv_diff",
            "delta_diff",
            "gamma_diff",
            "theta_diff",
            "vega_diff",
        ] {
            let col = format!("{greek} DOUBLE");
            assert!(
                GREEKS_VERIFICATION_DDL.contains(&col),
                "greeks_verification missing diff: {col}"
            );
        }
    }

    #[test]
    fn test_verification_ddl_has_match_status() {
        assert!(GREEKS_VERIFICATION_DDL.contains("match_status SYMBOL"));
    }

    #[test]
    fn test_verification_ddl_has_security_id() {
        assert!(GREEKS_VERIFICATION_DDL.contains("security_id LONG"));
    }

    #[test]
    fn test_verification_dedup_key_constant() {
        assert!(DEDUP_KEY_GREEKS_VERIFICATION.contains("security_id"));
        assert!(DEDUP_KEY_GREEKS_VERIFICATION.contains("segment"));
    }

    #[test]
    fn test_all_tables_have_timestamp() {
        assert!(OPTION_GREEKS_DDL.contains("TIMESTAMP(ts)"));
        assert!(PCR_SNAPSHOTS_DDL.contains("TIMESTAMP(ts)"));
        assert!(GREEKS_VERIFICATION_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_all_tables_have_wal() {
        assert!(OPTION_GREEKS_DDL.contains("WAL"));
        assert!(PCR_SNAPSHOTS_DDL.contains("WAL"));
        assert!(GREEKS_VERIFICATION_DDL.contains("WAL"));
    }

    #[test]
    fn test_all_tables_idempotent() {
        assert!(OPTION_GREEKS_DDL.contains("IF NOT EXISTS"));
        assert!(PCR_SNAPSHOTS_DDL.contains("IF NOT EXISTS"));
        assert!(GREEKS_VERIFICATION_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_option_greeks_ddl_has_symbol_name() {
        assert!(
            OPTION_GREEKS_DDL.contains("symbol_name SYMBOL"),
            "option_greeks must have symbol_name for cross-referencing"
        );
    }

    #[test]
    fn test_verification_ddl_has_symbol_name() {
        assert!(
            GREEKS_VERIFICATION_DDL.contains("symbol_name SYMBOL"),
            "greeks_verification must have symbol_name for cross-referencing"
        );
    }

    // --- Raw Dhan option chain table tests ---

    #[test]
    fn test_dhan_raw_ddl_has_all_dhan_api_fields() {
        // Every field from Dhan Option Chain API response must be stored as-is.
        for col in [
            "security_id LONG",
            "last_price DOUBLE",
            "average_price DOUBLE",
            "oi LONG",
            "previous_close_price DOUBLE",
            "previous_oi LONG",
            "previous_volume LONG",
            "volume LONG",
            "top_bid_price DOUBLE",
            "top_bid_quantity LONG",
            "top_ask_price DOUBLE",
            "top_ask_quantity LONG",
            "implied_volatility DOUBLE",
            "delta DOUBLE",
            "theta DOUBLE",
            "gamma DOUBLE",
            "vega DOUBLE",
        ] {
            assert!(
                DHAN_OPTION_CHAIN_RAW_DDL.contains(col),
                "dhan_option_chain_raw missing Dhan API field: {col}"
            );
        }
    }

    #[test]
    fn test_dhan_raw_ddl_has_identity_fields() {
        for col in [
            "underlying_symbol SYMBOL",
            "underlying_security_id LONG",
            "strike_price DOUBLE",
            "option_type SYMBOL",
            "expiry_date SYMBOL",
            "spot_price DOUBLE",
            "symbol_name SYMBOL",
            "segment SYMBOL",
        ] {
            assert!(
                DHAN_OPTION_CHAIN_RAW_DDL.contains(col),
                "dhan_option_chain_raw missing identity field: {col}"
            );
        }
    }

    #[test]
    fn test_dhan_raw_ddl_partition_and_wal() {
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("PARTITION BY HOUR WAL"));
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("TIMESTAMP(ts)"));
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_dhan_raw_dedup_key_constant() {
        assert!(DEDUP_KEY_DHAN_OPTION_CHAIN_RAW.contains("security_id"));
        assert!(DEDUP_KEY_DHAN_OPTION_CHAIN_RAW.contains("segment"));
    }

    #[test]
    fn test_dhan_raw_table_name() {
        assert_eq!(TABLE_DHAN_OPTION_CHAIN_RAW, "dhan_option_chain_raw");
    }

    #[test]
    fn test_table_names() {
        assert_eq!(TABLE_OPTION_GREEKS, "option_greeks");
        assert_eq!(TABLE_PCR_SNAPSHOTS, "pcr_snapshots");
        assert_eq!(TABLE_GREEKS_VERIFICATION, "greeks_verification");
    }

    // --- Async HTTP tests for ensure_greeks_tables + execute_ddl ---

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_400: &str = "HTTP/1.1 400 Bad Request\r\nContent-Length: 31\r\n\r\n{\"error\":\"table does not exist\"}";

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

    #[tokio::test]
    async fn test_ensure_greeks_tables_unreachable_no_panic() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Must not panic — best-effort DDL.
        ensure_greeks_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_greeks_tables_http_200() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises success path through all 4 DDL calls.
        ensure_greeks_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_greeks_tables_http_400() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises non-success warn path through all 4 DDL calls.
        ensure_greeks_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_greeks_tables_send_error() {
        // Server that immediately drops connection → Err branch in execute_ddl.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream); // Force connection reset
            }
        });
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises Err(err) branch in execute_ddl.
        ensure_greeks_tables(&config).await;
    }

    #[tokio::test]
    async fn test_execute_ddl_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let base_url = format!("http://127.0.0.1:{}/exec", port);
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        execute_ddl(&client, &base_url, "SELECT 1", "test_success").await;
    }

    #[tokio::test]
    async fn test_execute_ddl_non_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let base_url = format!("http://127.0.0.1:{}/exec", port);
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        execute_ddl(&client, &base_url, "INVALID SQL", "test_non_success").await;
    }

    #[tokio::test]
    async fn test_execute_ddl_send_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        tokio::task::yield_now().await;
        let base_url = format!("http://127.0.0.1:{}/exec", port);
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        execute_ddl(&client, &base_url, "SELECT 1", "test_send_error").await;
    }

    // --- ILP row builder tests ---

    fn make_test_buffer() -> Buffer {
        // TCP transport uses V1 protocol by default.
        Buffer::new(questdb::ingress::ProtocolVersion::V1)
    }

    fn sample_ts_nanos() -> i64 {
        1_711_180_800_000_000_000 // 2024-03-23T00:00:00Z in nanos
    }

    #[test]
    fn test_build_dhan_raw_row() {
        let mut buffer = make_test_buffer();
        let row = DhanRawRow {
            security_id: 42528,
            segment: "NSE_FNO",
            symbol_name: "NIFTY25MAR25650CE",
            underlying_symbol: "NIFTY",
            underlying_security_id: 13,
            underlying_segment: "IDX_I",
            strike_price: 25650.0,
            option_type: "CE",
            expiry_date: "2025-03-27",
            spot_price: 25642.8,
            last_price: 134.0,
            average_price: 146.99,
            oi: 3786445,
            previous_close_price: 244.85,
            previous_oi: 402220,
            previous_volume: 31931705,
            volume: 117567970,
            top_bid_price: 133.55,
            top_bid_quantity: 1625,
            top_ask_price: 134.0,
            top_ask_quantity: 1365,
            implied_volatility: 9.789,
            delta: 0.53871,
            theta: -15.1539,
            gamma: 0.00132,
            vega: 12.18593,
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_dhan_raw_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    #[test]
    fn test_build_option_greeks_row() {
        let mut buffer = make_test_buffer();
        let row = OptionGreeksRow {
            segment: "NSE_FNO",
            security_id: 42528,
            symbol_name: "NIFTY25MAR25650CE",
            underlying_security_id: 13,
            underlying_symbol: "NIFTY",
            strike_price: 25650.0,
            option_type: "CE",
            expiry_date: "2025-03-27",
            iv: 0.098,
            delta: 0.54,
            gamma: 0.0013,
            theta: -15.2,
            vega: 12.1,
            bs_price: 135.5,
            intrinsic_value: 0.0,
            extrinsic_value: 134.0,
            spot_price: 25642.8,
            option_ltp: 134.0,
            oi: 3786445,
            volume: 117567970,
            buildup_type: "LongBuildup",
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_option_greeks_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    #[test]
    fn test_build_pcr_snapshot_row() {
        let mut buffer = make_test_buffer();
        let row = PcrSnapshotRow {
            underlying_symbol: "NIFTY",
            expiry_date: "2025-03-27",
            pcr_oi: 0.78,
            pcr_volume: 0.65,
            total_put_oi: 4000000,
            total_call_oi: 5128205,
            total_put_volume: 80000000,
            total_call_volume: 123000000,
            sentiment: "Bullish",
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_pcr_snapshot_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    #[test]
    fn test_build_verification_row() {
        let mut buffer = make_test_buffer();
        let row = VerificationRow {
            security_id: 42528,
            segment: "NSE_FNO",
            symbol_name: "NIFTY25MAR25650CE",
            underlying_symbol: "NIFTY",
            strike_price: 25650.0,
            option_type: "CE",
            our_iv: 0.098,
            dhan_iv: 0.09789,
            iv_diff: 0.00011,
            our_delta: 0.54,
            dhan_delta: 0.53871,
            delta_diff: 0.00129,
            our_gamma: 0.0013,
            dhan_gamma: 0.00132,
            gamma_diff: -0.00002,
            our_theta: -15.2,
            dhan_theta: -15.1539,
            theta_diff: -0.0461,
            our_vega: 12.1,
            dhan_vega: 12.18593,
            vega_diff: -0.08593,
            match_status: "MATCH",
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_verification_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    #[test]
    fn test_multiple_rows_accumulate_in_buffer() {
        let mut buffer = make_test_buffer();
        let row = PcrSnapshotRow {
            underlying_symbol: "NIFTY",
            expiry_date: "2025-03-27",
            pcr_oi: 0.78,
            pcr_volume: 0.65,
            total_put_oi: 4000000,
            total_call_oi: 5128205,
            total_put_volume: 80000000,
            total_call_volume: 123000000,
            sentiment: "Bullish",
            ts_nanos: sample_ts_nanos(),
        };
        build_pcr_snapshot_row(&mut buffer, &row).unwrap();
        let len_after_one = buffer.len();

        let row2 = PcrSnapshotRow {
            underlying_symbol: "BANKNIFTY",
            ts_nanos: sample_ts_nanos() + 1_000_000_000,
            ..row
        };
        build_pcr_snapshot_row(&mut buffer, &row2).unwrap();
        assert!(
            buffer.len() > len_after_one,
            "buffer should grow with each row"
        );
    }
}
