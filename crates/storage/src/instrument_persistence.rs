//! QuestDB ILP persistence for daily instrument snapshots.
//!
//! After `build_fno_universe()` produces the in-memory universe, this module
//! writes a daily snapshot to QuestDB via the InfluxDB Line Protocol (ILP).
//!
//! Three tables are written:
//! - `instrument_build_metadata` — 1 row per build (health and statistics)
//! - `fno_underlyings` — ~215 rows per day (underlying reference data)
//! - `derivative_contracts` — ~150K rows per day (contract → security_id mapping)
//!
//! # Error Handling
//!
//! QuestDB persistence is observability data, NOT critical path.
//! If writes fail, the system logs a WARN and continues trading.

use anyhow::{Context, Result};
use chrono::{FixedOffset, NaiveDate, Utc};
use questdb::ingress::{Buffer, Sender, TimestampMicros, TimestampNanos};
use tracing::{info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::{
    ILP_FLUSH_BATCH_SIZE, IST_UTC_OFFSET_SECONDS, QUESTDB_TABLE_BUILD_METADATA,
    QUESTDB_TABLE_DERIVATIVE_CONTRACTS, QUESTDB_TABLE_FNO_UNDERLYINGS,
};
use dhan_live_trader_common::instrument_types::{
    DerivativeContract, FnoUnderlying, FnoUniverse, UniverseBuildMetadata,
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

    Ok(())
}

/// Builds the designated timestamp for the snapshot: today's IST date at midnight.
///
/// All rows for a single day share the same designated timestamp, making
/// date-based queries clean (e.g., `WHERE snapshot_date = '2026-03-15'`).
fn build_snapshot_timestamp() -> Result<TimestampNanos> {
    let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
    let today_ist = Utc::now().with_timezone(&ist).date_naive();
    naive_date_to_timestamp_nanos(today_ist)
}

/// Converts a `NaiveDate` to `TimestampNanos` at midnight IST.
fn naive_date_to_timestamp_nanos(date: NaiveDate) -> Result<TimestampNanos> {
    let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight is always valid");
    let ist_datetime = midnight
        .and_local_timezone(ist)
        .single()
        .expect("IST has no DST ambiguity");
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
        .column_i64("csv_row_count", metadata.csv_row_count as i64)
        .context("csv_row_count")?
        .column_i64("parsed_row_count", metadata.parsed_row_count as i64)
        .context("parsed_row_count")?
        .column_i64("index_count", metadata.index_count as i64)
        .context("index_count")?
        .column_i64("equity_count", metadata.equity_count as i64)
        .context("equity_count")?
        .column_i64("underlying_count", metadata.underlying_count as i64)
        .context("underlying_count")?
        .column_i64("derivative_count", metadata.derivative_count as i64)
        .context("derivative_count")?
        .column_i64("option_chain_count", metadata.option_chain_count as i64)
        .context("option_chain_count")?
        .column_i64(
            "build_duration_ms",
            metadata.build_duration.as_millis() as i64,
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
        .column_i64("contract_count", underlying.contract_count as i64)
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
    let expiry_nanos = naive_date_to_timestamp_nanos(contract.expiry_date)?;
    let option_type_str = contract
        .option_type
        .as_ref()
        .map(|ot| ot.as_str())
        .unwrap_or("");

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
        .column_ts(
            "expiry_date",
            TimestampMicros::new(expiry_nanos.as_i64() / 1000),
        )
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};
    use dhan_live_trader_common::instrument_types::{
        DhanInstrumentKind, UnderlyingKind, UniverseBuildMetadata,
    };
    use dhan_live_trader_common::types::{ExchangeSegment, OptionType};
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
            .column_i64("csv_row_count", metadata.csv_row_count as i64)
            .unwrap()
            .column_i64("parsed_row_count", metadata.parsed_row_count as i64)
            .unwrap()
            .column_i64("index_count", metadata.index_count as i64)
            .unwrap()
            .column_i64("equity_count", metadata.equity_count as i64)
            .unwrap()
            .column_i64("underlying_count", metadata.underlying_count as i64)
            .unwrap()
            .column_i64("derivative_count", metadata.derivative_count as i64)
            .unwrap()
            .column_i64("option_chain_count", metadata.option_chain_count as i64)
            .unwrap()
            .column_i64(
                "build_duration_ms",
                metadata.build_duration.as_millis() as i64,
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
}
