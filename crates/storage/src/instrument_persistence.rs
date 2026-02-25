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
//! # Idempotency Warning
//!
//! This function MUST be called at most once per calendar day (IST).
//! Calling it twice on the same day will insert duplicate rows because
//! QuestDB ILP auto-created tables have no deduplication keys configured.
//! The caller is responsible for ensuring single invocation per day.
//!
//! # Query Notes for Downstream Consumers
//!
//! - `expiry_date` is stored as a STRING in `YYYY-MM-DD` format, not a TIMESTAMP.
//! - Futures have `option_type = ''` (empty string, not NULL) and `strike_price = 0.0`.
//! - The designated timestamp (`timestamp` column) is IST midnight stored as UTC.
//!   QuestDB displays it in UTC (e.g., `2026-02-24T18:30:00Z` = `2026-02-25 00:00:00 IST`).
//!   Grafana dashboards should set timezone to IST for correct display.

use anyhow::{Context, Result};
use chrono::{FixedOffset, NaiveDate, Utc};
use questdb::ingress::{Buffer, Sender, TimestampMicros, TimestampNanos};
use tracing::{info, warn};

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
// Public API
// ---------------------------------------------------------------------------

/// Persists a daily instrument snapshot to QuestDB via ILP.
///
/// Writes build metadata, F&O underlyings, and all derivative contracts.
/// On failure, logs a warning and returns `Ok(())` — trading is not blocked.
pub fn persist_instrument_snapshot(
    universe: &FnoUniverse,
    questdb_config: &QuestDbConfig,
) -> Result<()> {
    match persist_inner(universe, questdb_config) {
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

// ---------------------------------------------------------------------------
// Internal Implementation
// ---------------------------------------------------------------------------

/// Inner persistence logic that propagates errors for the outer wrapper to catch.
fn persist_inner(universe: &FnoUniverse, questdb_config: &QuestDbConfig) -> Result<()> {
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
}
