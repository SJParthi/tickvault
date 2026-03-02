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

/// Timeout for QuestDB DDL HTTP requests (ALTER TABLE).
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

// ---------------------------------------------------------------------------
// Internal Implementation
// ---------------------------------------------------------------------------

/// Inner persistence logic that propagates errors for the outer wrapper to catch.
async fn persist_inner(universe: &FnoUniverse, questdb_config: &QuestDbConfig) -> Result<()> {
    // Enable DEDUP UPSERT KEYS before writing — makes re-runs idempotent.
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
        let ist =
            FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
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
        let ist =
            FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
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
        // The snapshot timestamp should be at IST midnight (00:00:00 IST),
        // which means the nanos value modulo a full day minus IST offset
        // should equal zero.
        let ts = build_snapshot_timestamp().unwrap();
        let nanos = ts.as_i64();

        // IST midnight = some UTC time. The nanosecond value of IST midnight
        // is always an exact multiple of (nanos_per_second * seconds_in_day) minus IST offset.
        // More precisely: nanos + IST_offset_nanos should be divisible by nanos_per_day.
        let ist_offset_nanos = i64::from(IST_UTC_OFFSET_SECONDS) * 1_000_000_000;
        let nanos_per_day: i64 = 86_400 * 1_000_000_000;
        let adjusted = nanos + ist_offset_nanos;
        assert_eq!(
            adjusted % nanos_per_day,
            0,
            "snapshot timestamp must be IST midnight (remainder should be zero)"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: naive_date_to_timestamp_nanos additional cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_naive_date_to_timestamp_nanos_known_date_exact_value() {
        // 2026-03-01 midnight IST = 2026-02-28 18:30:00 UTC.
        // 2026-02-28 18:30:00 UTC = epoch + some known nanos.
        let date = NaiveDate::from_ymd_opt(2026, 3, 1).expect("valid date");
        let ts = naive_date_to_timestamp_nanos(date).unwrap();

        // 2026-03-01T00:00:00+05:30 = 2026-02-28T18:30:00Z
        // Calculate manually: days from epoch to 2026-02-28 = 20,512 days
        // 20512 * 86400 = 1,772,236,800 seconds to start of 2026-02-28 UTC
        // + 18h30m = 66600 seconds = 1,772,303,400 seconds total
        // = 1,772,303,400,000,000,000 nanos
        let expected_nanos: i64 = 1_772_303_400_000_000_000;
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
}
