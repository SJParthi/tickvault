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
    DEDUP UPSERT KEYS(security_id, segment)\
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
    DEDUP UPSERT KEYS(underlying_symbol, expiry_date)\
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
    DEDUP UPSERT KEYS(security_id, segment)\
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
    DEDUP UPSERT KEYS(security_id, segment)\
";

// ---------------------------------------------------------------------------
// Public API — Table Setup
// ---------------------------------------------------------------------------

/// Creates all Greeks-related QuestDB tables (idempotent).
///
/// Best-effort: if QuestDB is unreachable, logs a warning and continues.
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
    fn test_option_greeks_ddl_has_dedup_key() {
        assert!(
            OPTION_GREEKS_DDL.contains("DEDUP UPSERT KEYS(security_id, segment)"),
            "option_greeks must have dedup on security_id + segment"
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
    fn test_pcr_snapshots_ddl_contains_dedup() {
        assert!(
            PCR_SNAPSHOTS_DDL.contains("DEDUP UPSERT KEYS(underlying_symbol, expiry_date)"),
            "pcr_snapshots must dedup on underlying + expiry"
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
    fn test_verification_ddl_has_security_id_dedup() {
        assert!(GREEKS_VERIFICATION_DDL.contains("security_id LONG"));
        assert!(GREEKS_VERIFICATION_DDL.contains("DEDUP UPSERT KEYS(security_id, segment)"));
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
    fn test_dhan_raw_ddl_dedup_and_partition() {
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("DEDUP UPSERT KEYS(security_id, segment)"));
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("PARTITION BY HOUR WAL"));
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("TIMESTAMP(ts)"));
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("IF NOT EXISTS"));
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
}
