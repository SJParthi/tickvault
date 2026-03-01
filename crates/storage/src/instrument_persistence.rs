//! QuestDB ILP persistence for daily instrument snapshots.
//!
//! After `build_fno_universe()` produces the in-memory universe, this module
//! writes a daily snapshot to QuestDB via the InfluxDB Line Protocol (ILP).
//!
//! Four tables are written:
//! - `instrument_build_metadata` — 1 row per build (health and statistics)
//! - `fno_underlyings` — ~215 rows per day (underlying reference data)
//! - `derivative_contracts` — ~150K rows per day (contract → security_id mapping)
//! - `subscribed_indices` — 31 rows per day (8 F&O + 23 Display indices)
//!
//! # Error Handling
//!
//! QuestDB persistence is observability data, NOT critical path.
//! If writes fail, the system logs a WARN and continues trading.
//!
//! # Idempotency
//!
//! DEDUP UPSERT KEYS are enabled on all 4 tables before each write via
//! `ensure_table_dedup_keys()`. This makes re-runs idempotent: same-day
//! data replaces existing rows instead of creating duplicates.
//!
//! # Query Notes for Downstream Consumers
//!
//! - `expiry_date` is stored as a STRING in `YYYY-MM-DD` format, not a TIMESTAMP.
//! - Futures have `option_type = ''` (empty string, not NULL) and `strike_price = 0.0`.
//! - The designated timestamp (`timestamp` column) is IST midnight stored as UTC.
//!   QuestDB displays it in UTC (e.g., `2026-02-24T18:30:00Z` = `2026-02-25 00:00:00 IST`).
//!   Grafana dashboards should set timezone to IST for correct display.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{FixedOffset, NaiveDate, Utc};
use questdb::ingress::{Buffer, Sender, TimestampMicros, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::{
    ILP_FLUSH_BATCH_SIZE, IST_UTC_OFFSET_SECONDS, QUESTDB_TABLE_BUILD_METADATA,
    QUESTDB_TABLE_DERIVATIVE_CONTRACTS, QUESTDB_TABLE_FNO_UNDERLYINGS,
    QUESTDB_TABLE_SUBSCRIBED_INDICES,
};
use dhan_live_trader_common::instrument_types::{
    DerivativeContract, FnoUnderlying, FnoUniverse, SubscribedIndex, UniverseBuildMetadata,
};
use dhan_live_trader_common::types::SecurityId;

// ---------------------------------------------------------------------------
// Constants — QuestDB DDL
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests (CREATE TABLE / ALTER TABLE).
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY for `instrument_build_metadata` table.
const DEDUP_KEY_BUILD_METADATA: &str = "csv_source";

/// DEDUP UPSERT KEY for `fno_underlyings` table.
const DEDUP_KEY_FNO_UNDERLYINGS: &str = "underlying_symbol";

/// DEDUP UPSERT KEY for `derivative_contracts` table.
const DEDUP_KEY_DERIVATIVE_CONTRACTS: &str = "security_id";

/// DEDUP UPSERT KEY for `subscribed_indices` table.
const DEDUP_KEY_SUBSCRIBED_INDICES: &str = "security_id";

// ---------------------------------------------------------------------------
// CREATE TABLE DDL — Explicit schemas for all 4 instrument tables
// ---------------------------------------------------------------------------

/// DDL for `instrument_build_metadata` — 1 row per daily build.
const BUILD_METADATA_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS instrument_build_metadata (\
        csv_source SYMBOL,\
        csv_row_count LONG,\
        parsed_row_count LONG,\
        index_count LONG,\
        equity_count LONG,\
        underlying_count LONG,\
        derivative_count LONG,\
        option_chain_count LONG,\
        build_duration_ms LONG,\
        build_timestamp TIMESTAMP,\
        timestamp TIMESTAMP\
    ) TIMESTAMP(timestamp) PARTITION BY DAY WAL\
";

/// DDL for `fno_underlyings` — ~215 rows per day.
const FNO_UNDERLYINGS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS fno_underlyings (\
        underlying_symbol SYMBOL,\
        price_feed_segment SYMBOL,\
        derivative_segment SYMBOL,\
        kind SYMBOL,\
        underlying_security_id LONG,\
        price_feed_security_id LONG,\
        lot_size LONG,\
        contract_count LONG,\
        timestamp TIMESTAMP\
    ) TIMESTAMP(timestamp) PARTITION BY DAY WAL\
";

/// DDL for `derivative_contracts` — ~150K rows per day.
const DERIVATIVE_CONTRACTS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS derivative_contracts (\
        underlying_symbol SYMBOL,\
        instrument_kind SYMBOL,\
        exchange_segment SYMBOL,\
        option_type SYMBOL,\
        symbol_name SYMBOL,\
        security_id LONG,\
        expiry_date STRING,\
        strike_price DOUBLE,\
        lot_size LONG,\
        tick_size DOUBLE,\
        display_name STRING,\
        timestamp TIMESTAMP\
    ) TIMESTAMP(timestamp) PARTITION BY DAY WAL\
";

/// DDL for `subscribed_indices` — 31 rows per day.
const SUBSCRIBED_INDICES_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS subscribed_indices (\
        symbol SYMBOL,\
        exchange SYMBOL,\
        category SYMBOL,\
        subcategory SYMBOL,\
        security_id LONG,\
        timestamp TIMESTAMP\
    ) TIMESTAMP(timestamp) PARTITION BY DAY WAL\
";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Persists a daily instrument snapshot to QuestDB via ILP.
///
/// Writes build metadata, F&O underlyings, and all derivative contracts.
/// On failure, logs a warning and returns `Ok(())` — trading is not blocked.
///
/// Before writing, enables DEDUP UPSERT KEYS on all 4 tables via QuestDB HTTP
/// to ensure idempotent re-runs (same-day data replaces, not duplicates).
pub async fn persist_instrument_snapshot(
    universe: &FnoUniverse,
    questdb_config: &QuestDbConfig,
) -> Result<()> {
    match persist_inner(universe, questdb_config).await {
        Ok(()) => {
            info!(
                underlying_count = universe.underlyings.len(),
                derivative_count = universe.derivative_contracts.len(),
                subscribed_index_count = universe.subscribed_indices.len(),
                "instrument snapshot persisted to QuestDB"
            );
            Ok(())
        }
        Err(err) => {
            warn!(
                ?err,
                "QuestDB instrument persistence failed — trading continues"
            );
            Ok(())
        }
    }
}

/// Creates all 4 instrument tables (if not exist) and enables DEDUP UPSERT KEYS.
///
/// Called once at startup from `main.rs` — same pattern as `ensure_tick_table_dedup_keys`.
/// Best-effort: if QuestDB is unreachable, logs warnings and continues.
pub async fn ensure_instrument_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "failed to build HTTP client for instrument table DDL");
            return;
        }
    };

    // Step 1: Create all 4 tables with explicit schemas.
    let table_ddls: &[(&str, &str)] = &[
        (QUESTDB_TABLE_BUILD_METADATA, BUILD_METADATA_CREATE_DDL),
        (QUESTDB_TABLE_FNO_UNDERLYINGS, FNO_UNDERLYINGS_CREATE_DDL),
        (
            QUESTDB_TABLE_DERIVATIVE_CONTRACTS,
            DERIVATIVE_CONTRACTS_CREATE_DDL,
        ),
        (
            QUESTDB_TABLE_SUBSCRIBED_INDICES,
            SUBSCRIBED_INDICES_CREATE_DDL,
        ),
    ];

    for (table_name, ddl) in table_ddls {
        match client.get(&base_url).query(&[("query", ddl)]).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    debug!(
                        table = *table_name,
                        "table ensured (CREATE TABLE IF NOT EXISTS)"
                    );
                } else {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    warn!(
                        table = *table_name,
                        %status,
                        body = body.chars().take(200).collect::<String>(),
                        "CREATE TABLE DDL returned non-success"
                    );
                }
            }
            Err(err) => {
                warn!(table = *table_name, ?err, "CREATE TABLE DDL request failed");
            }
        }
    }

    // Step 2: Enable DEDUP UPSERT KEYS on all 4 tables.
    let dedup_statements: &[(&str, &str)] = &[
        (QUESTDB_TABLE_BUILD_METADATA, DEDUP_KEY_BUILD_METADATA),
        (QUESTDB_TABLE_FNO_UNDERLYINGS, DEDUP_KEY_FNO_UNDERLYINGS),
        (
            QUESTDB_TABLE_DERIVATIVE_CONTRACTS,
            DEDUP_KEY_DERIVATIVE_CONTRACTS,
        ),
        (
            QUESTDB_TABLE_SUBSCRIBED_INDICES,
            DEDUP_KEY_SUBSCRIBED_INDICES,
        ),
    ];

    for (table, key) in dedup_statements {
        let sql = format!("ALTER TABLE {table} DEDUP ENABLE UPSERT KEYS(timestamp, {key})");
        match client.get(&base_url).query(&[("query", &sql)]).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    debug!(table, key, "DEDUP UPSERT KEY enabled");
                } else {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    warn!(
                        table,
                        key,
                        %status,
                        body = body.chars().take(200).collect::<String>(),
                        "DEDUP DDL returned non-success"
                    );
                }
            }
            Err(err) => {
                warn!(table, key, ?err, "DEDUP DDL request failed");
            }
        }
    }

    info!("instrument tables setup complete (DDL + DEDUP UPSERT KEYS)");
}

// ---------------------------------------------------------------------------
// Internal Implementation
// ---------------------------------------------------------------------------

/// Inner persistence logic that propagates errors for the outer wrapper to catch.
async fn persist_inner(universe: &FnoUniverse, questdb_config: &QuestDbConfig) -> Result<()> {
    // Enable DEDUP UPSERT KEYS before writing — safety net (startup already did this).
    ensure_table_dedup_keys(questdb_config).await;

    let conf_string = format!(
        "tcp::addr={}:{};",
        questdb_config.host, questdb_config.ilp_port
    );
    let mut sender =
        Sender::from_conf(&conf_string).context("failed to connect to QuestDB via ILP")?;
    let mut buffer = sender.new_buffer();

    let snapshot_nanos = build_snapshot_timestamp()?;

    write_build_metadata(
        &mut sender,
        &mut buffer,
        &universe.build_metadata,
        snapshot_nanos,
    )?;

    write_underlyings(
        &mut sender,
        &mut buffer,
        &universe.underlyings,
        snapshot_nanos,
    )?;

    write_derivative_contracts(
        &mut sender,
        &mut buffer,
        &universe.derivative_contracts,
        snapshot_nanos,
    )?;

    write_subscribed_indices(
        &mut sender,
        &mut buffer,
        &universe.subscribed_indices,
        snapshot_nanos,
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// QuestDB DEDUP UPSERT KEYS — Idempotency Setup
// ---------------------------------------------------------------------------

/// Enables DEDUP UPSERT KEYS on all 4 instrument tables via QuestDB HTTP `/exec`.
///
/// QuestDB WAL tables support deduplication: rows with the same designated
/// timestamp AND the same UPSERT KEY column value are deduplicated (last write
/// wins). This makes `persist_instrument_snapshot()` idempotent — running it
/// twice on the same day replaces rows instead of doubling them.
///
/// This function is best-effort: if QuestDB is unreachable or tables don't
/// exist yet, it logs a warning and continues. The ILP write that follows
/// will auto-create tables (without dedup), and dedup will be enabled on
/// the next successful run.
async fn ensure_table_dedup_keys(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(
                ?err,
                "failed to build HTTP client for QuestDB DDL — dedup not enabled"
            );
            return;
        }
    };

    let dedup_statements: &[(&str, &str)] = &[
        (QUESTDB_TABLE_BUILD_METADATA, DEDUP_KEY_BUILD_METADATA),
        (QUESTDB_TABLE_FNO_UNDERLYINGS, DEDUP_KEY_FNO_UNDERLYINGS),
        (
            QUESTDB_TABLE_DERIVATIVE_CONTRACTS,
            DEDUP_KEY_DERIVATIVE_CONTRACTS,
        ),
        (
            QUESTDB_TABLE_SUBSCRIBED_INDICES,
            DEDUP_KEY_SUBSCRIBED_INDICES,
        ),
    ];

    for (table, key) in dedup_statements {
        let sql = format!("ALTER TABLE {table} DEDUP ENABLE UPSERT KEYS(timestamp, {key})");
        match client.get(&base_url).query(&[("query", &sql)]).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    debug!(table, key, "DEDUP UPSERT KEY enabled");
                } else {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    warn!(
                        table,
                        key,
                        %status,
                        body = body.chars().take(200).collect::<String>(),
                        "QuestDB DEDUP DDL returned non-success — table may not exist yet"
                    );
                }
            }
            Err(err) => {
                warn!(
                    table,
                    key,
                    ?err,
                    "QuestDB DEDUP DDL request failed — dedup not enabled for this table"
                );
            }
        }
    }

    info!("DEDUP UPSERT KEYS ensured for all instrument tables");
}

/// Builds the designated timestamp for the snapshot: today's IST date at midnight.
///
/// All rows for a single day share the same designated timestamp, making
/// date-based queries clean (e.g., `WHERE snapshot_date = '2026-03-15'`).
fn build_snapshot_timestamp() -> Result<TimestampNanos> {
    let ist =
        FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).context("invalid IST offset seconds")?;
    let today_ist = Utc::now().with_timezone(&ist).date_naive();
    naive_date_to_timestamp_nanos(today_ist)
}

/// Converts a `NaiveDate` to `TimestampNanos` at midnight IST.
fn naive_date_to_timestamp_nanos(date: NaiveDate) -> Result<TimestampNanos> {
    let ist =
        FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).context("invalid IST offset seconds")?;
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .context("failed to construct midnight time")?;
    let ist_datetime = midnight
        .and_local_timezone(ist)
        .single()
        .context("ambiguous or invalid IST timezone conversion")?;
    TimestampNanos::from_datetime(ist_datetime).context("failed to convert IST date to ILP nanos")
}

// ---------------------------------------------------------------------------
// Table 1: instrument_build_metadata (1 row)
// ---------------------------------------------------------------------------

/// Writes a single row of build metadata.
fn write_build_metadata(
    sender: &mut Sender,
    buffer: &mut Buffer,
    metadata: &UniverseBuildMetadata,
    snapshot_nanos: TimestampNanos,
) -> Result<()> {
    let build_ts_micros = TimestampMicros::from_datetime(metadata.build_timestamp);

    buffer
        .table(QUESTDB_TABLE_BUILD_METADATA)
        .context("table name")?
        .symbol("csv_source", &metadata.csv_source)
        .context("csv_source")?
        .column_i64(
            "csv_row_count",
            i64::try_from(metadata.csv_row_count).context("csv_row_count overflows i64")?,
        )
        .context("csv_row_count")?
        .column_i64(
            "parsed_row_count",
            i64::try_from(metadata.parsed_row_count).context("parsed_row_count overflows i64")?,
        )
        .context("parsed_row_count")?
        .column_i64(
            "index_count",
            i64::try_from(metadata.index_count).context("index_count overflows i64")?,
        )
        .context("index_count")?
        .column_i64(
            "equity_count",
            i64::try_from(metadata.equity_count).context("equity_count overflows i64")?,
        )
        .context("equity_count")?
        .column_i64(
            "underlying_count",
            i64::try_from(metadata.underlying_count).context("underlying_count overflows i64")?,
        )
        .context("underlying_count")?
        .column_i64(
            "derivative_count",
            i64::try_from(metadata.derivative_count).context("derivative_count overflows i64")?,
        )
        .context("derivative_count")?
        .column_i64(
            "option_chain_count",
            i64::try_from(metadata.option_chain_count)
                .context("option_chain_count overflows i64")?,
        )
        .context("option_chain_count")?
        .column_i64(
            "build_duration_ms",
            i64::try_from(metadata.build_duration.as_millis())
                .context("build_duration_ms overflows i64")?,
        )
        .context("build_duration_ms")?
        .column_ts("build_timestamp", build_ts_micros)
        .context("build_timestamp")?
        .at(snapshot_nanos)
        .context("designated timestamp")?;

    sender
        .flush(buffer)
        .context("flush instrument_build_metadata")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Table 2: fno_underlyings (~215 rows)
// ---------------------------------------------------------------------------

/// Writes all F&O underlyings as a daily snapshot.
fn write_underlyings(
    sender: &mut Sender,
    buffer: &mut Buffer,
    underlyings: &std::collections::HashMap<String, FnoUnderlying>,
    snapshot_nanos: TimestampNanos,
) -> Result<()> {
    for underlying in underlyings.values() {
        write_single_underlying(buffer, underlying, snapshot_nanos)?;
    }

    sender.flush(buffer).context("flush fno_underlyings")?;

    Ok(())
}

/// Writes a single underlying row into the ILP buffer.
fn write_single_underlying(
    buffer: &mut Buffer,
    underlying: &FnoUnderlying,
    snapshot_nanos: TimestampNanos,
) -> Result<()> {
    buffer
        .table(QUESTDB_TABLE_FNO_UNDERLYINGS)
        .context("table name")?
        .symbol("underlying_symbol", &underlying.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("price_feed_segment", underlying.price_feed_segment.as_str())
        .context("price_feed_segment")?
        .symbol("derivative_segment", underlying.derivative_segment.as_str())
        .context("derivative_segment")?
        .symbol("kind", underlying.kind.as_str())
        .context("kind")?
        .column_i64(
            "underlying_security_id",
            i64::from(underlying.underlying_security_id),
        )
        .context("underlying_security_id")?
        .column_i64(
            "price_feed_security_id",
            i64::from(underlying.price_feed_security_id),
        )
        .context("price_feed_security_id")?
        .column_i64("lot_size", i64::from(underlying.lot_size))
        .context("lot_size")?
        .column_i64(
            "contract_count",
            i64::try_from(underlying.contract_count).context("contract_count overflows i64")?,
        )
        .context("contract_count")?
        .at(snapshot_nanos)
        .context("designated timestamp")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Table 3: derivative_contracts (~150K rows, batched)
// ---------------------------------------------------------------------------

/// Writes all derivative contracts as a daily snapshot, flushing in batches.
fn write_derivative_contracts(
    sender: &mut Sender,
    buffer: &mut Buffer,
    contracts: &std::collections::HashMap<SecurityId, DerivativeContract>,
    snapshot_nanos: TimestampNanos,
) -> Result<()> {
    for (index, contract) in contracts.values().enumerate() {
        write_single_contract(buffer, contract, snapshot_nanos)?;

        // Flush in batches to prevent unbounded memory growth.
        if (index + 1) % ILP_FLUSH_BATCH_SIZE == 0 {
            sender
                .flush(buffer)
                .context("flush derivative_contracts batch")?;
        }
    }

    // Flush remaining rows.
    if !buffer.is_empty() {
        sender
            .flush(buffer)
            .context("flush derivative_contracts final")?;
    }

    Ok(())
}

/// Writes a single derivative contract row into the ILP buffer.
fn write_single_contract(
    buffer: &mut Buffer,
    contract: &DerivativeContract,
    snapshot_nanos: TimestampNanos,
) -> Result<()> {
    let option_type_str = contract
        .option_type
        .as_ref()
        .map(|ot| ot.as_str())
        .unwrap_or("");

    // Expiry date stored as STRING "YYYY-MM-DD" — it's a calendar date, not a timestamp.
    // NaiveDate::to_string() produces "YYYY-MM-DD" format.
    let expiry_date_str = contract.expiry_date.to_string();

    buffer
        .table(QUESTDB_TABLE_DERIVATIVE_CONTRACTS)
        .context("table name")?
        .symbol("underlying_symbol", &contract.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("instrument_kind", contract.instrument_kind.as_str())
        .context("instrument_kind")?
        .symbol("exchange_segment", contract.exchange_segment.as_str())
        .context("exchange_segment")?
        .symbol("option_type", option_type_str)
        .context("option_type")?
        .symbol("symbol_name", &contract.symbol_name)
        .context("symbol_name")?
        .column_i64("security_id", i64::from(contract.security_id))
        .context("security_id")?
        .column_str("expiry_date", &expiry_date_str)
        .context("expiry_date")?
        .column_f64("strike_price", contract.strike_price)
        .context("strike_price")?
        .column_i64("lot_size", i64::from(contract.lot_size))
        .context("lot_size")?
        .column_f64("tick_size", contract.tick_size)
        .context("tick_size")?
        .column_str("display_name", &contract.display_name)
        .context("display_name")?
        .at(snapshot_nanos)
        .context("designated timestamp")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Table 4: subscribed_indices (31 rows: 8 F&O + 23 Display)
// ---------------------------------------------------------------------------

/// Writes all subscribed indices as a daily snapshot.
fn write_subscribed_indices(
    sender: &mut Sender,
    buffer: &mut Buffer,
    indices: &[SubscribedIndex],
    snapshot_nanos: TimestampNanos,
) -> Result<()> {
    for index in indices {
        write_single_subscribed_index(buffer, index, snapshot_nanos)?;
    }

    if !buffer.is_empty() {
        sender.flush(buffer).context("flush subscribed_indices")?;
    }

    Ok(())
}

/// Writes a single subscribed index row into the ILP buffer.
fn write_single_subscribed_index(
    buffer: &mut Buffer,
    index: &SubscribedIndex,
    snapshot_nanos: TimestampNanos,
) -> Result<()> {
    buffer
        .table(QUESTDB_TABLE_SUBSCRIBED_INDICES)
        .context("table name")?
        .symbol("symbol", &index.symbol)
        .context("symbol")?
        .symbol("exchange", index.exchange.as_str())
        .context("exchange")?
        .symbol("category", index.category.as_str())
        .context("category")?
        .symbol("subcategory", index.subcategory.as_str())
        .context("subcategory")?
        .column_i64("security_id", i64::from(index.security_id))
        .context("security_id")?
        .at(snapshot_nanos)
        .context("designated timestamp")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};
    use dhan_live_trader_common::instrument_types::{
        DhanInstrumentKind, IndexCategory, IndexSubcategory, UnderlyingKind, UniverseBuildMetadata,
    };
    use dhan_live_trader_common::types::{Exchange, ExchangeSegment, OptionType};
    use questdb::ingress::ProtocolVersion;
    use std::time::Duration;

    /// Helper: create a minimal FnoUnderlying for testing.
    fn make_test_underlying(symbol: &str, security_id: SecurityId) -> FnoUnderlying {
        FnoUnderlying {
            underlying_symbol: symbol.to_string(),
            underlying_security_id: security_id,
            price_feed_security_id: 13,
            price_feed_segment: ExchangeSegment::IdxI,
            derivative_segment: ExchangeSegment::NseFno,
            kind: UnderlyingKind::NseIndex,
            lot_size: 75,
            contract_count: 4031,
        }
    }

    /// Helper: create a minimal DerivativeContract for testing.
    fn make_test_contract(security_id: SecurityId) -> DerivativeContract {
        DerivativeContract {
            security_id,
            underlying_symbol: "NIFTY".to_string(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 18000.0,
            option_type: Some(OptionType::Call),
            lot_size: 75,
            tick_size: 0.05,
            symbol_name: "NIFTY-Mar2026-18000-CE".to_string(),
            display_name: "NIFTY 27 Mar 18000 CE".to_string(),
        }
    }

    /// Helper: create a minimal UniverseBuildMetadata for testing.
    fn make_test_metadata() -> UniverseBuildMetadata {
        let ist =
            FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
        UniverseBuildMetadata {
            csv_source: "primary".to_string(),
            csv_row_count: 276_018,
            parsed_row_count: 160_245,
            index_count: 194,
            equity_count: 2_442,
            underlying_count: 215,
            derivative_count: 150_949,
            option_chain_count: 1_288,
            build_duration: Duration::from_millis(3200),
            build_timestamp: Utc::now().with_timezone(&ist),
        }
    }

    #[test]
    fn test_build_metadata_buffer_has_correct_row_count() {
        let metadata = make_test_metadata();
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        // Create buffer directly (no sender needed for buffer-only tests).
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let build_ts_micros = TimestampMicros::from_datetime(metadata.build_timestamp);

        buffer
            .table(QUESTDB_TABLE_BUILD_METADATA)
            .unwrap()
            .symbol("csv_source", &metadata.csv_source)
            .unwrap()
            .column_i64(
                "csv_row_count",
                i64::try_from(metadata.csv_row_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "parsed_row_count",
                i64::try_from(metadata.parsed_row_count).unwrap(),
            )
            .unwrap()
            .column_i64("index_count", i64::try_from(metadata.index_count).unwrap())
            .unwrap()
            .column_i64(
                "equity_count",
                i64::try_from(metadata.equity_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "underlying_count",
                i64::try_from(metadata.underlying_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "derivative_count",
                i64::try_from(metadata.derivative_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "option_chain_count",
                i64::try_from(metadata.option_chain_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "build_duration_ms",
                i64::try_from(metadata.build_duration.as_millis()).unwrap(),
            )
            .unwrap()
            .column_ts("build_timestamp", build_ts_micros)
            .unwrap()
            .at(snapshot_nanos)
            .unwrap();

        assert_eq!(buffer.row_count(), 1);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_underlyings_buffer_has_correct_row_count() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let underlyings = vec![
            make_test_underlying("NIFTY", 26000),
            make_test_underlying("BANKNIFTY", 26009),
            make_test_underlying("RELIANCE", 2885),
        ];

        for underlying in &underlyings {
            write_single_underlying(&mut buffer, underlying, snapshot_nanos).unwrap();
        }

        assert_eq!(buffer.row_count(), 3);
    }

    #[test]
    fn test_contract_buffer_has_correct_row_count() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        for security_id in 1000..1010_u32 {
            let contract = make_test_contract(security_id);
            write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        }

        assert_eq!(buffer.row_count(), 10);
    }

    #[test]
    fn test_futures_contract_with_no_option_type() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let mut contract = make_test_contract(5000);
        contract.instrument_kind = DhanInstrumentKind::FutureIndex;
        contract.option_type = None;
        contract.strike_price = 0.0;
        contract.symbol_name = "NIFTY-Mar2026-FUT".to_string();
        contract.display_name = "NIFTY 27 Mar FUT".to_string();

        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_produces_valid_value() {
        let date = NaiveDate::from_ymd_opt(2026, 3, 15).expect("valid date");
        let ts = naive_date_to_timestamp_nanos(date).expect("valid timestamp");
        // March 15, 2026 00:00:00 IST = March 14, 2026 18:30:00 UTC
        // Verify it's a positive number in the right ballpark (after 2020).
        assert!(ts.as_i64() > 1_577_836_800_000_000_000); // > 2020-01-01
    }

    #[test]
    fn test_buffer_content_contains_table_name() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let underlying = make_test_underlying("NIFTY", 26000);
        write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_FNO_UNDERLYINGS));
        assert!(content.contains("NIFTY"));
        assert!(content.contains("IDX_I"));
        assert!(content.contains("NseIndex"));
    }

    #[test]
    fn test_buffer_content_contract_contains_fields() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let contract = make_test_contract(12345);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_DERIVATIVE_CONTRACTS));
        assert!(content.contains("NIFTY"));
        assert!(content.contains("OptionIndex"));
        assert!(content.contains("NSE_FNO"));
        assert!(content.contains("CE"));
        // Expiry date must be stored as YYYY-MM-DD string, never as a timestamp.
        assert!(content.contains("2026-03-27"));
        assert!(!content.contains("T18:30:00"));
    }

    #[test]
    fn test_exchange_segment_as_str() {
        assert_eq!(ExchangeSegment::IdxI.as_str(), "IDX_I");
        assert_eq!(ExchangeSegment::NseEquity.as_str(), "NSE_EQ");
        assert_eq!(ExchangeSegment::NseFno.as_str(), "NSE_FNO");
        assert_eq!(ExchangeSegment::BseEquity.as_str(), "BSE_EQ");
        assert_eq!(ExchangeSegment::BseFno.as_str(), "BSE_FNO");
        assert_eq!(ExchangeSegment::McxComm.as_str(), "MCX_COMM");
    }

    #[test]
    fn test_underlying_kind_as_str() {
        assert_eq!(UnderlyingKind::NseIndex.as_str(), "NseIndex");
        assert_eq!(UnderlyingKind::BseIndex.as_str(), "BseIndex");
        assert_eq!(UnderlyingKind::Stock.as_str(), "Stock");
    }

    #[test]
    fn test_dhan_instrument_kind_as_str() {
        assert_eq!(DhanInstrumentKind::FutureIndex.as_str(), "FutureIndex");
        assert_eq!(DhanInstrumentKind::FutureStock.as_str(), "FutureStock");
        assert_eq!(DhanInstrumentKind::OptionIndex.as_str(), "OptionIndex");
        assert_eq!(DhanInstrumentKind::OptionStock.as_str(), "OptionStock");
    }

    #[test]
    fn test_option_type_as_str() {
        assert_eq!(OptionType::Call.as_str(), "CE");
        assert_eq!(OptionType::Put.as_str(), "PE");
    }

    // --- Subscribed Indices Tests ---

    /// Helper: create a minimal SubscribedIndex for testing.
    fn make_test_fno_index(symbol: &str, security_id: SecurityId) -> SubscribedIndex {
        SubscribedIndex {
            symbol: symbol.to_string(),
            security_id,
            exchange: Exchange::NationalStockExchange,
            category: IndexCategory::FnoUnderlying,
            subcategory: IndexSubcategory::Fno,
        }
    }

    /// Helper: create a display index for testing.
    fn make_test_display_index(
        symbol: &str,
        security_id: SecurityId,
        subcategory: IndexSubcategory,
    ) -> SubscribedIndex {
        SubscribedIndex {
            symbol: symbol.to_string(),
            security_id,
            exchange: Exchange::NationalStockExchange,
            category: IndexCategory::DisplayIndex,
            subcategory,
        }
    }

    #[test]
    fn test_subscribed_indices_buffer_has_correct_row_count() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let indices = vec![
            make_test_fno_index("NIFTY", 13),
            make_test_fno_index("BANKNIFTY", 25),
            make_test_display_index("INDIA VIX", 21, IndexSubcategory::Volatility),
            make_test_display_index("NIFTY AUTO", 14, IndexSubcategory::Sectoral),
        ];

        for index in &indices {
            write_single_subscribed_index(&mut buffer, index, snapshot_nanos).unwrap();
        }

        assert_eq!(buffer.row_count(), 4);
    }

    #[test]
    fn test_subscribed_index_buffer_contains_fields() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let index = make_test_fno_index("NIFTY", 13);
        write_single_subscribed_index(&mut buffer, &index, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_SUBSCRIBED_INDICES));
        assert!(content.contains("NIFTY"));
        assert!(content.contains("NSE"));
        assert!(content.contains("FnoUnderlying"));
        assert!(content.contains("Fno"));
    }

    #[test]
    fn test_display_index_buffer_contains_subcategory() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let index = make_test_display_index("INDIA VIX", 21, IndexSubcategory::Volatility);
        write_single_subscribed_index(&mut buffer, &index, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("DisplayIndex"));
        assert!(content.contains("Volatility"));
        // ILP protocol escapes spaces in SYMBOL columns as "\ " (backslash-space).
        assert!(content.contains("INDIA\\ VIX"));
    }

    #[test]
    fn test_index_category_as_str() {
        assert_eq!(IndexCategory::FnoUnderlying.as_str(), "FnoUnderlying");
        assert_eq!(IndexCategory::DisplayIndex.as_str(), "DisplayIndex");
    }

    #[test]
    fn test_index_subcategory_as_str() {
        assert_eq!(IndexSubcategory::Volatility.as_str(), "Volatility");
        assert_eq!(IndexSubcategory::BroadMarket.as_str(), "BroadMarket");
        assert_eq!(IndexSubcategory::MidCap.as_str(), "MidCap");
        assert_eq!(IndexSubcategory::SmallCap.as_str(), "SmallCap");
        assert_eq!(IndexSubcategory::Sectoral.as_str(), "Sectoral");
        assert_eq!(IndexSubcategory::Thematic.as_str(), "Thematic");
        assert_eq!(IndexSubcategory::Fno.as_str(), "Fno");
    }

    #[test]
    fn test_exchange_as_str() {
        assert_eq!(Exchange::NationalStockExchange.as_str(), "NSE");
        assert_eq!(Exchange::BombayStockExchange.as_str(), "BSE");
    }

    // --- DEDUP UPSERT KEYS Tests ---

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_does_not_panic_with_unreachable_host() {
        // An unreachable host should gracefully log warnings, never panic.
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(), // RFC 5737 TEST-NET, guaranteed unreachable
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        // This must complete without panic — failure is logged, not propagated.
        ensure_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_does_not_panic_with_localhost() {
        // Localhost may or may not have QuestDB running — either way, no panic.
        let config = QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 19999, // unlikely to be in use
            pg_port: 8812,
            ilp_port: 9009,
        };
        ensure_table_dedup_keys(&config).await;
    }

    // --- Gap-fill tests ---

    #[test]
    fn test_build_metadata_buffer_contains_expected_fields() {
        let metadata = make_test_metadata();
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let build_ts_micros = TimestampMicros::from_datetime(metadata.build_timestamp);

        buffer
            .table(QUESTDB_TABLE_BUILD_METADATA)
            .unwrap()
            .symbol("csv_source", &metadata.csv_source)
            .unwrap()
            .column_i64(
                "csv_row_count",
                i64::try_from(metadata.csv_row_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "parsed_row_count",
                i64::try_from(metadata.parsed_row_count).unwrap(),
            )
            .unwrap()
            .column_i64("index_count", i64::try_from(metadata.index_count).unwrap())
            .unwrap()
            .column_i64(
                "equity_count",
                i64::try_from(metadata.equity_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "underlying_count",
                i64::try_from(metadata.underlying_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "derivative_count",
                i64::try_from(metadata.derivative_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "option_chain_count",
                i64::try_from(metadata.option_chain_count).unwrap(),
            )
            .unwrap()
            .column_i64(
                "build_duration_ms",
                i64::try_from(metadata.build_duration.as_millis()).unwrap(),
            )
            .unwrap()
            .column_ts("build_timestamp", build_ts_micros)
            .unwrap()
            .at(snapshot_nanos)
            .unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(
            content.contains(QUESTDB_TABLE_BUILD_METADATA),
            "buffer must contain table name 'instrument_build_metadata'"
        );
        assert!(
            content.contains("csv_row_count"),
            "buffer must contain csv_row_count field"
        );
        assert!(
            content.contains("parsed_row_count"),
            "buffer must contain parsed_row_count field"
        );
        assert!(
            content.contains("index_count"),
            "buffer must contain index_count field"
        );
        assert!(
            content.contains("equity_count"),
            "buffer must contain equity_count field"
        );
        assert!(
            content.contains("underlying_count"),
            "buffer must contain underlying_count field"
        );
        assert!(
            content.contains("derivative_count"),
            "buffer must contain derivative_count field"
        );
        assert!(
            content.contains("option_chain_count"),
            "buffer must contain option_chain_count field"
        );
        assert!(
            content.contains("build_duration_ms"),
            "buffer must contain build_duration_ms field"
        );
        assert!(
            content.contains("build_timestamp"),
            "buffer must contain build_timestamp field"
        );
        assert!(
            content.contains("csv_source"),
            "buffer must contain csv_source symbol"
        );
        assert!(
            content.contains("primary"),
            "buffer must contain csv_source value 'primary'"
        );
    }

    #[test]
    fn test_underlying_buffer_with_empty_input() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let empty_underlyings: std::collections::HashMap<String, FnoUnderlying> =
            std::collections::HashMap::new();

        // Replicate what write_underlyings does: iterate and call write_single_underlying.
        // With empty input, no rows are written.
        for underlying in empty_underlyings.values() {
            write_single_underlying(&mut buffer, underlying, snapshot_nanos).unwrap();
        }

        assert!(buffer.is_empty(), "buffer must be empty for empty input");
        assert_eq!(buffer.row_count(), 0);
    }

    #[test]
    fn test_contract_buffer_with_empty_input() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let empty_contracts: std::collections::HashMap<SecurityId, DerivativeContract> =
            std::collections::HashMap::new();

        // Replicate what write_derivative_contracts does with empty input.
        for contract in empty_contracts.values() {
            write_single_contract(&mut buffer, contract, snapshot_nanos).unwrap();
        }

        assert!(buffer.is_empty(), "buffer must be empty for empty input");
        assert_eq!(buffer.row_count(), 0);
    }

    #[test]
    fn test_subscribed_indices_buffer_with_empty_input() {
        let snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let empty_indices: Vec<SubscribedIndex> = Vec::new();

        // Replicate what write_subscribed_indices does with empty input.
        for index in &empty_indices {
            write_single_subscribed_index(&mut buffer, index, snapshot_nanos).unwrap();
        }

        assert!(buffer.is_empty(), "buffer must be empty for empty input");
        assert_eq!(buffer.row_count(), 0);
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_epoch_date() {
        // 1970-01-01 midnight IST = 1969-12-31 18:30:00 UTC.
        // IST is UTC+5:30, so midnight IST is 5h30m BEFORE midnight UTC.
        // That means the nanos value should be negative: -(5*3600 + 30*60) * 1_000_000_000.
        let epoch_date = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid date");
        let ts = naive_date_to_timestamp_nanos(epoch_date).expect("valid timestamp");

        let expected_nanos: i64 = -((5 * 3600 + 30 * 60) as i64) * 1_000_000_000;
        assert_eq!(
            ts.as_i64(),
            expected_nanos,
            "1970-01-01 midnight IST should be -19800 seconds in nanos (UTC offset)"
        );
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_far_future() {
        // 2099-12-31 — must not overflow or return an error.
        let far_future = NaiveDate::from_ymd_opt(2099, 12, 31).expect("valid date");
        let ts =
            naive_date_to_timestamp_nanos(far_future).expect("far future date must not overflow");

        // 2099-12-31 midnight IST = 2099-12-30 18:30:00 UTC.
        // Must be well past 2020 epoch.
        assert!(
            ts.as_i64() > 1_577_836_800_000_000_000,
            "far future timestamp must be after 2020"
        );
        // Must also be past 2090 to confirm it's truly in the right range.
        // 2090-01-01 UTC ~ 3_786_912_000 seconds ~ 3_786_912_000_000_000_000 nanos.
        assert!(
            ts.as_i64() > 3_786_912_000_000_000_000,
            "far future timestamp must be after 2090"
        );
    }
}
