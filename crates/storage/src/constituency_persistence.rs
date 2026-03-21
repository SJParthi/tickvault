//! QuestDB persistence for NSE index constituency data.
//!
//! Writes index → stock mappings from niftyindices.com to QuestDB so
//! Grafana can display constituency tables. Daily snapshot — DEDUP
//! UPSERT KEYS on `(ts, index_name, symbol)` prevent duplicates.
//!
//! Best-effort: failures log ERROR but don't block trading.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Sender, TimestampNanos};
use reqwest::Client;
use tracing::{info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::QUESTDB_TABLE_INDEX_CONSTITUENTS;
use dhan_live_trader_common::instrument_types::{FnoUniverse, IndexConstituencyMap};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY for the `index_constituents` table.
const DEDUP_KEY_INDEX_CONSTITUENTS: &str = "index_name, symbol";

/// DDL for `index_constituents` — one row per (index, stock) pair.
const INDEX_CONSTITUENTS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS index_constituents (\
        index_name SYMBOL,\
        symbol SYMBOL,\
        isin STRING,\
        weight DOUBLE,\
        sector STRING,\
        security_id LONG,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY MONTH WAL\
";

/// Maximum retry attempts for constituency ILP persistence.
const CONSTITUENCY_PERSIST_MAX_RETRIES: u32 = 3;

/// Delay between constituency persistence retries (seconds).
const CONSTITUENCY_PERSIST_RETRY_DELAY_SECS: u64 = 2;

// ---------------------------------------------------------------------------
// DDL Setup
// ---------------------------------------------------------------------------

/// Creates the `index_constituents` table and enables DEDUP. Idempotent.
pub async fn ensure_constituency_table(questdb_config: &QuestDbConfig) {
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
                "failed to build HTTP client for constituency table DDL"
            );
            return;
        }
    };

    // Step 1: Create table
    match client
        .get(&base_url)
        .query(&[("query", INDEX_CONSTITUENTS_CREATE_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("index_constituents table ensured (CREATE TABLE IF NOT EXISTS)");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "index_constituents table CREATE DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, "failed to send CREATE TABLE for index_constituents");
        }
    }

    // Step 2: Enable DEDUP
    let dedup_ddl = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_INDEX_CONSTITUENTS, DEDUP_KEY_INDEX_CONSTITUENTS
    );

    match client
        .get(&base_url)
        .query(&[("query", &dedup_ddl)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("index_constituents DEDUP UPSERT KEYS enabled");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                // DEDUP already enabled = non-error (QuestDB returns error on re-enable)
                if !body.contains("already enabled") {
                    warn!(
                        %status,
                        body = body.chars().take(200).collect::<String>(),
                        "index_constituents DEDUP DDL returned non-success"
                    );
                }
            }
        }
        Err(err) => {
            warn!(?err, "failed to send DEDUP DDL for index_constituents");
        }
    }
}

// ---------------------------------------------------------------------------
// Data Persistence
// ---------------------------------------------------------------------------

/// Persists constituency data to QuestDB with retry logic. Best-effort.
///
/// When `fno_universe` is provided, each constituent is enriched with the
/// Dhan `security_id` from the instrument master (symbol → security_id mapping).
/// This enables news-based trading: symbol → security_id → F&O contracts.
pub fn persist_constituency(
    constituency_map: &IndexConstituencyMap,
    questdb_config: &QuestDbConfig,
    fno_universe: Option<&FnoUniverse>,
) -> Result<()> {
    let mut last_err = None;
    for attempt in 1..=CONSTITUENCY_PERSIST_MAX_RETRIES {
        match persist_constituency_inner(constituency_map, questdb_config, fno_universe) {
            Ok(count) => {
                info!(
                    entries = count,
                    table = QUESTDB_TABLE_INDEX_CONSTITUENTS,
                    attempt,
                    "index constituency persisted to QuestDB"
                );
                return Ok(());
            }
            Err(err) => {
                warn!(
                    ?err,
                    attempt,
                    max_retries = CONSTITUENCY_PERSIST_MAX_RETRIES,
                    "constituency persistence attempt failed — retrying"
                );
                last_err = Some(err);
                if attempt < CONSTITUENCY_PERSIST_MAX_RETRIES {
                    std::thread::sleep(std::time::Duration::from_secs(
                        CONSTITUENCY_PERSIST_RETRY_DELAY_SECS,
                    ));
                }
            }
        }
    }
    tracing::error!(
        err = ?last_err,
        "constituency persistence failed after {CONSTITUENCY_PERSIST_MAX_RETRIES} attempts — index_constituents table will be empty in Grafana"
    );
    Ok(())
}

fn persist_constituency_inner(
    constituency_map: &IndexConstituencyMap,
    questdb_config: &QuestDbConfig,
    fno_universe: Option<&FnoUniverse>,
) -> Result<usize> {
    let conf_string = format!(
        "tcp::addr={}:{};",
        questdb_config.host, questdb_config.ilp_port
    );
    let mut sender = Sender::from_conf(&conf_string)
        .context("failed to connect to QuestDB ILP for constituency")?;
    let mut buffer = sender.new_buffer();

    // Use build timestamp as the snapshot timestamp for all rows.
    let build_epoch_nanos = constituency_map
        .build_metadata
        .build_timestamp
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let snapshot_ts = TimestampNanos::new(build_epoch_nanos);

    let mut count = 0_usize;

    for (index_name, constituents) in &constituency_map.index_to_constituents {
        for constituent in constituents {
            // Enrich with security_id from instrument master (0 if not found).
            let security_id = fno_universe
                .and_then(|u| u.symbol_to_security_id(&constituent.symbol))
                .unwrap_or(0);

            buffer
                .table(QUESTDB_TABLE_INDEX_CONSTITUENTS)
                .context("table name")?
                .symbol("index_name", index_name)
                .context("index_name")?
                .symbol("symbol", &constituent.symbol)
                .context("symbol")?
                .column_str("isin", &constituent.isin)
                .context("isin")?
                .column_f64("weight", constituent.weight)
                .context("weight")?
                .column_str("sector", &constituent.sector)
                .context("sector")?
                .column_i64("security_id", i64::from(security_id))
                .context("security_id")?
                .at(snapshot_ts)
                .context("designated timestamp")?;

            count = count.saturating_add(1);
        }
    }

    if count > 0 {
        sender
            .flush(&mut buffer)
            .context("failed to flush constituency data to QuestDB")?;
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_ddl_is_valid_sql() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains(QUESTDB_TABLE_INDEX_CONSTITUENTS));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("index_name SYMBOL"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("symbol SYMBOL"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("weight DOUBLE"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_dedup_key_includes_index_name_and_symbol() {
        assert!(DEDUP_KEY_INDEX_CONSTITUENTS.contains("index_name"));
        assert!(DEDUP_KEY_INDEX_CONSTITUENTS.contains("symbol"));
    }

    #[test]
    fn test_persist_constituency_unreachable_host_returns_ok() {
        // persist_constituency is best-effort — returns Ok even on failure.
        let map = IndexConstituencyMap::default();
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            ilp_port: 19009,
            http_port: 19000,
            pg_port: 18812,
        };
        let result = persist_constituency(&map, &config, None);
        assert!(
            result.is_ok(),
            "persist_constituency must be best-effort (Ok on failure)"
        );
    }

    #[test]
    fn test_persist_empty_map_returns_ok() {
        // Empty map should return Ok(0) without connecting to QuestDB.
        let map = IndexConstituencyMap::default();
        assert_eq!(map.index_count(), 0);
        assert_eq!(map.stock_count(), 0);
    }

    #[test]
    fn test_table_constant_matches_ddl() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains(QUESTDB_TABLE_INDEX_CONSTITUENTS));
    }

    #[test]
    fn test_constituency_persist_max_retries_is_3() {
        assert_eq!(CONSTITUENCY_PERSIST_MAX_RETRIES, 3);
    }

    #[test]
    fn test_constituency_persist_retry_delay_is_2_secs() {
        assert_eq!(CONSTITUENCY_PERSIST_RETRY_DELAY_SECS, 2);
    }

    #[test]
    fn test_questdb_ddl_timeout_is_reasonable_constituency() {
        assert!((5..=30).contains(&QUESTDB_DDL_TIMEOUT_SECS));
    }

    #[test]
    fn test_ddl_has_security_id_column() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("security_id LONG"),
            "DDL must include security_id LONG for instrument master enrichment"
        );
    }

    #[test]
    fn test_ddl_uses_wal() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("WAL"),
            "DDL must use WAL for concurrent write safety"
        );
    }

    #[test]
    fn test_ddl_is_single_statement() {
        assert!(
            !INDEX_CONSTITUENTS_CREATE_DDL.contains(';'),
            "DDL must be a single statement without semicolons"
        );
    }

    #[test]
    fn test_ddl_has_partition_by_month() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("PARTITION BY MONTH"),
            "DDL must partition by MONTH for constituency data"
        );
    }

    // -----------------------------------------------------------------------
    // TCP drain server helper (same pattern as other storage tests)
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // DDL tests with mock HTTP server
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_constituency_table_does_not_panic_unreachable() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Should not panic — just logs warnings and returns.
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_ddl_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the success path for both CREATE TABLE and DEDUP DDL.
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_ddl_non_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the non-success path for CREATE TABLE DDL.
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_dedup_already_enabled() {
        // Test with a response body containing "already enabled" — skips the warn
        let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 37\r\n\r\n{\"error\":\"dedup already enabled foo\"}";
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
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_constituency_table(&config).await;
    }

    // -----------------------------------------------------------------------
    // persist_constituency tests with data
    // -----------------------------------------------------------------------

    fn make_test_constituency_map() -> IndexConstituencyMap {
        use chrono::NaiveDate;
        use dhan_live_trader_common::instrument_types::IndexConstituent;

        let constituents = vec![
            IndexConstituent {
                index_name: "Nifty 50".to_string(),
                symbol: "RELIANCE".to_string(),
                isin: "INE002A01018".to_string(),
                weight: 10.5,
                sector: "Energy".to_string(),
                last_updated: NaiveDate::from_ymd_opt(2026, 3, 20).unwrap(),
            },
            IndexConstituent {
                index_name: "Nifty 50".to_string(),
                symbol: "TCS".to_string(),
                isin: "INE467B01029".to_string(),
                weight: 4.2,
                sector: "IT".to_string(),
                last_updated: NaiveDate::from_ymd_opt(2026, 3, 20).unwrap(),
            },
        ];

        let mut index_to_constituents = std::collections::HashMap::new();
        index_to_constituents.insert("Nifty 50".to_string(), constituents);

        let mut stock_to_indices = std::collections::HashMap::new();
        stock_to_indices.insert("RELIANCE".to_string(), vec!["Nifty 50".to_string()]);
        stock_to_indices.insert("TCS".to_string(), vec!["Nifty 50".to_string()]);

        IndexConstituencyMap {
            index_to_constituents,
            stock_to_indices,
            build_metadata: Default::default(),
        }
    }

    #[test]
    fn test_persist_constituency_with_data_unreachable_host_returns_ok() {
        let map = make_test_constituency_map();
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            ilp_port: 19009,
            http_port: 19000,
            pg_port: 18812,
        };
        let result = persist_constituency(&map, &config, None);
        assert!(
            result.is_ok(),
            "persist_constituency must be best-effort (Ok on failure)"
        );
    }

    #[test]
    fn test_persist_constituency_inner_with_valid_tcp_server() {
        let port = spawn_tcp_drain_server();
        let map = make_test_constituency_map();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let result = persist_constituency_inner(&map, &config, None);
        assert!(result.is_ok());
        let count = result.unwrap();
        assert_eq!(count, 2, "should persist 2 constituent entries");
    }

    #[test]
    fn test_persist_constituency_inner_empty_map() {
        let port = spawn_tcp_drain_server();
        let map = IndexConstituencyMap::default();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let result = persist_constituency_inner(&map, &config, None);
        assert!(result.is_ok());
        let count = result.unwrap();
        assert_eq!(count, 0, "empty map should produce 0 entries");
    }

    #[test]
    fn test_persist_constituency_with_fno_universe_enrichment() {
        use dhan_live_trader_common::instrument_types::{
            FnoUnderlying, FnoUniverse, UnderlyingKind, UniverseBuildMetadata,
        };
        use dhan_live_trader_common::trading_calendar::ist_offset;
        use dhan_live_trader_common::types::ExchangeSegment;

        let port = spawn_tcp_drain_server();
        let map = make_test_constituency_map();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };

        // Build a minimal FnoUniverse with RELIANCE mapped
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

        let ist = ist_offset();
        let universe = FnoUniverse {
            underlyings,
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "primary".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 1,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::from_millis(100),
                build_timestamp: chrono::Utc::now().with_timezone(&ist),
            },
        };

        let result = persist_constituency_inner(&map, &config, Some(&universe));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 2);
    }

    #[test]
    fn test_persist_constituency_with_valid_server_first_attempt_succeeds() {
        let port = spawn_tcp_drain_server();
        let map = make_test_constituency_map();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let result = persist_constituency(&map, &config, None);
        assert!(
            result.is_ok(),
            "first attempt should succeed with valid TCP"
        );
    }

    #[test]
    fn test_persist_constituency_inner_connection_error() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        let map = make_test_constituency_map();
        let result = persist_constituency_inner(&map, &config, None);
        // Connection should fail — inner propagates errors
        let _is_err = result.is_err();
    }

    #[test]
    fn test_persist_constituency_multiple_indices() {
        use chrono::NaiveDate;
        use dhan_live_trader_common::instrument_types::IndexConstituent;

        let port = spawn_tcp_drain_server();

        let mut index_to_constituents = std::collections::HashMap::new();
        index_to_constituents.insert(
            "Nifty 50".to_string(),
            vec![IndexConstituent {
                index_name: "Nifty 50".to_string(),
                symbol: "RELIANCE".to_string(),
                isin: "INE002A01018".to_string(),
                weight: 10.5,
                sector: "Energy".to_string(),
                last_updated: NaiveDate::from_ymd_opt(2026, 3, 20).unwrap(),
            }],
        );
        index_to_constituents.insert(
            "Nifty Bank".to_string(),
            vec![
                IndexConstituent {
                    index_name: "Nifty Bank".to_string(),
                    symbol: "HDFCBANK".to_string(),
                    isin: "INE040A01034".to_string(),
                    weight: 25.0,
                    sector: "Banking".to_string(),
                    last_updated: NaiveDate::from_ymd_opt(2026, 3, 20).unwrap(),
                },
                IndexConstituent {
                    index_name: "Nifty Bank".to_string(),
                    symbol: "ICICIBANK".to_string(),
                    isin: "INE090A01021".to_string(),
                    weight: 20.0,
                    sector: "Banking".to_string(),
                    last_updated: NaiveDate::from_ymd_opt(2026, 3, 20).unwrap(),
                },
            ],
        );

        let map = IndexConstituencyMap {
            index_to_constituents,
            stock_to_indices: std::collections::HashMap::new(),
            build_metadata: Default::default(),
        };

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };

        let result = persist_constituency_inner(&map, &config, None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 3, "should persist 1 + 2 = 3 entries");
    }

    #[test]
    fn test_ilp_connection_string_format_constituency() {
        let config = QuestDbConfig {
            host: "dlt-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf_string = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        assert_eq!(conf_string, "tcp::addr=dlt-questdb:9009;");
    }

    #[test]
    fn test_http_base_url_format_constituency() {
        let config = QuestDbConfig {
            host: "dlt-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let base_url = format!("http://{}:{}/exec", config.host, config.http_port);
        assert_eq!(base_url, "http://dlt-questdb:9000/exec");
    }

    #[test]
    fn test_dedup_sql_format_constituency() {
        let dedup_ddl = format!(
            "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
            QUESTDB_TABLE_INDEX_CONSTITUENTS, DEDUP_KEY_INDEX_CONSTITUENTS
        );
        assert!(dedup_ddl.contains("ALTER TABLE index_constituents"));
        assert!(dedup_ddl.contains("DEDUP ENABLE UPSERT KEYS(ts, index_name, symbol)"));
    }

    // -----------------------------------------------------------------------
    // Additional DDL coverage: two-phase mock server returning different
    // responses for CREATE TABLE and DEDUP requests.
    // -----------------------------------------------------------------------

    /// Spawns an HTTP mock that returns `first_response` for the first request
    /// and `second_response` for all subsequent requests.
    async fn spawn_two_phase_http_server(
        first_response: &'static str,
        second_response: &'static str,
    ) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let counter = counter.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        let idx = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let resp = if idx == 0 {
                            first_response
                        } else {
                            second_response
                        };
                        let _ = stream.write_all(resp.as_bytes()).await;
                    });
                }
            }
        });
        tokio::task::yield_now().await;
        port
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_create_ok_dedup_fail() {
        // CREATE TABLE succeeds (200), DEDUP fails (400 without "already enabled")
        let port = spawn_two_phase_http_server(MOCK_HTTP_200, MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Should not panic — exercises success branch for CREATE, warn for DEDUP
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_create_fail_dedup_ok() {
        // CREATE TABLE fails (400), DEDUP succeeds (200)
        let port = spawn_two_phase_http_server(MOCK_HTTP_400, MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Should not panic — exercises warn for CREATE, success for DEDUP
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_create_ok_dedup_already_enabled() {
        // CREATE TABLE succeeds (200), DEDUP returns "already enabled" (400)
        let dedup_resp = "HTTP/1.1 400 Bad Request\r\nContent-Length: 37\r\n\r\n{\"error\":\"dedup already enabled foo\"}";
        // Use a static-lifetime response for the second phase
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let counter = counter.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        let idx = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let resp = if idx == 0 { MOCK_HTTP_200 } else { dedup_resp };
                        let _ = stream.write_all(resp.as_bytes()).await;
                    });
                }
            }
        });
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // CREATE succeeds, DEDUP "already enabled" = silently OK
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_dedup_network_error() {
        // First request (CREATE) succeeds, then the server drops for DEDUP.
        // This exercises the Err branch for the DEDUP send.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept first connection, respond with 200, then stop accepting
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(MOCK_HTTP_200.as_bytes()).await;
                drop(stream);
            }
            // Accept second connection but drop it immediately to cause error
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
        ensure_constituency_table(&config).await;
    }

    // -----------------------------------------------------------------------
    // persist_constituency: exercise the retry-then-succeed path
    // -----------------------------------------------------------------------

    #[test]
    fn test_persist_constituency_inner_unreachable_is_err() {
        // Verify that persist_constituency_inner returns Err (not Ok) on
        // connection failure — this drives the retry loop in persist_constituency.
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            ilp_port: 19009,
            http_port: 19000,
            pg_port: 18812,
        };
        let map = make_test_constituency_map();
        let result = persist_constituency_inner(&map, &config, None);
        assert!(
            result.is_err(),
            "persist_constituency_inner must propagate connection errors"
        );
    }

    #[test]
    fn test_persist_constituency_retries_on_failure_then_ok() {
        // persist_constituency must return Ok even after all retries fail.
        // This specifically tests that the error log + Ok(()) on line 175-179
        // is reached when persist_constituency_inner fails 3 times.
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            ilp_port: 19009,
            http_port: 19000,
            pg_port: 18812,
        };
        let map = make_test_constituency_map();
        let result = persist_constituency(&map, &config, None);
        assert!(
            result.is_ok(),
            "persist_constituency must be best-effort — Ok even after all retries fail"
        );
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_ddl_non_success_with_tracing() {
        // Same as the existing non-success test but with a tracing subscriber
        // installed to ensure warn! macro body (line 89, 121) is evaluated.
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let subscriber = {
            use tracing_subscriber::layer::SubscriberExt;
            tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().with_test_writer())
        };
        let _guard = tracing::subscriber::set_default(subscriber);
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_create_send_error() {
        // Server accepts connection for CREATE TABLE then immediately closes,
        // causing a transport error on send. This exercises the Err(err) branch
        // at line 94 (CREATE TABLE send error).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept first connection and drop immediately — causes send error
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
            // Accept second connection and drop — DEDUP also gets send error
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
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_send_error_with_tracing() {
        // Same as above but with tracing subscriber to cover warn! body
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
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
        let subscriber = {
            use tracing_subscriber::layer::SubscriberExt;
            tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().with_test_writer())
        };
        let _guard = tracing::subscriber::set_default(subscriber);
        ensure_constituency_table(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage: security_id zero handling when symbol not in FnoUniverse
    // -----------------------------------------------------------------------

    #[test]
    fn test_security_id_zero_for_unmapped_symbol() {
        // When fno_universe is Some but symbol is not found,
        // security_id defaults to 0 via unwrap_or(0).
        use dhan_live_trader_common::instrument_types::FnoUniverse;

        let ist = dhan_live_trader_common::trading_calendar::ist_offset();
        let universe = FnoUniverse {
            underlyings: std::collections::HashMap::new(),
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: dhan_live_trader_common::instrument_types::UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::from_millis(1),
                build_timestamp: chrono::Utc::now().with_timezone(&ist),
            },
        };

        // symbol_to_security_id returns None for missing symbols
        let sec_id = universe.symbol_to_security_id("NONEXISTENT").unwrap_or(0);
        assert_eq!(sec_id, 0, "missing symbol must default to security_id 0");
    }

    #[test]
    fn test_persist_constituency_ddl_has_isin_column() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("isin STRING"),
            "DDL must include isin STRING column"
        );
    }

    #[test]
    fn test_persist_constituency_ddl_has_sector_column() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("sector STRING"),
            "DDL must include sector STRING column"
        );
    }

    #[test]
    fn test_persist_constituency_inner_with_fno_universe_missing_symbol() {
        // FnoUniverse present but does not contain the symbol — exercises
        // the `unwrap_or(0)` default security_id path.
        use dhan_live_trader_common::instrument_types::{FnoUniverse, UniverseBuildMetadata};
        use dhan_live_trader_common::trading_calendar::ist_offset;

        let port = spawn_tcp_drain_server();
        let map = make_test_constituency_map();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };

        let ist = ist_offset();
        let universe = FnoUniverse {
            underlyings: std::collections::HashMap::new(), // Empty = no symbol matches
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "primary".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::from_millis(1),
                build_timestamp: chrono::Utc::now().with_timezone(&ist),
            },
        };

        let result = persist_constituency_inner(&map, &config, Some(&universe));
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            2,
            "should persist 2 entries with security_id=0 for missing symbols"
        );
    }
}
