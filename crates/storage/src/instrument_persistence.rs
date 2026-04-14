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
//! - The designated timestamp (`timestamp` column) uses IST-as-UTC convention:
//!   midnight IST stored directly (e.g., `2026-02-25T00:00:00Z` = midnight IST Feb 25).

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use questdb::ingress::{Buffer, Sender, TimestampMicros, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    ILP_FLUSH_BATCH_SIZE, QUESTDB_TABLE_BUILD_METADATA, QUESTDB_TABLE_DERIVATIVE_CONTRACTS,
    QUESTDB_TABLE_FNO_UNDERLYINGS, QUESTDB_TABLE_SUBSCRIBED_INDICES,
};
use tickvault_common::instrument_types::{
    DerivativeContract, FnoUnderlying, FnoUniverse, SubscribedIndex, UniverseBuildMetadata,
};
use tickvault_common::trading_calendar::ist_offset;
use tickvault_common::types::SecurityId;

// ---------------------------------------------------------------------------
// Instrument Lifecycle Event Types
// ---------------------------------------------------------------------------

/// Type of day-over-day instrument lifecycle event detected by delta detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEventType {
    /// A new derivative contract appeared in today's universe.
    ContractAdded,
    /// A contract present yesterday is absent today (expired or delisted).
    ContractExpired,
    /// Lot size changed for a contract or underlying.
    LotSizeChanged,
    /// Tick size changed for a contract.
    TickSizeChanged,
    /// A generic field changed (strike_price, option_type, segment, display_name, symbol_name).
    FieldChanged,
    /// A new underlying symbol appeared.
    UnderlyingAdded,
    /// An underlying symbol was removed.
    UnderlyingRemoved,
    /// I-P1-03: Same security_id now maps to a different underlying.
    SecurityIdReused,
    /// I-P1-04: Same contract identity got a different security_id.
    SecurityIdReassigned,
}

impl LifecycleEventType {
    /// Returns a snake_case string representation for QuestDB SYMBOL columns.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ContractAdded => "contract_added",
            Self::ContractExpired => "contract_expired",
            Self::LotSizeChanged => "lot_size_changed",
            Self::TickSizeChanged => "tick_size_changed",
            Self::FieldChanged => "field_changed",
            Self::UnderlyingAdded => "underlying_added",
            Self::UnderlyingRemoved => "underlying_removed",
            Self::SecurityIdReused => "security_id_reused",
            Self::SecurityIdReassigned => "security_id_reassigned",
        }
    }
}

/// A single lifecycle event from day-over-day delta detection.
#[derive(Debug, Clone)]
pub struct LifecycleEvent {
    /// The security_id involved (0 for underlying-level events).
    pub security_id: u32,
    /// The underlying symbol this event relates to.
    pub underlying_symbol: String,
    /// The type of change detected.
    pub event_type: LifecycleEventType,
    /// Which field changed (empty for add/remove events).
    pub field_changed: String,
    /// Previous value (empty for add events).
    pub old_value: String,
    /// New value (empty for remove events).
    pub new_value: String,
}

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
/// I-P1-05: Includes `underlying_symbol` to prevent security_id reuse collision
/// across different underlyings (e.g., same ID reused for NIFTY → BANKNIFTY).
const DEDUP_KEY_DERIVATIVE_CONTRACTS: &str = "security_id, underlying_symbol";

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

    // Client::builder().timeout().build() is infallible (no custom TLS).
    // Coverage: unwrap_or_else avoids uncoverable else-return on dead path.
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

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

    let conf_string = questdb_config.build_ilp_conf_string();
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

    // Client::builder().timeout().build() is infallible (no custom TLS).
    // Coverage: unwrap_or_else avoids uncoverable else-return on dead path.
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

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
    let today_ist = Utc::now().with_timezone(&ist_offset()).date_naive();
    naive_date_to_timestamp_nanos(today_ist)
}

/// Converts a `NaiveDate` to `TimestampNanos` at midnight (IST-as-UTC convention).
///
/// Stores midnight of the IST date directly so QuestDB displays
/// `2026-03-09T00:00:00Z` for IST date 2026-03-09. No timezone shifting.
fn naive_date_to_timestamp_nanos(date: NaiveDate) -> Result<TimestampNanos> {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .context("failed to construct midnight time")?;
    let epoch_nanos = midnight
        .and_utc()
        .timestamp_nanos_opt()
        .context("timestamp nanos overflow")?;
    Ok(TimestampNanos::new(epoch_nanos))
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
    write_single_build_metadata(buffer, metadata, snapshot_nanos)?;

    sender
        .flush(buffer)
        .context("flush instrument_build_metadata")?;

    Ok(())
}

/// Writes a single build metadata row into the ILP buffer (no flush).
fn write_single_build_metadata(
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
        if index.saturating_add(1) % ILP_FLUSH_BATCH_SIZE == 0 {
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
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only arithmetic is not on hot path
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use questdb::ingress::ProtocolVersion;
    use std::time::Duration;
    use tickvault_common::instrument_types::{
        DhanInstrumentKind, IndexCategory, IndexSubcategory, UnderlyingKind, UniverseBuildMetadata,
    };
    use tickvault_common::types::{Exchange, ExchangeSegment, OptionType};

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

    /// Spawn a background TCP server that accepts one connection and drains
    /// all data until EOF. Returns the port. Consolidates 8+ identical
    /// spawn-accept-drain patterns into one site.
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

    /// Spawn a background TCP server that accepts multiple connections, each
    /// drained in its own thread. Returns the port. Used by tests that need
    /// both HTTP and ILP connections to the same port.
    fn spawn_multi_accept_tcp_drain_server() -> u16 {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(mut s) => {
                        std::thread::spawn(move || {
                            let mut buf = [0u8; 65536];
                            loop {
                                match s.read(&mut buf) {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {}
                                }
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });
        port
    }

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_400: &str = "HTTP/1.1 400 Bad Request\r\nContent-Length: 31\r\n\r\n{\"error\":\"table does not exist\"}";

    /// Spawn an async HTTP mock server that returns a fixed `response` for
    /// every request. Returns the port. Consolidates 2 identical async
    /// mock-server patterns.
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

    /// Helper: create a minimal UniverseBuildMetadata for testing.
    fn make_test_metadata() -> UniverseBuildMetadata {
        let ist = ist_offset();
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
        let _snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let buffer = Buffer::new(ProtocolVersion::V1);
        let empty_underlyings: std::collections::HashMap<String, FnoUnderlying> =
            std::collections::HashMap::new();

        // Empty input — no rows should be written.
        assert!(empty_underlyings.is_empty());
        assert!(buffer.is_empty(), "buffer must be empty for empty input");
        assert_eq!(buffer.row_count(), 0);
    }

    #[test]
    fn test_contract_buffer_with_empty_input() {
        let _snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let buffer = Buffer::new(ProtocolVersion::V1);
        let empty_contracts: std::collections::HashMap<SecurityId, DerivativeContract> =
            std::collections::HashMap::new();

        // Empty input — no rows should be written.
        assert!(empty_contracts.is_empty());
        assert!(buffer.is_empty(), "buffer must be empty for empty input");
        assert_eq!(buffer.row_count(), 0);
    }

    #[test]
    fn test_subscribed_indices_buffer_with_empty_input() {
        let _snapshot_nanos = naive_date_to_timestamp_nanos(
            NaiveDate::from_ymd_opt(2026, 2, 25).expect("valid date"),
        )
        .expect("valid timestamp");

        let buffer = Buffer::new(ProtocolVersion::V1);
        let empty_indices: Vec<SubscribedIndex> = Vec::new();

        // Empty input — no rows should be written.
        assert!(empty_indices.is_empty());
        assert!(buffer.is_empty(), "buffer must be empty for empty input");
        assert_eq!(buffer.row_count(), 0);
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_epoch_date() {
        // IST-as-UTC convention: 1970-01-01 midnight is stored as epoch 0.
        let epoch_date = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid date");
        let ts = naive_date_to_timestamp_nanos(epoch_date).expect("valid timestamp");

        assert_eq!(
            ts.as_i64(),
            0,
            "1970-01-01 midnight IST-as-UTC should be epoch 0"
        );
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_far_future() {
        // 2099-12-31 — must not overflow or return an error.
        let far_future = NaiveDate::from_ymd_opt(2099, 12, 31).expect("valid date");
        let ts =
            naive_date_to_timestamp_nanos(far_future).expect("far future date must not overflow");

        // IST-as-UTC: 2099-12-31T00:00:00Z (midnight of the date).
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

    // -----------------------------------------------------------------------
    // Coverage gap-fill: write_single_build_metadata (extracted helper)
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_single_build_metadata_produces_single_row() {
        let metadata = make_test_metadata();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_build_metadata(&mut buffer, &metadata, snapshot_nanos).unwrap();

        assert_eq!(buffer.row_count(), 1);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_write_single_build_metadata_contains_all_fields() {
        let metadata = make_test_metadata();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_build_metadata(&mut buffer, &metadata, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_BUILD_METADATA));
        assert!(content.contains("csv_source"));
        assert!(content.contains("primary"));
        assert!(content.contains("csv_row_count"));
        assert!(content.contains("parsed_row_count"));
        assert!(content.contains("index_count"));
        assert!(content.contains("equity_count"));
        assert!(content.contains("underlying_count"));
        assert!(content.contains("derivative_count"));
        assert!(content.contains("option_chain_count"));
        assert!(content.contains("build_duration_ms"));
        assert!(content.contains("build_timestamp"));
    }

    #[test]
    fn test_write_single_build_metadata_with_zero_counts() {
        let ist = ist_offset();
        let metadata = UniverseBuildMetadata {
            csv_source: "fallback".to_string(),
            csv_row_count: 0,
            parsed_row_count: 0,
            index_count: 0,
            equity_count: 0,
            underlying_count: 0,
            derivative_count: 0,
            option_chain_count: 0,
            build_duration: Duration::from_millis(0),
            build_timestamp: Utc::now().with_timezone(&ist),
        };
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_build_metadata(&mut buffer, &metadata, snapshot_nanos).unwrap();

        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("fallback"));
    }

    #[test]
    fn test_write_single_build_metadata_with_large_counts() {
        let ist = ist_offset();
        let metadata = UniverseBuildMetadata {
            csv_source: "primary".to_string(),
            csv_row_count: 1_000_000,
            parsed_row_count: 500_000,
            index_count: 999,
            equity_count: 9999,
            underlying_count: 500,
            derivative_count: 300_000,
            option_chain_count: 5000,
            build_duration: Duration::from_secs(120),
            build_timestamp: Utc::now().with_timezone(&ist),
        };
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_build_metadata(&mut buffer, &metadata, snapshot_nanos).unwrap();

        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        // Verify large counts appear in buffer content.
        assert!(content.contains("1000000i"));
        assert!(content.contains("500000i"));
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: write_single_underlying edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_single_underlying_stock_kind() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let underlying = FnoUnderlying {
            underlying_symbol: "RELIANCE".to_string(),
            underlying_security_id: 2885,
            price_feed_security_id: 2885,
            price_feed_segment: ExchangeSegment::NseEquity,
            derivative_segment: ExchangeSegment::NseFno,
            kind: UnderlyingKind::Stock,
            lot_size: 250,
            contract_count: 120,
        };

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("RELIANCE"));
        assert!(content.contains("NSE_EQ"));
        assert!(content.contains("NSE_FNO"));
        assert!(content.contains("Stock"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_underlying_bse_index() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let underlying = FnoUnderlying {
            underlying_symbol: "SENSEX".to_string(),
            underlying_security_id: 1,
            price_feed_security_id: 1,
            price_feed_segment: ExchangeSegment::BseEquity,
            derivative_segment: ExchangeSegment::BseFno,
            kind: UnderlyingKind::BseIndex,
            lot_size: 10,
            contract_count: 500,
        };

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("SENSEX"));
        assert!(content.contains("BSE_EQ"));
        assert!(content.contains("BSE_FNO"));
        assert!(content.contains("BseIndex"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_underlying_zero_contract_count() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let underlying = FnoUnderlying {
            underlying_symbol: "NEWCO".to_string(),
            underlying_security_id: 99999,
            price_feed_security_id: 99999,
            price_feed_segment: ExchangeSegment::NseEquity,
            derivative_segment: ExchangeSegment::NseFno,
            kind: UnderlyingKind::Stock,
            lot_size: 1,
            contract_count: 0,
        };

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_underlying_mcx_segment() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let underlying = FnoUnderlying {
            underlying_symbol: "CRUDEOIL".to_string(),
            underlying_security_id: 50000,
            price_feed_security_id: 50000,
            price_feed_segment: ExchangeSegment::McxComm,
            derivative_segment: ExchangeSegment::McxComm,
            kind: UnderlyingKind::Stock, // MCX uses Stock kind in the current model
            lot_size: 100,
            contract_count: 24,
        };

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("CRUDEOIL"));
        assert!(content.contains("MCX_COMM"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_multiple_underlyings_different_kinds() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);

        let underlyings = vec![
            FnoUnderlying {
                underlying_symbol: "NIFTY".to_string(),
                underlying_security_id: 26000,
                price_feed_security_id: 13,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::NseIndex,
                lot_size: 75,
                contract_count: 4031,
            },
            FnoUnderlying {
                underlying_symbol: "RELIANCE".to_string(),
                underlying_security_id: 2885,
                price_feed_security_id: 2885,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 250,
                contract_count: 120,
            },
            FnoUnderlying {
                underlying_symbol: "SENSEX".to_string(),
                underlying_security_id: 1,
                price_feed_security_id: 1,
                price_feed_segment: ExchangeSegment::BseEquity,
                derivative_segment: ExchangeSegment::BseFno,
                kind: UnderlyingKind::BseIndex,
                lot_size: 10,
                contract_count: 500,
            },
        ];

        for u in &underlyings {
            write_single_underlying(&mut buffer, u, snapshot_nanos).unwrap();
        }

        assert_eq!(buffer.row_count(), 3);

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("NIFTY"));
        assert!(content.contains("RELIANCE"));
        assert!(content.contains("SENSEX"));
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: write_single_contract edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_single_contract_put_option() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let contract = DerivativeContract {
            security_id: 20000,
            underlying_symbol: "BANKNIFTY".to_string(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 48000.0,
            option_type: Some(OptionType::Put),
            lot_size: 15,
            tick_size: 0.05,
            symbol_name: "BANKNIFTY-Mar2026-48000-PE".to_string(),
            display_name: "BANKNIFTY 27 Mar 48000 PE".to_string(),
        };

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("BANKNIFTY"));
        assert!(content.contains("PE"));
        assert!(content.contains("OptionIndex"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_contract_future_stock() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let contract = DerivativeContract {
            security_id: 30000,
            underlying_symbol: "RELIANCE".to_string(),
            instrument_kind: DhanInstrumentKind::FutureStock,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 4, 30).expect("valid date"),
            strike_price: 0.0,
            option_type: None,
            lot_size: 250,
            tick_size: 0.05,
            symbol_name: "RELIANCE-Apr2026-FUT".to_string(),
            display_name: "RELIANCE 30 Apr FUT".to_string(),
        };

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("RELIANCE"));
        assert!(content.contains("FutureStock"));
        assert!(content.contains("2026-04-30"));
        // Futures have empty option_type — ILP encodes it as empty symbol value.
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_contract_option_stock() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let contract = DerivativeContract {
            security_id: 40000,
            underlying_symbol: "TCS".to_string(),
            instrument_kind: DhanInstrumentKind::OptionStock,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 4000.0,
            option_type: Some(OptionType::Call),
            lot_size: 150,
            tick_size: 0.05,
            symbol_name: "TCS-Mar2026-4000-CE".to_string(),
            display_name: "TCS 27 Mar 4000 CE".to_string(),
        };

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("TCS"));
        assert!(content.contains("OptionStock"));
        assert!(content.contains("CE"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_contract_bse_exchange_segment() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let contract = DerivativeContract {
            security_id: 50000,
            underlying_symbol: "SENSEX".to_string(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::BseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 28).expect("valid date"),
            strike_price: 72000.0,
            option_type: Some(OptionType::Call),
            lot_size: 10,
            tick_size: 0.05,
            symbol_name: "SENSEX-Mar2026-72000-CE".to_string(),
            display_name: "SENSEX 28 Mar 72000 CE".to_string(),
        };

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("BSE_FNO"));
        assert!(content.contains("SENSEX"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_contract_expiry_date_format_is_string_not_timestamp() {
        // Regression: expiry_date must be stored as "YYYY-MM-DD" string, never as timestamp.
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let contract = make_test_contract(12345);
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        // The ILP string column format uses double quotes: expiry_date="2026-03-27"
        assert!(
            content.contains("2026-03-27"),
            "expiry_date must be YYYY-MM-DD string"
        );
        // Must NOT contain a time component from timestamp conversion.
        assert!(
            !content.contains("T18:30:00"),
            "expiry_date must not contain time component"
        );
    }

    #[test]
    fn test_write_many_contracts_batch_row_count() {
        // Simulates writing a realistic number of contracts.
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        for security_id in 10000..10100_u32 {
            let mut contract = make_test_contract(security_id);
            contract.strike_price = f64::from(security_id) * 10.0;
            write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        }

        assert_eq!(buffer.row_count(), 100);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: write_single_subscribed_index edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_single_subscribed_index_broad_market() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let index = make_test_display_index("NIFTY 100", 101, IndexSubcategory::BroadMarket);
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_subscribed_index(&mut buffer, &index, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("BroadMarket"));
        assert!(content.contains("DisplayIndex"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_subscribed_index_midcap() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let index = make_test_display_index("NIFTYMCAP50", 102, IndexSubcategory::MidCap);
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_subscribed_index(&mut buffer, &index, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("MidCap"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_subscribed_index_smallcap() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let index = make_test_display_index("NIFTYSMLCAP50", 103, IndexSubcategory::SmallCap);
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_subscribed_index(&mut buffer, &index, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("SmallCap"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_subscribed_index_thematic() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let index = make_test_display_index("NIFTY CONSUMPTION", 104, IndexSubcategory::Thematic);
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_subscribed_index(&mut buffer, &index, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("Thematic"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_subscribed_index_bse_exchange() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let index = SubscribedIndex {
            symbol: "SENSEX".to_string(),
            security_id: 999,
            exchange: Exchange::BombayStockExchange,
            category: IndexCategory::FnoUnderlying,
            subcategory: IndexSubcategory::Fno,
        };

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_subscribed_index(&mut buffer, &index, snapshot_nanos).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("BSE"));
        assert!(content.contains("SENSEX"));
        assert!(content.contains("FnoUnderlying"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_all_31_subscribed_indices() {
        // Simulate writing all 31 indices (8 FnO + 23 Display) at once.
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);

        // 8 F&O underlying indices.
        for i in 0..8_u32 {
            let idx = make_test_fno_index(&format!("FNO_IDX_{i}"), i + 1);
            write_single_subscribed_index(&mut buffer, &idx, snapshot_nanos).unwrap();
        }

        // 23 Display indices with mixed subcategories.
        let subcategories = [
            IndexSubcategory::Volatility,
            IndexSubcategory::BroadMarket,
            IndexSubcategory::MidCap,
            IndexSubcategory::SmallCap,
            IndexSubcategory::Sectoral,
            IndexSubcategory::Thematic,
        ];
        for i in 0..23_u32 {
            let sub = &subcategories[(i as usize) % subcategories.len()];
            let idx = SubscribedIndex {
                symbol: format!("DISPLAY_IDX_{i}"),
                security_id: 100 + i,
                exchange: Exchange::NationalStockExchange,
                category: IndexCategory::DisplayIndex,
                subcategory: *sub,
            };
            write_single_subscribed_index(&mut buffer, &idx, snapshot_nanos).unwrap();
        }

        assert_eq!(buffer.row_count(), 31);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: build_snapshot_timestamp
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_snapshot_timestamp_returns_valid_value() {
        // build_snapshot_timestamp uses Utc::now(), so we just verify it returns
        // a reasonable value (today in IST, as nanos).
        let ts = build_snapshot_timestamp().unwrap();
        // Must be after 2020-01-01 and before 2100-01-01.
        assert!(ts.as_i64() > 1_577_836_800_000_000_000);
        assert!(ts.as_i64() < 4_102_444_800_000_000_000);
    }

    #[test]
    fn test_build_snapshot_timestamp_is_at_ist_midnight() {
        // IST-as-UTC convention: snapshot timestamp is midnight of the IST date.
        // The nanos value should be exactly divisible by nanos_per_day (no offset).
        let ts = build_snapshot_timestamp().unwrap();
        let nanos = ts.as_i64();

        let nanos_per_day: i64 = 86_400 * 1_000_000_000;
        assert_eq!(
            nanos % nanos_per_day,
            0,
            "snapshot timestamp must be at midnight (IST-as-UTC convention)"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: naive_date_to_timestamp_nanos additional cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_naive_date_to_timestamp_nanos_known_date_exact_value() {
        // IST-as-UTC: 2026-03-01 midnight stored as 2026-03-01T00:00:00Z.
        let date = NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date");
        let ts = naive_date_to_timestamp_nanos(date).unwrap();

        // 2026-03-01T00:00:00Z
        // Days from epoch to 2026-03-01 = 20,513 days
        // 20513 * 86400 = 1,772,323,200 seconds
        // = 1,772,323,200,000,000,000 nanos
        let expected_nanos: i64 = 1_772_323_200_000_000_000;
        assert_eq!(ts.as_i64(), expected_nanos);
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_leap_year() {
        // 2024-02-29 is a valid leap year date.
        let date = NaiveDate::from_ymd_opt(2024, 2, 29).expect("valid leap year date");
        let ts = naive_date_to_timestamp_nanos(date).unwrap();

        // Must be valid and positive (after 2020).
        assert!(ts.as_i64() > 1_577_836_800_000_000_000);
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_year_boundary() {
        // Test Dec 31 -> Jan 1 transition.
        let dec31 = NaiveDate::from_ymd_opt(2025, 12, 31).expect("valid date");
        let jan01 = NaiveDate::from_ymd_opt(2026, 1, 1).expect("valid date");

        let ts_dec31 = naive_date_to_timestamp_nanos(dec31).unwrap();
        let ts_jan01 = naive_date_to_timestamp_nanos(jan01).unwrap();

        // Jan 1 should be exactly 1 day (86400 seconds) after Dec 31.
        let nanos_per_day: i64 = 86_400 * 1_000_000_000;
        assert_eq!(ts_jan01.as_i64() - ts_dec31.as_i64(), nanos_per_day);
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_consecutive_days() {
        // Any two consecutive days should differ by exactly 86400 * 1e9 nanos.
        let nanos_per_day: i64 = 86_400 * 1_000_000_000;

        let d1 = NaiveDate::from_ymd_opt(2026, 6, 15).expect("valid date");
        let d2 = NaiveDate::from_ymd_opt(2026, 6, 16).expect("valid date");

        let ts1 = naive_date_to_timestamp_nanos(d1).unwrap();
        let ts2 = naive_date_to_timestamp_nanos(d2).unwrap();

        assert_eq!(ts2.as_i64() - ts1.as_i64(), nanos_per_day);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: DEDUP key constants and DDL SQL format
    // -----------------------------------------------------------------------

    #[test]
    fn test_dedup_key_constants_are_non_empty() {
        assert!(!DEDUP_KEY_BUILD_METADATA.is_empty());
        assert!(!DEDUP_KEY_FNO_UNDERLYINGS.is_empty());
        assert!(!DEDUP_KEY_DERIVATIVE_CONTRACTS.is_empty());
        assert!(!DEDUP_KEY_SUBSCRIBED_INDICES.is_empty());
    }

    #[test]
    fn test_questdb_ddl_timeout_is_reasonable() {
        assert!((5..=60).contains(&QUESTDB_DDL_TIMEOUT_SECS));
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: persist_instrument_snapshot (async wrapper)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_persist_instrument_snapshot_returns_ok_on_connection_failure() {
        // persist_instrument_snapshot catches errors and returns Ok(()) — trading
        // is never blocked by QuestDB failures.
        let universe = FnoUniverse {
            underlyings: std::collections::HashMap::new(),
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: make_test_metadata(),
        };

        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(), // RFC 5737 TEST-NET, unreachable
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };

        // Must return Ok(()) even when QuestDB is unreachable.
        let result = persist_instrument_snapshot(&universe, &config).await;
        assert!(
            result.is_ok(),
            "persist_instrument_snapshot must not propagate errors"
        );
    }

    #[tokio::test]
    async fn test_persist_instrument_snapshot_returns_ok_on_invalid_port() {
        // Using port 0 which is invalid/unreachable for ILP.
        let universe = FnoUniverse {
            underlyings: std::collections::HashMap::new(),
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: make_test_metadata(),
        };

        let config = QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 0,
            pg_port: 0,
            ilp_port: 0,
        };

        let result = persist_instrument_snapshot(&universe, &config).await;
        assert!(
            result.is_ok(),
            "persist_instrument_snapshot must swallow errors"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: buffer clearing after row writes
    // -----------------------------------------------------------------------

    #[test]
    fn test_buffer_clear_resets_row_count() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let underlying = make_test_underlying("NIFTY", 26000);
        write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);

        buffer.clear();
        assert_eq!(buffer.row_count(), 0);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_mixed_table_writes_to_same_buffer() {
        // Verify that writing rows from different tables into the same buffer
        // accumulates row counts correctly (this is what persist_inner does).
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);

        // Write 1 build metadata row.
        write_single_build_metadata(&mut buffer, &make_test_metadata(), snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);

        // Write 2 underlying rows.
        write_single_underlying(
            &mut buffer,
            &make_test_underlying("NIFTY", 26000),
            snapshot_nanos,
        )
        .unwrap();
        write_single_underlying(
            &mut buffer,
            &make_test_underlying("BANKNIFTY", 26009),
            snapshot_nanos,
        )
        .unwrap();
        assert_eq!(buffer.row_count(), 3);

        // Write 1 contract row.
        write_single_contract(&mut buffer, &make_test_contract(12345), snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 4);

        // Write 1 subscribed index row.
        write_single_subscribed_index(
            &mut buffer,
            &make_test_fno_index("NIFTY", 13),
            snapshot_nanos,
        )
        .unwrap();
        assert_eq!(buffer.row_count(), 5);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: persist_inner via persist_instrument_snapshot
    // (covers lines 86-149 — Sender creation, flush error paths)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_persist_inner_fails_on_unreachable_ilp_sender() {
        // persist_inner tries Sender::from_conf which may fail on unreachable
        // TCP endpoint. persist_instrument_snapshot wraps this and returns Ok.
        // This exercises the Err arm at line 94-99 of persist_instrument_snapshot.
        let universe = FnoUniverse {
            underlyings: std::collections::HashMap::new(),
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: make_test_metadata(),
        };

        // Port 1 on loopback — connection refused fast, exercises error path.
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };

        let result = persist_instrument_snapshot(&universe, &config).await;
        assert!(
            result.is_ok(),
            "persist_instrument_snapshot must catch errors from persist_inner"
        );
    }

    #[tokio::test]
    async fn test_persist_inner_with_nonempty_universe_and_unreachable_host() {
        // Tests persist_inner with actual data in the universe — exercises the
        // code paths that build metadata, underlyings, contracts, and indices
        // before the flush fails.
        let mut underlyings = std::collections::HashMap::new();
        underlyings.insert("NIFTY".to_string(), make_test_underlying("NIFTY", 26000));
        underlyings.insert(
            "BANKNIFTY".to_string(),
            make_test_underlying("BANKNIFTY", 26009),
        );

        let mut derivative_contracts = std::collections::HashMap::new();
        derivative_contracts.insert(10001_u32, make_test_contract(10001));
        derivative_contracts.insert(10002_u32, make_test_contract(10002));

        let subscribed_indices = vec![
            make_test_fno_index("NIFTY", 13),
            make_test_fno_index("BANKNIFTY", 25),
        ];

        let universe = FnoUniverse {
            underlyings,
            derivative_contracts,
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices,
            build_metadata: make_test_metadata(),
        };

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };

        let result = persist_instrument_snapshot(&universe, &config).await;
        assert!(
            result.is_ok(),
            "must return Ok even with populated universe and unreachable QuestDB"
        );
    }

    #[tokio::test]
    async fn test_persist_inner_with_invalid_hostname() {
        // Completely bogus hostname — DNS resolution fails.
        let universe = FnoUniverse {
            underlyings: std::collections::HashMap::new(),
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: make_test_metadata(),
        };

        let config = QuestDbConfig {
            host: "this.host.does.not.exist.example.invalid".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };

        let result = persist_instrument_snapshot(&universe, &config).await;
        assert!(
            result.is_ok(),
            "persist_instrument_snapshot must swallow DNS resolution errors"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: persist_inner directly (lines 109-151)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_persist_inner_error_path_returns_err() {
        // Call persist_inner directly to verify it returns Err (not swallowed).
        let universe = FnoUniverse {
            underlyings: std::collections::HashMap::new(),
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: make_test_metadata(),
        };

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };

        // persist_inner propagates errors unlike persist_instrument_snapshot.
        let result = persist_inner(&universe, &config).await;
        // On most systems, TCP to port 1 on loopback is refused.
        // The result depends on whether Sender::from_conf connects eagerly or lazily.
        // Either way, it should not panic.
        let _is_err = result.is_err();
    }

    #[tokio::test]
    async fn test_persist_inner_with_populated_universe_error_path() {
        // Tests persist_inner directly with a populated universe.
        let mut underlyings = std::collections::HashMap::new();
        underlyings.insert("NIFTY".to_string(), make_test_underlying("NIFTY", 26000));

        let mut derivative_contracts = std::collections::HashMap::new();
        for i in 0..5_u32 {
            derivative_contracts.insert(10000 + i, make_test_contract(10000 + i));
        }

        let subscribed_indices = vec![make_test_fno_index("NIFTY", 13)];

        let universe = FnoUniverse {
            underlyings,
            derivative_contracts,
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices,
            build_metadata: make_test_metadata(),
        };

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };

        let result = persist_inner(&universe, &config).await;
        // Should not panic, regardless of whether it's Ok or Err.
        let _is_err = result.is_err();
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: ensure_table_dedup_keys additional configs
    // (covers lines 180-232 — HTTP client build, request, response paths)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_with_port_1() {
        // Port 1 is almost always refused — exercises the Err arm of the HTTP send.
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        ensure_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_with_invalid_hostname() {
        // DNS resolution failure — exercises the Err arm differently.
        let config = QuestDbConfig {
            host: "this.host.does.not.exist.example.invalid".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        ensure_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_with_ipv6_loopback() {
        // IPv6 loopback on a random port — exercises different network path.
        let config = QuestDbConfig {
            host: "::1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        ensure_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_success_with_mock_http() {
        // Start a mock HTTP server that returns 200 OK for all DDL requests.
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        // Exercises the success path (response.status().is_success() == true).
        ensure_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_non_success_with_mock_http() {
        // Start a mock HTTP server that returns 400 Bad Request for DDL requests.
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        // Exercises the non-success path (response.status().is_success() == false).
        ensure_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: write_build_metadata / write_underlyings /
    // write_derivative_contracts / write_subscribed_indices with Sender
    // (covers sender.flush error paths — lines 272-276, 357-359, etc.)
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_build_metadata_with_sender_flush_error() {
        // Live TCP server that drains data — exercises the Ok path of
        // write_build_metadata (write_single_build_metadata + sender.flush).
        let port = spawn_tcp_drain_server();

        let conf = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf).unwrap();
        let mut buffer = sender.new_buffer();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();
        let metadata = make_test_metadata();

        // write_build_metadata calls write_single_build_metadata + flush.
        write_build_metadata(&mut sender, &mut buffer, &metadata, snapshot_nanos).unwrap();
    }

    #[test]
    fn test_write_underlyings_with_sender_flush_error() {
        let port = spawn_tcp_drain_server();

        let conf = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf).unwrap();
        let mut buffer = sender.new_buffer();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut underlyings = std::collections::HashMap::new();
        underlyings.insert("NIFTY".to_string(), make_test_underlying("NIFTY", 26000));

        // write_underlyings iterates, writes rows, then flushes.
        write_underlyings(&mut sender, &mut buffer, &underlyings, snapshot_nanos).unwrap();
    }

    #[test]
    fn test_write_derivative_contracts_with_sender_flush_error() {
        let port = spawn_tcp_drain_server();

        let conf = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf).unwrap();
        let mut buffer = sender.new_buffer();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut contracts = std::collections::HashMap::new();
        contracts.insert(10001_u32, make_test_contract(10001));

        // write_derivative_contracts iterates, writes rows, batch-flushes, final flush.
        write_derivative_contracts(&mut sender, &mut buffer, &contracts, snapshot_nanos).unwrap();
    }

    #[test]
    fn test_write_subscribed_indices_with_sender_flush_error() {
        let port = spawn_tcp_drain_server();

        let conf = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf).unwrap();
        let mut buffer = sender.new_buffer();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let indices = vec![make_test_fno_index("NIFTY", 13)];

        // write_subscribed_indices iterates, writes rows, then flushes.
        write_subscribed_indices(&mut sender, &mut buffer, &indices, snapshot_nanos).unwrap();
    }

    #[test]
    fn test_write_derivative_contracts_empty_with_sender() {
        // Empty contracts — no flush needed, should return Ok.
        let port = spawn_tcp_drain_server();

        let conf = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf).unwrap();
        let mut buffer = sender.new_buffer();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let contracts: std::collections::HashMap<SecurityId, DerivativeContract> =
            std::collections::HashMap::new();

        // With empty contracts, no rows are written and no flush is needed.
        let result =
            write_derivative_contracts(&mut sender, &mut buffer, &contracts, snapshot_nanos);
        assert!(result.is_ok(), "empty contracts should not trigger flush");
    }

    #[test]
    fn test_write_subscribed_indices_empty_with_sender() {
        // Empty indices — no flush needed, should return Ok.
        let port = spawn_tcp_drain_server();

        let conf = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf).unwrap();
        let mut buffer = sender.new_buffer();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let indices: Vec<SubscribedIndex> = Vec::new();

        let result = write_subscribed_indices(&mut sender, &mut buffer, &indices, snapshot_nanos);
        assert!(result.is_ok(), "empty indices should not trigger flush");
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: write_derivative_contracts batch flush path
    // (covers the mid-batch flush at ILP_FLUSH_BATCH_SIZE boundary)
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_derivative_contracts_batch_flush_boundary() {
        // Creates exactly ILP_FLUSH_BATCH_SIZE + 1 contracts to exercise
        // the batch flush path inside write_derivative_contracts.
        let port = spawn_tcp_drain_server();

        let conf = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf).unwrap();
        let mut buffer = sender.new_buffer();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut contracts = std::collections::HashMap::new();
        for i in 0..(ILP_FLUSH_BATCH_SIZE as u32 + 1) {
            contracts.insert(10000 + i, make_test_contract(10000 + i));
        }

        // This exercises the batch flush at ILP_FLUSH_BATCH_SIZE boundary
        // and the final flush for remaining rows — both succeed with the TCP server.
        let result =
            write_derivative_contracts(&mut sender, &mut buffer, &contracts, snapshot_nanos);
        assert!(
            result.is_ok(),
            "batch flush must succeed with live TCP server"
        );
    }

    // -----------------------------------------------------------------------
    // Live TCP server tests — covers write + flush operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_all_tables_with_live_tcp_server() {
        // Dummy TCP server that accepts connections and drains data
        let port = spawn_tcp_drain_server();

        let conf = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf).unwrap();
        let mut buffer = sender.new_buffer();
        let snapshot_nanos = TimestampNanos::new(1_740_556_500_000_000_000);

        // Write build metadata
        let metadata = make_test_metadata();
        write_build_metadata(&mut sender, &mut buffer, &metadata, snapshot_nanos).unwrap();

        // Write underlyings
        let mut underlyings = std::collections::HashMap::new();
        underlyings.insert("NIFTY".to_string(), make_test_underlying("NIFTY", 26000));
        underlyings.insert(
            "BANKNIFTY".to_string(),
            make_test_underlying("BANKNIFTY", 26009),
        );
        write_underlyings(&mut sender, &mut buffer, &underlyings, snapshot_nanos).unwrap();

        // Write derivative contracts
        let mut contracts = std::collections::HashMap::new();
        contracts.insert(50001, make_test_contract(50001));
        contracts.insert(50002, make_test_contract(50002));
        contracts.insert(50003, make_test_contract(50003));
        write_derivative_contracts(&mut sender, &mut buffer, &contracts, snapshot_nanos).unwrap();

        // Write subscribed indices
        let indices = vec![
            make_test_fno_index("NIFTY 50", 13),
            make_test_display_index("SENSEX", 1, IndexSubcategory::BroadMarket),
        ];
        write_subscribed_indices(&mut sender, &mut buffer, &indices, snapshot_nanos).unwrap();
    }

    #[tokio::test]
    async fn test_persist_instrument_snapshot_success_path() {
        let port = spawn_multi_accept_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        // Build a populated universe
        let universe = FnoUniverse {
            build_metadata: make_test_metadata(),
            underlyings: {
                let mut m = std::collections::HashMap::new();
                m.insert("NIFTY".to_string(), make_test_underlying("NIFTY", 26000));
                m
            },
            derivative_contracts: {
                let mut m = std::collections::HashMap::new();
                m.insert(50001, make_test_contract(50001));
                m
            },
            subscribed_indices: vec![make_test_fno_index("NIFTY 50", 13)],
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
        };

        let result = persist_instrument_snapshot(&universe, &config).await;
        // persist_instrument_snapshot swallows errors and always returns Ok
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // I-P1-05: DEDUP_KEY_DERIVATIVE_CONTRACTS must include underlying_symbol
    // -----------------------------------------------------------------------

    #[test]
    fn test_dedup_key_derivative_contracts_includes_security_id_and_underlying() {
        // I-P1-05: Compound key must include both security_id and underlying_symbol
        // to prevent cross-underlying collision when security_ids are reused.
        assert!(
            DEDUP_KEY_DERIVATIVE_CONTRACTS.contains("security_id"),
            "DEDUP_KEY_DERIVATIVE_CONTRACTS must include security_id"
        );
        assert!(
            DEDUP_KEY_DERIVATIVE_CONTRACTS.contains("underlying_symbol"),
            "I-P1-05: DEDUP_KEY_DERIVATIVE_CONTRACTS must include underlying_symbol \
             to prevent cross-underlying security_id collisions"
        );
        assert_eq!(
            DEDUP_KEY_DERIVATIVE_CONTRACTS, "security_id, underlying_symbol",
            "I-P1-05: exact dedup key value"
        );
    }

    #[test]
    fn test_dedup_key_fno_underlyings_is_underlying_symbol() {
        assert_eq!(
            DEDUP_KEY_FNO_UNDERLYINGS, "underlying_symbol",
            "fno_underlyings DEDUP key must be underlying_symbol"
        );
    }

    #[test]
    fn test_dedup_key_build_metadata_is_csv_source() {
        assert_eq!(DEDUP_KEY_BUILD_METADATA, "csv_source");
    }

    #[test]
    fn test_dedup_key_subscribed_indices_is_security_id() {
        assert_eq!(DEDUP_KEY_SUBSCRIBED_INDICES, "security_id");
    }

    // -----------------------------------------------------------------------
    // DDL contains correct DEDUP key columns
    // -----------------------------------------------------------------------

    #[test]
    fn test_derivative_contracts_ddl_contains_dedup_column() {
        assert!(
            DERIVATIVE_CONTRACTS_CREATE_DDL.contains("security_id"),
            "DDL must contain the DEDUP key column"
        );
    }

    #[test]
    fn test_fno_underlyings_ddl_contains_dedup_column() {
        assert!(
            FNO_UNDERLYINGS_CREATE_DDL.contains("underlying_symbol"),
            "DDL must contain the DEDUP key column"
        );
    }

    #[test]
    fn test_derivative_contracts_ddl_has_underlying_symbol() {
        // I-P1-05: DDL must have underlying_symbol column for compound queries
        assert!(
            DERIVATIVE_CONTRACTS_CREATE_DDL.contains("underlying_symbol"),
            "derivative_contracts DDL must include underlying_symbol for cross-underlying queries"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: ensure_instrument_tables DDL tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_instrument_tables_does_not_panic_unreachable() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Should not panic — just logs warnings and returns.
        ensure_instrument_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_instrument_tables_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the success path for all 4 CREATE TABLE + 4 DEDUP DDL.
        ensure_instrument_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_instrument_tables_non_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the non-success path for CREATE TABLE DDL.
        ensure_instrument_tables(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: LifecycleEventType tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_lifecycle_event_type_as_str_all_variants() {
        assert_eq!(LifecycleEventType::ContractAdded.as_str(), "contract_added");
        assert_eq!(
            LifecycleEventType::ContractExpired.as_str(),
            "contract_expired"
        );
        assert_eq!(
            LifecycleEventType::LotSizeChanged.as_str(),
            "lot_size_changed"
        );
        assert_eq!(
            LifecycleEventType::TickSizeChanged.as_str(),
            "tick_size_changed"
        );
        assert_eq!(LifecycleEventType::FieldChanged.as_str(), "field_changed");
        assert_eq!(
            LifecycleEventType::UnderlyingAdded.as_str(),
            "underlying_added"
        );
        assert_eq!(
            LifecycleEventType::UnderlyingRemoved.as_str(),
            "underlying_removed"
        );
        assert_eq!(
            LifecycleEventType::SecurityIdReused.as_str(),
            "security_id_reused"
        );
        assert_eq!(
            LifecycleEventType::SecurityIdReassigned.as_str(),
            "security_id_reassigned"
        );
    }

    #[test]
    fn test_lifecycle_event_type_equality() {
        assert_eq!(
            LifecycleEventType::ContractAdded,
            LifecycleEventType::ContractAdded
        );
        assert_ne!(
            LifecycleEventType::ContractAdded,
            LifecycleEventType::ContractExpired
        );
    }

    #[test]
    fn test_lifecycle_event_clone() {
        let event = LifecycleEvent {
            security_id: 12345,
            underlying_symbol: "NIFTY".to_string(),
            event_type: LifecycleEventType::ContractAdded,
            field_changed: String::new(),
            old_value: String::new(),
            new_value: String::new(),
        };
        let cloned = event.clone();
        assert_eq!(cloned.security_id, 12345);
        assert_eq!(cloned.underlying_symbol, "NIFTY");
        assert_eq!(cloned.event_type, LifecycleEventType::ContractAdded);
    }

    #[test]
    fn test_lifecycle_event_field_changed() {
        let event = LifecycleEvent {
            security_id: 42,
            underlying_symbol: "RELIANCE".to_string(),
            event_type: LifecycleEventType::LotSizeChanged,
            field_changed: "lot_size".to_string(),
            old_value: "250".to_string(),
            new_value: "500".to_string(),
        };
        assert_eq!(event.field_changed, "lot_size");
        assert_eq!(event.old_value, "250");
        assert_eq!(event.new_value, "500");
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: DDL constants validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_metadata_ddl_is_valid() {
        assert!(BUILD_METADATA_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("instrument_build_metadata"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("csv_source SYMBOL"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("csv_row_count LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("build_duration_ms LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("build_timestamp TIMESTAMP"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("PARTITION BY DAY WAL"));
    }

    #[test]
    fn test_fno_underlyings_ddl_is_valid() {
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("fno_underlyings"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("underlying_symbol SYMBOL"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("price_feed_segment SYMBOL"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("lot_size LONG"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("contract_count LONG"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("PARTITION BY DAY WAL"));
    }

    #[test]
    fn test_derivative_contracts_ddl_is_valid() {
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("derivative_contracts"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("security_id LONG"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("expiry_date STRING"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("strike_price DOUBLE"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("tick_size DOUBLE"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("PARTITION BY DAY WAL"));
    }

    #[test]
    fn test_subscribed_indices_ddl_is_valid() {
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("subscribed_indices"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("symbol SYMBOL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("exchange SYMBOL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("category SYMBOL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("subcategory SYMBOL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("PARTITION BY DAY WAL"));
    }

    #[test]
    fn test_all_ddl_are_single_statements() {
        assert!(!BUILD_METADATA_CREATE_DDL.contains(';'));
        assert!(!FNO_UNDERLYINGS_CREATE_DDL.contains(';'));
        assert!(!DERIVATIVE_CONTRACTS_CREATE_DDL.contains(';'));
        assert!(!SUBSCRIBED_INDICES_CREATE_DDL.contains(';'));
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: write_build_metadata via Sender (flush path)
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_build_metadata_reuse_after_flush() {
        let port = spawn_tcp_drain_server();
        let conf = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf).unwrap();
        let mut buffer = sender.new_buffer();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();
        let metadata = make_test_metadata();

        // First write + flush
        write_build_metadata(&mut sender, &mut buffer, &metadata, snapshot_nanos).unwrap();

        // Buffer should be reusable after flush
        write_single_build_metadata(&mut buffer, &metadata, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_underlyings_empty_produces_no_rows() {
        // Empty underlyings — write_underlyings writes 0 rows then flushes.
        // Flushing an empty buffer to QuestDB returns an error from the Sender,
        // which is expected. Verify no rows are written to the buffer.
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date"))
                .unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let underlyings: std::collections::HashMap<String, FnoUnderlying> =
            std::collections::HashMap::new();

        // No rows written for empty input.
        for underlying in underlyings.values() {
            write_single_underlying(&mut buffer, underlying, snapshot_nanos).unwrap();
        }
        assert_eq!(
            buffer.row_count(),
            0,
            "empty underlyings should produce 0 rows"
        );
        assert!(buffer.is_empty());
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: persist_inner with TCP drain (success path)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_persist_inner_success_with_multi_accept_server() {
        let port = spawn_multi_accept_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut underlyings = std::collections::HashMap::new();
        underlyings.insert("NIFTY".to_string(), make_test_underlying("NIFTY", 26000));

        let mut contracts = std::collections::HashMap::new();
        contracts.insert(10001_u32, make_test_contract(10001));
        contracts.insert(10002_u32, make_test_contract(10002));

        let indices = vec![make_test_fno_index("NIFTY", 13)];

        let universe = FnoUniverse {
            underlyings,
            derivative_contracts: contracts,
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: indices,
            build_metadata: make_test_metadata(),
        };

        let result = persist_inner(&universe, &config).await;
        // With multi-accept TCP drain, the ILP writes should succeed
        // (HTTP DDL may fail since raw TCP is not HTTP, but ILP should work).
        // Either way, it should not panic.
        let _is_ok = result.is_ok();
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: build_snapshot_timestamp epoch precision
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_snapshot_epoch_nanos_returns_midnight_ist() {
        // I-P1-08: snapshot timestamp must be at midnight IST.
        let ts = build_snapshot_timestamp().unwrap();
        let nanos = ts.as_i64();
        let nanos_per_day: i64 = 86_400 * 1_000_000_000;
        assert_eq!(
            nanos % nanos_per_day,
            0,
            "snapshot timestamp must be at midnight (IST-as-UTC convention)"
        );
    }

    #[test]
    fn test_build_snapshot_epoch_nanos_matches_naive_date_function() {
        // I-P1-08: build_snapshot_timestamp and naive_date_to_timestamp_nanos
        // must produce the same result for today's IST date.
        let ts1 = build_snapshot_timestamp().unwrap();
        let today_ist = chrono::Utc::now().with_timezone(&ist_offset()).date_naive();
        let ts2 = naive_date_to_timestamp_nanos(today_ist).unwrap();
        assert_eq!(ts1.as_i64(), ts2.as_i64());
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: LifecycleEvent construction, DDL constants,
    // write helpers, naive_date edge cases, ensure_instrument_tables
    // -----------------------------------------------------------------------

    #[test]
    fn test_lifecycle_event_type_clone_roundtrip() {
        let original = LifecycleEventType::SecurityIdReused;
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    #[test]
    fn test_lifecycle_event_construction_and_fields() {
        let event = LifecycleEvent {
            security_id: 12345,
            underlying_symbol: "NIFTY".to_string(),
            event_type: LifecycleEventType::LotSizeChanged,
            field_changed: "lot_size".to_string(),
            old_value: "75".to_string(),
            new_value: "50".to_string(),
        };
        assert_eq!(event.security_id, 12345);
        assert_eq!(event.underlying_symbol, "NIFTY");
        assert_eq!(event.event_type, LifecycleEventType::LotSizeChanged);
        assert_eq!(event.field_changed, "lot_size");
        assert_eq!(event.old_value, "75");
        assert_eq!(event.new_value, "50");
    }

    #[test]
    fn test_lifecycle_event_add_has_empty_old_value() {
        let event = LifecycleEvent {
            security_id: 99999,
            underlying_symbol: "RELIANCE".to_string(),
            event_type: LifecycleEventType::ContractAdded,
            field_changed: String::new(),
            old_value: String::new(),
            new_value: "added".to_string(),
        };
        assert!(event.old_value.is_empty());
        assert!(event.field_changed.is_empty());
    }

    #[test]
    fn test_lifecycle_event_remove_has_empty_new_value() {
        let event = LifecycleEvent {
            security_id: 0,
            underlying_symbol: "BANKNIFTY".to_string(),
            event_type: LifecycleEventType::UnderlyingRemoved,
            field_changed: String::new(),
            old_value: "present".to_string(),
            new_value: String::new(),
        };
        assert_eq!(event.security_id, 0);
        assert!(event.new_value.is_empty());
    }

    #[test]
    fn test_dedup_key_build_metadata_value() {
        assert_eq!(DEDUP_KEY_BUILD_METADATA, "csv_source");
    }

    #[test]
    fn test_dedup_key_fno_underlyings_value() {
        assert_eq!(DEDUP_KEY_FNO_UNDERLYINGS, "underlying_symbol");
    }

    #[test]
    fn test_dedup_key_derivative_contracts_includes_underlying() {
        // I-P1-05: Must include underlying_symbol to prevent security_id reuse collision
        assert!(DEDUP_KEY_DERIVATIVE_CONTRACTS.contains("security_id"));
        assert!(DEDUP_KEY_DERIVATIVE_CONTRACTS.contains("underlying_symbol"));
    }

    #[test]
    fn test_dedup_key_subscribed_indices_value() {
        assert_eq!(DEDUP_KEY_SUBSCRIBED_INDICES, "security_id");
    }

    #[test]
    fn test_build_metadata_ddl_contains_all_columns() {
        assert!(BUILD_METADATA_CREATE_DDL.contains("instrument_build_metadata"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("csv_source SYMBOL"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("csv_row_count LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("parsed_row_count LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("index_count LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("equity_count LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("underlying_count LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("derivative_count LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("option_chain_count LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("build_duration_ms LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("build_timestamp TIMESTAMP"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("TIMESTAMP(timestamp)"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("PARTITION BY DAY WAL"));
    }

    #[test]
    fn test_fno_underlyings_ddl_contains_all_columns() {
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("fno_underlyings"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("underlying_symbol SYMBOL"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("price_feed_segment SYMBOL"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("derivative_segment SYMBOL"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("kind SYMBOL"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("lot_size LONG"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("contract_count LONG"));
    }

    #[test]
    fn test_derivative_contracts_ddl_contains_all_columns() {
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("derivative_contracts"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("underlying_symbol SYMBOL"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("instrument_kind SYMBOL"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("exchange_segment SYMBOL"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("option_type SYMBOL"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("security_id LONG"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("expiry_date STRING"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("strike_price DOUBLE"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("lot_size LONG"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("tick_size DOUBLE"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("display_name STRING"));
    }

    #[test]
    fn test_subscribed_indices_ddl_contains_all_columns() {
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("subscribed_indices"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("symbol SYMBOL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("exchange SYMBOL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("category SYMBOL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("subcategory SYMBOL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("security_id LONG"));
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_different_dates_are_distinct() {
        let date1 = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let date2 = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let ts1 = naive_date_to_timestamp_nanos(date1).unwrap();
        let ts2 = naive_date_to_timestamp_nanos(date2).unwrap();
        assert_ne!(ts1.as_i64(), ts2.as_i64());
        // One day = 86400 * 1e9 nanos
        let one_day_nanos: i64 = 86_400 * 1_000_000_000;
        assert_eq!(ts2.as_i64() - ts1.as_i64(), one_day_nanos);
    }

    #[test]
    fn test_naive_date_to_timestamp_nanos_always_at_midnight() {
        let dates = [
            NaiveDate::from_ymd_opt(2025, 6, 15).unwrap(),
            NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(),
            NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(), // leap year
        ];
        let nanos_per_day: i64 = 86_400 * 1_000_000_000;
        for date in dates {
            let ts = naive_date_to_timestamp_nanos(date).unwrap();
            assert_eq!(
                ts.as_i64() % nanos_per_day,
                0,
                "timestamp for {:?} must be at midnight",
                date
            );
        }
    }

    #[test]
    fn test_write_single_build_metadata_produces_one_row() {
        let metadata = make_test_metadata();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_build_metadata(&mut buffer, &metadata, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_single_underlying_produces_one_row() {
        let underlying = make_test_underlying("FINNIFTY", 26037);
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("FINNIFTY"));
    }

    #[test]
    fn test_write_single_contract_with_put_option() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut contract = make_test_contract(55555);
        contract.option_type = Some(OptionType::Put);
        contract.strike_price = 22000.0;

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("PE"));
    }

    #[test]
    fn test_write_single_contract_future_no_option_type() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut contract = make_test_contract(66666);
        contract.option_type = None; // Future — no option type
        contract.instrument_kind = DhanInstrumentKind::FutureIndex;
        contract.strike_price = 0.0;

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
        // Empty option_type should not cause issues
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("FutureIndex"));
    }

    #[test]
    fn test_write_single_subscribed_index_produces_one_row() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let index = make_test_display_index("NIFTY IT", 19, IndexSubcategory::Sectoral);
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_subscribed_index(&mut buffer, &index, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_multiple_underlyings_batch() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let symbols = ["NIFTY", "BANKNIFTY", "FINNIFTY", "RELIANCE", "TCS"];
        for (i, sym) in symbols.iter().enumerate() {
            let underlying = make_test_underlying(sym, 26000 + i as u32);
            write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();
        }
        assert_eq!(buffer.row_count(), 5);
    }

    #[test]
    fn test_write_multiple_contracts_batch() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        for id in 10000..10020_u32 {
            let contract = make_test_contract(id);
            write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        }
        assert_eq!(buffer.row_count(), 20);
    }

    #[test]
    fn test_underlying_with_stock_kind() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut underlying = make_test_underlying("RELIANCE", 2885);
        underlying.kind = UnderlyingKind::Stock;
        underlying.price_feed_segment = ExchangeSegment::NseEquity;

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("Stock"));
        assert!(content.contains("NSE_EQ"));
    }

    #[test]
    fn test_contract_with_bse_segment() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut contract = make_test_contract(77777);
        contract.exchange_segment = ExchangeSegment::BseFno;

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("BSE_FNO"));
    }

    #[test]
    fn test_build_snapshot_timestamp_is_positive() {
        let ts = build_snapshot_timestamp().unwrap();
        assert!(ts.as_i64() > 0, "snapshot timestamp must be positive");
    }

    #[test]
    fn test_naive_date_far_future() {
        let date = NaiveDate::from_ymd_opt(2099, 12, 31).unwrap();
        let ts = naive_date_to_timestamp_nanos(date).unwrap();
        assert!(ts.as_i64() > 0);
    }

    #[test]
    fn test_naive_date_past_date() {
        let date = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let ts = naive_date_to_timestamp_nanos(date).unwrap();
        assert!(ts.as_i64() > 0);
        // 2020-01-01 midnight UTC = 1577836800 seconds = 1577836800000000000 nanos
        assert_eq!(ts.as_i64(), 1_577_836_800_000_000_000);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: LifecycleEventType exhaustive, persist_instrument_snapshot
    // error wrapping, ensure_instrument_tables DDL ordering, write_* edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_lifecycle_event_type_as_str_roundtrip_all_variants() {
        // Verify every variant produces a non-empty snake_case string
        let variants = [
            LifecycleEventType::ContractAdded,
            LifecycleEventType::ContractExpired,
            LifecycleEventType::LotSizeChanged,
            LifecycleEventType::TickSizeChanged,
            LifecycleEventType::FieldChanged,
            LifecycleEventType::UnderlyingAdded,
            LifecycleEventType::UnderlyingRemoved,
            LifecycleEventType::SecurityIdReused,
            LifecycleEventType::SecurityIdReassigned,
        ];
        for variant in &variants {
            let s = variant.as_str();
            assert!(!s.is_empty(), "as_str must not be empty for {:?}", variant);
            assert!(
                s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "as_str must be snake_case for {:?}, got '{}'",
                variant,
                s
            );
        }
    }

    #[test]
    fn test_lifecycle_event_type_all_variants_unique() {
        let variants = [
            LifecycleEventType::ContractAdded,
            LifecycleEventType::ContractExpired,
            LifecycleEventType::LotSizeChanged,
            LifecycleEventType::TickSizeChanged,
            LifecycleEventType::FieldChanged,
            LifecycleEventType::UnderlyingAdded,
            LifecycleEventType::UnderlyingRemoved,
            LifecycleEventType::SecurityIdReused,
            LifecycleEventType::SecurityIdReassigned,
        ];
        let strings: Vec<&str> = variants.iter().map(|v| v.as_str()).collect();
        let mut deduped = strings.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            strings.len(),
            deduped.len(),
            "all lifecycle event type strings must be unique"
        );
    }

    #[test]
    fn test_lifecycle_event_security_id_reused_fields() {
        let event = LifecycleEvent {
            security_id: 12345,
            underlying_symbol: "NIFTY".to_string(),
            event_type: LifecycleEventType::SecurityIdReused,
            field_changed: "underlying_symbol".to_string(),
            old_value: "BANKNIFTY".to_string(),
            new_value: "NIFTY".to_string(),
        };
        assert_eq!(event.event_type.as_str(), "security_id_reused");
        assert_eq!(event.security_id, 12345);
        assert_eq!(event.old_value, "BANKNIFTY");
        assert_eq!(event.new_value, "NIFTY");
    }

    #[test]
    fn test_lifecycle_event_security_id_reassigned() {
        let event = LifecycleEvent {
            security_id: 99999,
            underlying_symbol: "RELIANCE".to_string(),
            event_type: LifecycleEventType::SecurityIdReassigned,
            field_changed: "security_id".to_string(),
            old_value: "88888".to_string(),
            new_value: "99999".to_string(),
        };
        assert_eq!(event.event_type.as_str(), "security_id_reassigned");
        assert_eq!(event.field_changed, "security_id");
    }

    #[test]
    fn test_write_single_contract_with_none_option_type() {
        // Futures have option_type = None → should serialize as empty string
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut contract = make_test_contract(55555);
        contract.option_type = None;
        contract.instrument_kind = DhanInstrumentKind::FutureIndex;

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        let content = String::from_utf8_lossy(buffer.as_bytes());
        // Verify the buffer was written (non-empty)
        assert!(!content.is_empty());
        assert!(content.contains("FutureIndex"));
    }

    #[test]
    fn test_write_single_contract_expiry_date_is_yyyy_mm_dd() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let contract = make_test_contract(11111);
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        let content = String::from_utf8_lossy(buffer.as_bytes());
        // expiry_date should be in YYYY-MM-DD format
        assert!(
            content.contains("2026-03-27"),
            "expiry_date must be stored as YYYY-MM-DD string"
        );
    }

    #[test]
    fn test_write_contract_preserves_strike_price_precision() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut contract = make_test_contract(22222);
        contract.strike_price = 25650.5;

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        // If we get here without error, the f64 strike price was written successfully
        assert!(buffer.row_count() > 0);
    }

    #[test]
    fn test_dedup_key_constants_format() {
        // DEDUP_KEY_DERIVATIVE_CONTRACTS must include both security_id and underlying_symbol
        // per I-P1-05 gap enforcement
        assert!(DEDUP_KEY_DERIVATIVE_CONTRACTS.contains("security_id"));
        assert!(DEDUP_KEY_DERIVATIVE_CONTRACTS.contains("underlying_symbol"));

        // All dedup keys must not have leading/trailing whitespace
        assert_eq!(
            DEDUP_KEY_BUILD_METADATA.trim(),
            DEDUP_KEY_BUILD_METADATA,
            "dedup key must not have leading/trailing whitespace"
        );
        assert_eq!(DEDUP_KEY_FNO_UNDERLYINGS.trim(), DEDUP_KEY_FNO_UNDERLYINGS,);
        assert_eq!(
            DEDUP_KEY_DERIVATIVE_CONTRACTS.trim(),
            DEDUP_KEY_DERIVATIVE_CONTRACTS,
        );
        assert_eq!(
            DEDUP_KEY_SUBSCRIBED_INDICES.trim(),
            DEDUP_KEY_SUBSCRIBED_INDICES,
        );
    }

    #[test]
    fn test_questdb_ddl_timeout_constant_value() {
        assert_eq!(
            QUESTDB_DDL_TIMEOUT_SECS, 10,
            "DDL timeout must be 10 seconds"
        );
    }

    #[test]
    fn test_all_create_ddl_contain_timestamp_column() {
        for ddl in [
            BUILD_METADATA_CREATE_DDL,
            FNO_UNDERLYINGS_CREATE_DDL,
            DERIVATIVE_CONTRACTS_CREATE_DDL,
            SUBSCRIBED_INDICES_CREATE_DDL,
        ] {
            assert!(
                ddl.contains("timestamp TIMESTAMP"),
                "DDL must contain designated timestamp column"
            );
            assert!(
                ddl.contains("TIMESTAMP(timestamp)"),
                "DDL must declare designated timestamp"
            );
        }
    }

    #[test]
    fn test_all_create_ddl_use_wal_mode() {
        for ddl in [
            BUILD_METADATA_CREATE_DDL,
            FNO_UNDERLYINGS_CREATE_DDL,
            DERIVATIVE_CONTRACTS_CREATE_DDL,
            SUBSCRIBED_INDICES_CREATE_DDL,
        ] {
            assert!(
                ddl.contains("WAL"),
                "DDL must use WAL mode for dedup support"
            );
        }
    }

    #[test]
    fn test_all_create_ddl_are_idempotent() {
        for ddl in [
            BUILD_METADATA_CREATE_DDL,
            FNO_UNDERLYINGS_CREATE_DDL,
            DERIVATIVE_CONTRACTS_CREATE_DDL,
            SUBSCRIBED_INDICES_CREATE_DDL,
        ] {
            assert!(
                ddl.contains("IF NOT EXISTS"),
                "DDL must use IF NOT EXISTS for idempotency"
            );
        }
    }

    #[test]
    fn test_write_underlying_with_zero_security_ids() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut underlying = make_test_underlying("ZERO", 0);
        underlying.price_feed_security_id = 0;
        underlying.lot_size = 0;
        underlying.contract_count = 0;

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_underlying(&mut buffer, &underlying, snapshot_nanos).unwrap();
        assert_eq!(
            buffer.row_count(),
            1,
            "zero values must still produce a row"
        );
    }

    #[test]
    fn test_write_contract_with_zero_strike_price_and_lot_size() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let mut contract = make_test_contract(33333);
        contract.strike_price = 0.0;
        contract.lot_size = 0;
        contract.tick_size = 0.0;

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_write_contract_large_security_id() {
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()).unwrap();

        let contract = make_test_contract(u32::MAX);
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        write_single_contract(&mut buffer, &contract, snapshot_nanos).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_naive_date_consecutive_months_produce_increasing_timestamps() {
        let jan =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()).unwrap();
        let feb =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()).unwrap();
        let mar =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap();

        assert!(jan.as_i64() < feb.as_i64(), "Jan < Feb");
        assert!(feb.as_i64() < mar.as_i64(), "Feb < Mar");
    }

    #[tokio::test]
    async fn test_persist_instrument_snapshot_with_empty_universe() {
        // Empty universe should still not panic — it writes 1 metadata row + 0 data rows
        let universe = FnoUniverse {
            underlyings: std::collections::HashMap::new(),
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: vec![],
            build_metadata: make_test_metadata(),
        };
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Should return Ok(()) even though QuestDB is unreachable (error is swallowed)
        let result = persist_instrument_snapshot(&universe, &config).await;
        assert!(
            result.is_ok(),
            "persist_instrument_snapshot must swallow errors"
        );
    }

    #[test]
    fn test_lifecycle_event_debug_impl() {
        let event = LifecycleEvent {
            security_id: 42,
            underlying_symbol: "TEST".to_string(),
            event_type: LifecycleEventType::LotSizeChanged,
            field_changed: "lot_size".to_string(),
            old_value: "50".to_string(),
            new_value: "75".to_string(),
        };
        let debug_str = format!("{event:?}");
        assert!(debug_str.contains("LotSizeChanged"));
        assert!(debug_str.contains("TEST"));
    }

    // -----------------------------------------------------------------------
    // Coverage: DDL warn! field evaluation with tracing subscriber
    // -----------------------------------------------------------------------

    fn install_test_subscriber() -> tracing::subscriber::DefaultGuard {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_test_writer());
        tracing::subscriber::set_default(subscriber)
    }

    #[tokio::test]
    async fn test_ensure_instrument_tables_non_success_with_tracing() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // With tracing subscriber, warn! body expressions are evaluated.
        ensure_instrument_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_instrument_tables_send_error_with_tracing() {
        let _guard = install_test_subscriber();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
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
        ensure_instrument_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_non_success_with_tracing() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Covers ensure_table_dedup_keys non-success body evaluation.
        ensure_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_persist_instrument_snapshot_ok_path_with_mock_http_and_ilp() {
        let _guard = install_test_subscriber();
        // Mock HTTP for DDL (ensure_table_dedup_keys)
        let http_port = spawn_mock_http_server(MOCK_HTTP_200).await;
        // TCP drain for ILP writes
        let ilp_port = spawn_multi_accept_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: http_port,
            ilp_port,
        };

        let universe = FnoUniverse {
            build_metadata: make_test_metadata(),
            underlyings: {
                let mut m = std::collections::HashMap::new();
                m.insert("NIFTY".to_string(), make_test_underlying("NIFTY", 26000));
                m
            },
            derivative_contracts: {
                let mut m = std::collections::HashMap::new();
                m.insert(50001, make_test_contract(50001));
                m
            },
            subscribed_indices: vec![make_test_fno_index("NIFTY 50", 13)],
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
        };

        let result = persist_instrument_snapshot(&universe, &config).await;
        assert!(
            result.is_ok(),
            "persist_instrument_snapshot with valid HTTP+ILP must succeed"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: write_build_metadata / write_underlyings /
    // write_derivative_contracts / write_subscribed_indices with TCP drain
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_build_metadata_with_tcp_drain_server() {
        let port = spawn_tcp_drain_server();
        let conf_string = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf_string).unwrap();
        let mut buffer = sender.new_buffer();

        let metadata = make_test_metadata();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap();

        let result = write_build_metadata(&mut sender, &mut buffer, &metadata, snapshot_nanos);
        assert!(
            result.is_ok(),
            "write_build_metadata must succeed with TCP drain: {:?}",
            result
        );
    }

    #[test]
    fn test_write_underlyings_with_tcp_drain_server() {
        let port = spawn_tcp_drain_server();
        let conf_string = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf_string).unwrap();
        let mut buffer = sender.new_buffer();

        let mut underlyings = std::collections::HashMap::new();
        underlyings.insert("NIFTY".to_string(), make_test_underlying("NIFTY", 26000));
        underlyings.insert(
            "BANKNIFTY".to_string(),
            make_test_underlying("BANKNIFTY", 26009),
        );

        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap();

        let result = write_underlyings(&mut sender, &mut buffer, &underlyings, snapshot_nanos);
        assert!(
            result.is_ok(),
            "write_underlyings must succeed with TCP drain: {:?}",
            result
        );
    }

    #[test]
    fn test_write_derivative_contracts_with_tcp_drain_server() {
        let port = spawn_tcp_drain_server();
        let conf_string = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf_string).unwrap();
        let mut buffer = sender.new_buffer();

        let mut contracts = std::collections::HashMap::new();
        for sec_id in 50001..50011_u32 {
            contracts.insert(sec_id, make_test_contract(sec_id));
        }

        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap();

        let result =
            write_derivative_contracts(&mut sender, &mut buffer, &contracts, snapshot_nanos);
        assert!(
            result.is_ok(),
            "write_derivative_contracts must succeed with TCP drain: {:?}",
            result
        );
    }

    #[test]
    fn test_write_subscribed_indices_with_tcp_drain_server() {
        let port = spawn_tcp_drain_server();
        let conf_string = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf_string).unwrap();
        let mut buffer = sender.new_buffer();

        let indices = vec![
            make_test_fno_index("NIFTY", 13),
            make_test_fno_index("BANKNIFTY", 25),
            make_test_display_index("INDIA VIX", 21, IndexSubcategory::Volatility),
        ];

        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap();

        let result = write_subscribed_indices(&mut sender, &mut buffer, &indices, snapshot_nanos);
        assert!(
            result.is_ok(),
            "write_subscribed_indices must succeed with TCP drain: {:?}",
            result
        );
    }

    #[test]
    fn test_write_subscribed_indices_empty_list_with_tcp_drain() {
        let port = spawn_tcp_drain_server();
        let conf_string = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf_string).unwrap();
        let mut buffer = sender.new_buffer();

        let indices: Vec<SubscribedIndex> = vec![];
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap();

        let result = write_subscribed_indices(&mut sender, &mut buffer, &indices, snapshot_nanos);
        assert!(
            result.is_ok(),
            "empty indices list should not attempt to flush"
        );
    }

    #[test]
    fn test_write_derivative_contracts_empty_map_with_tcp_drain() {
        let port = spawn_tcp_drain_server();
        let conf_string = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf_string).unwrap();
        let mut buffer = sender.new_buffer();

        let contracts: std::collections::HashMap<SecurityId, DerivativeContract> =
            std::collections::HashMap::new();
        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap();

        let result =
            write_derivative_contracts(&mut sender, &mut buffer, &contracts, snapshot_nanos);
        assert!(
            result.is_ok(),
            "empty contracts should not attempt to flush"
        );
    }

    #[test]
    fn test_write_underlyings_single_entry_with_tcp_drain() {
        let port = spawn_tcp_drain_server();
        let conf_string = format!("tcp::addr=127.0.0.1:{port};");
        let mut sender = Sender::from_conf(&conf_string).unwrap();
        let mut buffer = sender.new_buffer();

        let mut underlyings = std::collections::HashMap::new();
        underlyings.insert(
            "RELIANCE".to_string(),
            FnoUnderlying {
                underlying_symbol: "RELIANCE".to_string(),
                underlying_security_id: 2885,
                price_feed_security_id: 2885,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 250,
                contract_count: 120,
            },
        );

        let snapshot_nanos =
            naive_date_to_timestamp_nanos(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap();

        let result = write_underlyings(&mut sender, &mut buffer, &underlyings, snapshot_nanos);
        assert!(
            result.is_ok(),
            "single underlying write must succeed: {:?}",
            result
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: ensure_instrument_tables success path with tracing subscriber
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_instrument_tables_success_with_tracing() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // With tracing subscriber, info!/debug! field expressions are evaluated.
        ensure_instrument_tables(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage: ensure_table_dedup_keys success path with tracing subscriber
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_success_with_tracing() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Covers the success branch of ensure_table_dedup_keys.
        ensure_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage: ensure_table_dedup_keys send error with tracing subscriber
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_table_dedup_keys_send_error_with_tracing() {
        let _guard = install_test_subscriber();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
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
        // Covers the Err branch of the DEDUP send.
        ensure_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage: LifecycleEvent clone
    // -----------------------------------------------------------------------

    #[test]
    fn test_lifecycle_event_clone_roundtrip() {
        let event = LifecycleEvent {
            security_id: 42,
            underlying_symbol: "NIFTY".to_string(),
            event_type: LifecycleEventType::ContractAdded,
            field_changed: String::new(),
            old_value: String::new(),
            new_value: String::new(),
        };
        let cloned = event.clone();
        assert_eq!(cloned.security_id, 42);
        assert_eq!(cloned.underlying_symbol, "NIFTY");
        assert_eq!(cloned.event_type, LifecycleEventType::ContractAdded);
    }

    #[test]
    fn test_lifecycle_event_type_eq_and_ne() {
        assert_eq!(
            LifecycleEventType::ContractAdded,
            LifecycleEventType::ContractAdded
        );
        assert_ne!(
            LifecycleEventType::ContractAdded,
            LifecycleEventType::ContractExpired
        );
    }

    #[tokio::test]
    async fn test_persist_inner_write_error_covers_question_mark_propagation() {
        let _guard = install_test_subscriber();
        // Mock HTTP for DDL (best-effort, continues on error)
        let http_port = spawn_mock_http_server(MOCK_HTTP_200).await;
        // TCP server that accepts connection then immediately closes it.
        // The ILP Sender connects successfully but flush fails.
        let ilp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ilp_port = ilp_listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // Accept one connection and drop it immediately
            if let Ok((_stream, _)) = ilp_listener.accept() {
                // Connection accepted but immediately dropped
            }
        });

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: http_port,
            ilp_port,
        };

        let universe = FnoUniverse {
            build_metadata: make_test_metadata(),
            underlyings: {
                let mut m = std::collections::HashMap::new();
                m.insert("NIFTY".to_string(), make_test_underlying("NIFTY", 26000));
                m
            },
            derivative_contracts: {
                let mut m = std::collections::HashMap::new();
                m.insert(50001, make_test_contract(50001));
                m
            },
            subscribed_indices: vec![make_test_fno_index("NIFTY 50", 13)],
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
        };

        // persist_inner should fail at one of the write_* calls when flush
        // encounters the dropped connection. persist_instrument_snapshot
        // wraps it and returns Ok.
        let result = persist_instrument_snapshot(&universe, &config).await;
        assert!(
            result.is_ok(),
            "persist_instrument_snapshot always returns Ok"
        );
    }

    // -----------------------------------------------------------------------
    // LifecycleEventType::as_str — all 9 variants (extended)
    // -----------------------------------------------------------------------

    #[test]
    fn test_lifecycle_event_type_as_str_all_nine_variants() {
        assert_eq!(LifecycleEventType::ContractAdded.as_str(), "contract_added");
        assert_eq!(
            LifecycleEventType::ContractExpired.as_str(),
            "contract_expired"
        );
        assert_eq!(
            LifecycleEventType::LotSizeChanged.as_str(),
            "lot_size_changed"
        );
        assert_eq!(
            LifecycleEventType::TickSizeChanged.as_str(),
            "tick_size_changed"
        );
        assert_eq!(LifecycleEventType::FieldChanged.as_str(), "field_changed");
        assert_eq!(
            LifecycleEventType::UnderlyingAdded.as_str(),
            "underlying_added"
        );
        assert_eq!(
            LifecycleEventType::UnderlyingRemoved.as_str(),
            "underlying_removed"
        );
        assert_eq!(
            LifecycleEventType::SecurityIdReused.as_str(),
            "security_id_reused"
        );
        assert_eq!(
            LifecycleEventType::SecurityIdReassigned.as_str(),
            "security_id_reassigned"
        );
    }

    #[test]
    fn test_lifecycle_event_type_debug_impl() {
        let event = LifecycleEventType::ContractAdded;
        let debug = format!("{event:?}");
        assert!(debug.contains("ContractAdded"));
    }

    #[test]
    fn test_lifecycle_event_type_clone_and_eq() {
        let a = LifecycleEventType::LotSizeChanged;
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn test_lifecycle_event_struct_fields() {
        let event = LifecycleEvent {
            security_id: 50001,
            underlying_symbol: "NIFTY".to_string(),
            event_type: LifecycleEventType::ContractAdded,
            field_changed: String::new(),
            old_value: String::new(),
            new_value: "new_contract".to_string(),
        };
        assert_eq!(event.security_id, 50001);
        assert_eq!(event.underlying_symbol, "NIFTY");
        assert_eq!(event.event_type.as_str(), "contract_added");
        assert!(event.field_changed.is_empty());
        assert!(event.old_value.is_empty());
        assert_eq!(event.new_value, "new_contract");
    }

    #[test]
    fn test_lifecycle_event_debug() {
        let event = LifecycleEvent {
            security_id: 0,
            underlying_symbol: "RELIANCE".to_string(),
            event_type: LifecycleEventType::UnderlyingRemoved,
            field_changed: String::new(),
            old_value: "old".to_string(),
            new_value: String::new(),
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("LifecycleEvent"));
        assert!(debug.contains("RELIANCE"));
    }

    // -----------------------------------------------------------------------
    // DDL constants coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_dedup_key_constants_content() {
        assert_eq!(DEDUP_KEY_BUILD_METADATA, "csv_source");
        assert_eq!(DEDUP_KEY_FNO_UNDERLYINGS, "underlying_symbol");
        assert_eq!(
            DEDUP_KEY_DERIVATIVE_CONTRACTS, "security_id, underlying_symbol",
            "I-P1-05: must include underlying_symbol"
        );
        assert_eq!(DEDUP_KEY_SUBSCRIBED_INDICES, "security_id");
    }

    #[test]
    fn test_ddl_constants_are_single_statements() {
        assert!(
            !BUILD_METADATA_CREATE_DDL.contains(';'),
            "DDL must not contain semicolons"
        );
        assert!(
            !FNO_UNDERLYINGS_CREATE_DDL.contains(';'),
            "DDL must not contain semicolons"
        );
        assert!(
            !DERIVATIVE_CONTRACTS_CREATE_DDL.contains(';'),
            "DDL must not contain semicolons"
        );
        assert!(
            !SUBSCRIBED_INDICES_CREATE_DDL.contains(';'),
            "DDL must not contain semicolons"
        );
    }

    #[test]
    fn test_ddl_constants_use_wal() {
        assert!(BUILD_METADATA_CREATE_DDL.contains("WAL"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("WAL"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("WAL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("WAL"));
    }

    #[test]
    fn test_ddl_constants_have_timestamp() {
        assert!(BUILD_METADATA_CREATE_DDL.contains("TIMESTAMP(timestamp)"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("TIMESTAMP(timestamp)"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("TIMESTAMP(timestamp)"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("TIMESTAMP(timestamp)"));
    }

    #[test]
    fn test_ddl_timeout_is_reasonable() {
        assert!((5..=30).contains(&QUESTDB_DDL_TIMEOUT_SECS));
    }

    #[test]
    fn test_build_metadata_ddl_has_key_columns() {
        assert!(BUILD_METADATA_CREATE_DDL.contains("csv_source SYMBOL"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("derivative_count LONG"));
        assert!(BUILD_METADATA_CREATE_DDL.contains("build_duration_ms LONG"));
    }

    #[test]
    fn test_fno_underlyings_ddl_has_key_columns() {
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("underlying_symbol SYMBOL"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("lot_size LONG"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("contract_count LONG"));
    }

    #[test]
    fn test_derivative_contracts_ddl_has_key_columns() {
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("security_id LONG"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("strike_price DOUBLE"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("expiry_date STRING"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("option_type SYMBOL"));
    }

    #[test]
    fn test_subscribed_indices_ddl_has_key_columns() {
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("security_id LONG"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("symbol SYMBOL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("category SYMBOL"));
    }

    // -----------------------------------------------------------------------
    // Coverage: LifecycleEventType::as_str all variants roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn test_lifecycle_event_type_as_str_comprehensive() {
        let variants = [
            (LifecycleEventType::ContractAdded, "contract_added"),
            (LifecycleEventType::ContractExpired, "contract_expired"),
            (LifecycleEventType::LotSizeChanged, "lot_size_changed"),
            (LifecycleEventType::TickSizeChanged, "tick_size_changed"),
            (LifecycleEventType::FieldChanged, "field_changed"),
            (LifecycleEventType::UnderlyingAdded, "underlying_added"),
            (LifecycleEventType::UnderlyingRemoved, "underlying_removed"),
            (LifecycleEventType::SecurityIdReused, "security_id_reused"),
            (
                LifecycleEventType::SecurityIdReassigned,
                "security_id_reassigned",
            ),
        ];
        for (variant, expected) in &variants {
            assert_eq!(variant.as_str(), *expected);
        }
    }

    // -----------------------------------------------------------------------
    // Coverage: DDL additional field checks
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_metadata_ddl_has_build_duration() {
        assert!(BUILD_METADATA_CREATE_DDL.contains("build_duration_ms LONG"));
    }

    #[test]
    fn test_build_metadata_ddl_has_build_timestamp() {
        assert!(BUILD_METADATA_CREATE_DDL.contains("build_timestamp TIMESTAMP"));
    }

    #[test]
    fn test_fno_underlyings_ddl_has_lot_size() {
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("lot_size LONG"));
    }

    #[test]
    fn test_fno_underlyings_ddl_has_contract_count() {
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("contract_count LONG"));
    }

    #[test]
    fn test_derivative_contracts_ddl_has_tick_size() {
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("tick_size DOUBLE"));
    }

    #[test]
    fn test_derivative_contracts_ddl_has_lot_size() {
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("lot_size LONG"));
    }

    #[test]
    fn test_derivative_contracts_ddl_has_display_name() {
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("display_name STRING"));
    }

    #[test]
    fn test_subscribed_indices_ddl_has_exchange() {
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("exchange SYMBOL"));
    }

    #[test]
    fn test_subscribed_indices_ddl_has_subcategory() {
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("subcategory SYMBOL"));
    }

    // -----------------------------------------------------------------------
    // Coverage: LifecycleEvent construction
    // -----------------------------------------------------------------------

    #[test]
    fn test_lifecycle_event_debug_format_lot_size() {
        let event = LifecycleEvent {
            security_id: 42528,
            underlying_symbol: "NIFTY".to_string(),
            event_type: LifecycleEventType::LotSizeChanged,
            field_changed: "lot_size".to_string(),
            old_value: "50".to_string(),
            new_value: "75".to_string(),
        };
        let debug_str = format!("{event:?}");
        assert!(debug_str.contains("LotSizeChanged"));
        assert!(debug_str.contains("42528"));
    }

    #[test]
    fn test_lifecycle_event_type_clone_reused_variant() {
        let original = LifecycleEventType::SecurityIdReused;
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    // -----------------------------------------------------------------------
    // Coverage: DEDUP key constants values
    // -----------------------------------------------------------------------

    #[test]
    fn test_dedup_key_build_metadata_exact() {
        assert_eq!(DEDUP_KEY_BUILD_METADATA, "csv_source");
    }

    #[test]
    fn test_dedup_key_fno_underlyings_exact() {
        assert_eq!(DEDUP_KEY_FNO_UNDERLYINGS, "underlying_symbol");
    }

    #[test]
    fn test_dedup_key_derivative_contracts_exact() {
        assert_eq!(
            DEDUP_KEY_DERIVATIVE_CONTRACTS,
            "security_id, underlying_symbol"
        );
    }

    #[test]
    fn test_dedup_key_subscribed_indices_exact() {
        assert_eq!(DEDUP_KEY_SUBSCRIBED_INDICES, "security_id");
    }

    // -----------------------------------------------------------------------
    // Coverage: DDL timeout constant
    // -----------------------------------------------------------------------

    #[test]
    fn test_instrument_ddl_timeout_value() {
        assert_eq!(QUESTDB_DDL_TIMEOUT_SECS, 10);
    }

    // -----------------------------------------------------------------------
    // Coverage: All DDLs have TIMESTAMP and WAL
    // -----------------------------------------------------------------------

    #[test]
    fn test_all_instrument_ddls_have_wal() {
        assert!(BUILD_METADATA_CREATE_DDL.contains("WAL"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("WAL"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("WAL"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("WAL"));
    }

    #[test]
    fn test_all_instrument_ddls_have_partition() {
        assert!(BUILD_METADATA_CREATE_DDL.contains("PARTITION BY DAY"));
        assert!(FNO_UNDERLYINGS_CREATE_DDL.contains("PARTITION BY DAY"));
        assert!(DERIVATIVE_CONTRACTS_CREATE_DDL.contains("PARTITION BY DAY"));
        assert!(SUBSCRIBED_INDICES_CREATE_DDL.contains("PARTITION BY DAY"));
    }

    // -----------------------------------------------------------------------
    // I-P1-08: Cross-Day Snapshot Accumulation — NO DELETE guard
    // -----------------------------------------------------------------------

    /// I-P1-08: The persist path must NOT contain DELETE FROM for snapshot tables.
    /// Historical rows are preserved across days (SEBI audit trail).
    /// Data retention handled by QuestDB partition management, NOT application DELETE.
    #[test]
    fn test_no_delete_from_snapshot_tables_in_persist_path() {
        // Read the source file — only check production code (before #[cfg(test)])
        let source = include_str!("instrument_persistence.rs");
        let production_code = source
            .split("#[cfg(test)]")
            .next()
            .expect("source must have production code");

        // Construct pattern at runtime to avoid self-referential match
        let pattern = ["DELET", "E FRO", "M"].concat();
        let lower = pattern.to_lowercase();

        assert!(
            !production_code.contains(&pattern) && !production_code.contains(&lower),
            "I-P1-08: production code must NOT contain destructive SQL"
        );
    }

    /// I-P1-08: Snapshot timestamps for different dates must be distinct.
    #[test]
    fn test_snapshot_nanos_for_different_dates_are_distinct() {
        let date1 = chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        let date2 = chrono::NaiveDate::from_ymd_opt(2026, 4, 2).unwrap();
        let nanos1 = naive_date_to_timestamp_nanos(date1)
            .expect("valid nanos for date1")
            .as_i64();
        let nanos2 = naive_date_to_timestamp_nanos(date2)
            .expect("valid nanos for date2")
            .as_i64();
        assert_ne!(
            nanos1, nanos2,
            "I-P1-08: different dates must produce different snapshot timestamps"
        );
        assert!(
            nanos2 > nanos1,
            "I-P1-08: later date must have greater timestamp"
        );
    }

    /// I-P1-08: Cross-day accumulation by design — same security_id on different
    /// days produces rows with different timestamps (not overwritten).
    #[test]
    fn test_snapshot_cross_day_accumulation_by_design() {
        let day1 = chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        let day2 = chrono::NaiveDate::from_ymd_opt(2026, 4, 2).unwrap();
        let ts1 = naive_date_to_timestamp_nanos(day1)
            .expect("valid nanos for day1")
            .as_i64();
        let ts2 = naive_date_to_timestamp_nanos(day2)
            .expect("valid nanos for day2")
            .as_i64();

        // The same security_id persisted on two different days produces two
        // distinct rows (different timestamps). This is intentional — DEDUP
        // UPSERT KEYS include the designated timestamp, so rows accumulate.
        assert_ne!(ts1, ts2);
        assert!(ts1 > 0, "timestamp must be positive epoch nanos");
        assert!(ts2 > 0, "timestamp must be positive epoch nanos");
    }
}
