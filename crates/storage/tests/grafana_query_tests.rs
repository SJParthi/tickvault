//! Integration tests for Grafana dashboard SQL queries.
//!
//! Runs the **exact same SQL** that the `market-data.json` Grafana dashboard uses
//! against QuestDB's HTTP `/exec` API. If these tests pass, the dashboard renders.
//!
//! # Test categories
//! 1. **Schema validation** — every table/view exists with correct columns and types
//! 2. **Functional** — each Grafana panel query parses, executes, returns correct columns
//! 3. **Regression** — edge cases (empty ranges, non-existent IDs, future timestamps)
//! 4. **Stress** — queries complete within bounded time even on large tables
//!
//! # Running
//! ```bash
//! # Requires QuestDB running (Docker stack up)
//! QUESTDB_HTTP_URL=http://localhost:9000 cargo test -p dhan-live-trader-storage \
//!     --test grafana_query_tests -- --ignored
//! ```
//!
//! All tests are `#[ignore]` — they need a live QuestDB instance with tables created
//! by the application's startup DDL (run the app once, or use `ensure_*` functions).

use std::time::{Duration, Instant};

use serde::Deserialize;

// ---------------------------------------------------------------------------
// QuestDB HTTP /exec response types
// ---------------------------------------------------------------------------

/// Column metadata from QuestDB JSON response.
#[derive(Debug, Deserialize)]
struct QuestDbColumn {
    name: String,
    #[serde(rename = "type")]
    type_name: String,
}

/// Successful QuestDB `/exec` JSON response.
#[derive(Debug, Deserialize)]
struct QuestDbResponse {
    columns: Vec<QuestDbColumn>,
    #[allow(dead_code)] // APPROVED: retained for data-level assertions in future tests
    dataset: Vec<Vec<serde_json::Value>>,
    count: u64,
}

/// Error response from QuestDB `/exec`.
#[derive(Debug, Deserialize)]
struct QuestDbError {
    error: String,
    #[serde(default)]
    position: i64,
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// QuestDB HTTP URL from environment or Docker default.
fn questdb_url() -> String {
    std::env::var("QUESTDB_HTTP_URL").unwrap_or_else(|_| "http://dlt-questdb:9000".to_string())
}

/// Execute a SQL query against QuestDB HTTP API and return parsed response.
async fn execute_query(sql: &str) -> QuestDbResponse {
    let url = questdb_url();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client");

    let response = client
        .get(format!("{}/exec", url))
        .query(&[("query", sql)])
        .send()
        .await
        .unwrap_or_else(|err| panic!("QuestDB HTTP request failed: {err}"));

    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|err| panic!("failed to read response body: {err}"));

    assert!(
        status.is_success(),
        "QuestDB returned HTTP {status} for query:\n{sql}\nBody: {body}"
    );

    // Check for QuestDB error envelope
    if let Ok(error_resp) = serde_json::from_str::<QuestDbError>(&body)
        && !error_resp.error.is_empty()
    {
        panic!(
            "QuestDB SQL error at position {}: {}\nQuery: {sql}",
            error_resp.position, error_resp.error
        );
    }

    serde_json::from_str::<QuestDbResponse>(&body)
        .unwrap_or_else(|err| panic!("failed to parse QuestDB response: {err}\nBody: {body}"))
}

/// Execute a SQL query and also return elapsed time.
async fn execute_query_timed(sql: &str) -> (QuestDbResponse, Duration) {
    let start = Instant::now();
    let response = execute_query(sql).await;
    (response, start.elapsed())
}

/// Assert a response contains the expected column names (order-insensitive).
fn assert_columns_present(response: &QuestDbResponse, expected: &[&str], query_name: &str) {
    let actual_names: Vec<&str> = response.columns.iter().map(|c| c.name.as_str()).collect();
    for expected_col in expected {
        assert!(
            actual_names.contains(expected_col),
            "[{query_name}] missing column '{expected_col}'. Got: {actual_names:?}"
        );
    }
}

/// Assert a response contains the expected columns in exact order with types.
fn assert_columns_exact(response: &QuestDbResponse, expected: &[(&str, &str)], query_name: &str) {
    assert_eq!(
        response.columns.len(),
        expected.len(),
        "[{query_name}] column count mismatch: expected {}, got {}.\nExpected: {expected:?}\nGot: {:?}",
        expected.len(),
        response.columns.len(),
        response
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.type_name.as_str()))
            .collect::<Vec<_>>()
    );

    for (i, (exp_name, exp_type)) in expected.iter().enumerate() {
        let col = &response.columns[i];
        assert_eq!(
            col.name, *exp_name,
            "[{query_name}] column {i} name mismatch: expected '{exp_name}', got '{}'",
            col.name
        );
        assert_eq!(
            col.type_name, *exp_type,
            "[{query_name}] column '{exp_name}' type mismatch: expected '{exp_type}', got '{}'",
            col.type_name
        );
    }
}

// =========================================================================
// 1. SCHEMA VALIDATION — every QuestDB table exists with correct columns
// =========================================================================

/// Validate table schema by querying `SELECT * FROM <table> LIMIT 0`.
/// Returns column metadata without needing any data in the table.
async fn validate_table_schema(table: &str, expected: &[(&str, &str)]) {
    let sql = format!("SELECT * FROM {table} LIMIT 0");
    let response = execute_query(&sql).await;
    assert_columns_exact(&response, expected, &format!("schema:{table}"));
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_ticks_table() {
    validate_table_schema(
        "ticks",
        &[
            ("segment", "SYMBOL"),
            ("security_id", "LONG"),
            ("ltp", "DOUBLE"),
            ("open", "DOUBLE"),
            ("high", "DOUBLE"),
            ("low", "DOUBLE"),
            ("close", "DOUBLE"),
            ("volume", "LONG"),
            ("oi", "LONG"),
            ("avg_price", "DOUBLE"),
            ("last_trade_qty", "LONG"),
            ("total_buy_qty", "LONG"),
            ("total_sell_qty", "LONG"),
            ("exchange_timestamp", "LONG"),
            ("received_at", "TIMESTAMP"),
            ("ts", "TIMESTAMP"),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_market_depth_table() {
    validate_table_schema(
        "market_depth",
        &[
            ("segment", "SYMBOL"),
            ("security_id", "LONG"),
            ("level", "LONG"),
            ("bid_qty", "LONG"),
            ("ask_qty", "LONG"),
            ("bid_orders", "LONG"),
            ("ask_orders", "LONG"),
            ("bid_price", "DOUBLE"),
            ("ask_price", "DOUBLE"),
            ("received_at", "TIMESTAMP"),
            ("ts", "TIMESTAMP"),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_previous_close_table() {
    validate_table_schema(
        "previous_close",
        &[
            ("segment", "SYMBOL"),
            ("security_id", "LONG"),
            ("prev_close", "DOUBLE"),
            ("prev_oi", "LONG"),
            ("ts", "TIMESTAMP"),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_historical_candles_table() {
    validate_table_schema(
        "historical_candles",
        &[
            ("segment", "SYMBOL"),
            ("timeframe", "SYMBOL"),
            ("security_id", "LONG"),
            ("open", "DOUBLE"),
            ("high", "DOUBLE"),
            ("low", "DOUBLE"),
            ("close", "DOUBLE"),
            ("volume", "LONG"),
            ("oi", "LONG"),
            ("ts", "TIMESTAMP"),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_candles_1s_table() {
    validate_table_schema(
        "candles_1s",
        &[
            ("segment", "SYMBOL"),
            ("security_id", "LONG"),
            ("open", "DOUBLE"),
            ("high", "DOUBLE"),
            ("low", "DOUBLE"),
            ("close", "DOUBLE"),
            ("volume", "LONG"),
            ("oi", "LONG"),
            ("tick_count", "INT"),
            ("ts", "TIMESTAMP"),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_subscribed_indices_table() {
    validate_table_schema(
        "subscribed_indices",
        &[
            ("symbol", "SYMBOL"),
            ("exchange", "SYMBOL"),
            ("category", "SYMBOL"),
            ("subcategory", "SYMBOL"),
            ("security_id", "LONG"),
            ("timestamp", "TIMESTAMP"),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_fno_underlyings_table() {
    validate_table_schema(
        "fno_underlyings",
        &[
            ("underlying_symbol", "SYMBOL"),
            ("price_feed_segment", "SYMBOL"),
            ("derivative_segment", "SYMBOL"),
            ("kind", "SYMBOL"),
            ("underlying_security_id", "LONG"),
            ("price_feed_security_id", "LONG"),
            ("lot_size", "LONG"),
            ("contract_count", "LONG"),
            ("timestamp", "TIMESTAMP"),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_derivative_contracts_table() {
    validate_table_schema(
        "derivative_contracts",
        &[
            ("underlying_symbol", "SYMBOL"),
            ("instrument_kind", "SYMBOL"),
            ("exchange_segment", "SYMBOL"),
            ("option_type", "SYMBOL"),
            ("symbol_name", "SYMBOL"),
            ("security_id", "LONG"),
            ("expiry_date", "STRING"),
            ("strike_price", "DOUBLE"),
            ("lot_size", "LONG"),
            ("tick_size", "DOUBLE"),
            ("display_name", "STRING"),
            ("timestamp", "TIMESTAMP"),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_instrument_build_metadata_table() {
    validate_table_schema(
        "instrument_build_metadata",
        &[
            ("csv_source", "SYMBOL"),
            ("csv_row_count", "LONG"),
            ("parsed_row_count", "LONG"),
            ("index_count", "LONG"),
            ("equity_count", "LONG"),
            ("underlying_count", "LONG"),
            ("derivative_count", "LONG"),
            ("option_chain_count", "LONG"),
            ("build_duration_ms", "LONG"),
            ("build_timestamp", "TIMESTAMP"),
            ("timestamp", "TIMESTAMP"),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn schema_nse_holidays_table() {
    validate_table_schema(
        "nse_holidays",
        &[
            ("name", "SYMBOL"),
            ("holiday_type", "SYMBOL"),
            ("ts", "TIMESTAMP"),
        ],
    )
    .await;
}

// =========================================================================
// 2. FUNCTIONAL — exact Grafana panel queries execute without error
// =========================================================================

// ---------------------------------------------------------------------------
// 2a. Stat panels (overview row)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_stat_total_ticks() {
    let response = execute_query("SELECT count() AS total FROM ticks;").await;
    assert_columns_present(&response, &["total"], "stat:total_ticks");
    assert_eq!(response.count, 1, "count query must return exactly 1 row");
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_stat_historical_candles() {
    let response = execute_query("SELECT count() AS total FROM historical_candles;").await;
    assert_columns_present(&response, &["total"], "stat:historical_candles");
    assert_eq!(response.count, 1);
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_stat_fno_underlyings() {
    let response = execute_query("SELECT count() AS total FROM fno_underlyings;").await;
    assert_columns_present(&response, &["total"], "stat:fno_underlyings");
    assert_eq!(response.count, 1);
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_stat_derivative_contracts() {
    let response = execute_query("SELECT count() AS total FROM derivative_contracts;").await;
    assert_columns_present(&response, &["total"], "stat:derivative_contracts");
    assert_eq!(response.count, 1);
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_stat_subscribed_indices() {
    let response = execute_query("SELECT count() AS total FROM subscribed_indices;").await;
    assert_columns_present(&response, &["total"], "stat:subscribed_indices");
    assert_eq!(response.count, 1);
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_stat_unique_securities() {
    let response = execute_query("SELECT count_distinct(security_id) AS total FROM ticks;").await;
    assert_columns_present(&response, &["total"], "stat:unique_securities");
    assert_eq!(response.count, 1);
}

// ---------------------------------------------------------------------------
// 2b. Template variable queries (dropdown pickers with UNION ALL)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_underlying_picker_union_all() {
    // Exact query from market-data.json line 952
    let sql = "\
        SELECT DISTINCT symbol AS __text, security_id AS __value \
        FROM subscribed_indices WHERE category = 'FnoUnderlying' \
        UNION ALL \
        SELECT DISTINCT underlying_symbol AS __text, price_feed_security_id AS __value \
        FROM fno_underlyings WHERE kind = 'Stock' \
        ORDER BY 1;";
    let response = execute_query(sql).await;
    assert_columns_present(&response, &["__text", "__value"], "var:underlying_picker");
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_contract_picker_union_all() {
    // Exact query from market-data.json line 970, with NIFTY (security_id=13) substituted
    let sql = "\
        SELECT 'NIFTY (Spot)' AS __text, 13 AS __value \
        UNION ALL \
        SELECT display_name AS __text, security_id AS __value \
        FROM derivative_contracts WHERE underlying_symbol = 'NIFTY' \
        ORDER BY 1;";
    let response = execute_query(sql).await;
    assert_columns_present(&response, &["__text", "__value"], "var:contract_picker");
    // Must have at least the Spot row
    assert!(
        response.count >= 1,
        "contract picker must return at least the Spot row"
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_segment_resolver_triple_union_all() {
    // Exact query from market-data.json line 988, with NIFTY (13) substituted
    let sql = "\
        SELECT 'IDX_I' AS segment FROM subscribed_indices \
        WHERE security_id = 13 AND symbol = 'NIFTY' \
        UNION ALL \
        SELECT price_feed_segment AS segment FROM fno_underlyings \
        WHERE price_feed_security_id = 13 AND underlying_symbol = 'NIFTY' \
        UNION ALL \
        SELECT exchange_segment AS segment FROM derivative_contracts \
        WHERE security_id = 13 AND underlying_symbol = 'NIFTY';";
    let response = execute_query(sql).await;
    assert_columns_present(&response, &["segment"], "var:segment_resolver");
}

// ---------------------------------------------------------------------------
// 2c. Candlestick chart queries (OHLCV — the core question)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_historical_ohlcv_candlestick() {
    // Exact query from market-data.json line 260
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM historical_candles \
        WHERE timeframe = '1m' AND security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_columns_exact(
        &response,
        &[
            ("time", "TIMESTAMP"),
            ("open", "DOUBLE"),
            ("high", "DOUBLE"),
            ("low", "DOUBLE"),
            ("close", "DOUBLE"),
            ("volume", "LONG"),
        ],
        "chart:historical_ohlcv",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_live_ohlcv_candlestick_1m() {
    // Exact query from market-data.json line 332 (candles_${timeframe} → candles_1m)
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM candles_1m \
        WHERE security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_columns_exact(
        &response,
        &[
            ("time", "TIMESTAMP"),
            ("open", "DOUBLE"),
            ("high", "DOUBLE"),
            ("low", "DOUBLE"),
            ("close", "DOUBLE"),
            ("volume", "LONG"),
        ],
        "chart:live_ohlcv_1m",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_live_ohlcv_candlestick_5m() {
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM candles_5m \
        WHERE security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_columns_exact(
        &response,
        &[
            ("time", "TIMESTAMP"),
            ("open", "DOUBLE"),
            ("high", "DOUBLE"),
            ("low", "DOUBLE"),
            ("close", "DOUBLE"),
            ("volume", "LONG"),
        ],
        "chart:live_ohlcv_5m",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_live_ohlcv_candlestick_15m() {
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM candles_15m \
        WHERE security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_columns_exact(
        &response,
        &[
            ("time", "TIMESTAMP"),
            ("open", "DOUBLE"),
            ("high", "DOUBLE"),
            ("low", "DOUBLE"),
            ("close", "DOUBLE"),
            ("volume", "LONG"),
        ],
        "chart:live_ohlcv_15m",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_live_ohlcv_candlestick_1h() {
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM candles_1h \
        WHERE security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_columns_exact(
        &response,
        &[
            ("time", "TIMESTAMP"),
            ("open", "DOUBLE"),
            ("high", "DOUBLE"),
            ("low", "DOUBLE"),
            ("close", "DOUBLE"),
            ("volume", "LONG"),
        ],
        "chart:live_ohlcv_1h",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_live_ohlcv_candlestick_1d() {
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM candles_1d \
        WHERE security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_columns_exact(
        &response,
        &[
            ("time", "TIMESTAMP"),
            ("open", "DOUBLE"),
            ("high", "DOUBLE"),
            ("low", "DOUBLE"),
            ("close", "DOUBLE"),
            ("volume", "LONG"),
        ],
        "chart:live_ohlcv_1d",
    );
}

// ---------------------------------------------------------------------------
// 2d. SMA overlay (window function)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_sma_overlay_historical() {
    // Exact query from market-data.json line 268
    let sql = "\
        SELECT time, avg(close) OVER (ORDER BY time ROWS BETWEEN 19 PRECEDING AND CURRENT ROW) AS \"SMA 20\" \
        FROM ( \
          SELECT ts AS time, close \
          FROM historical_candles \
          WHERE timeframe = '1m' AND security_id = 13 AND segment = 'IDX_I' \
            AND ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
          ORDER BY ts \
        );";
    let response = execute_query(sql).await;
    assert_columns_present(&response, &["time", "SMA 20"], "chart:sma_historical");
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_sma_overlay_live() {
    // Exact query from market-data.json line 340 (candles_1m)
    let sql = "\
        SELECT time, avg(close) OVER (ORDER BY time ROWS BETWEEN 19 PRECEDING AND CURRENT ROW) AS \"SMA 20\" \
        FROM ( \
          SELECT ts AS time, close \
          FROM candles_1m \
          WHERE security_id = 13 AND segment = 'IDX_I' \
            AND ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
          ORDER BY ts \
        );";
    let response = execute_query(sql).await;
    assert_columns_present(&response, &["time", "SMA 20"], "chart:sma_live");
}

// ---------------------------------------------------------------------------
// 2e. Data table panels
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_tick_explorer_table() {
    // Exact query from market-data.json line 413
    let sql = "\
        SELECT segment, security_id, ltp, open, high, low, close, volume, oi, \
        avg_price, last_trade_qty, total_buy_qty, total_sell_qty, \
        ts AS exchange_time, received_at \
        FROM ticks \
        WHERE ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts DESC LIMIT 200;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &[
            "segment",
            "security_id",
            "ltp",
            "open",
            "high",
            "low",
            "close",
            "volume",
            "oi",
            "avg_price",
            "last_trade_qty",
            "total_buy_qty",
            "total_sell_qty",
            "exchange_time",
            "received_at",
        ],
        "table:tick_explorer",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_historical_candle_table() {
    // Exact query from market-data.json line 486
    let sql = "\
        SELECT segment, security_id, open, high, low, close, volume, oi, \
        ts AS candle_time \
        FROM historical_candles \
        WHERE timeframe = '1m' AND ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts DESC LIMIT 500;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &[
            "segment",
            "security_id",
            "open",
            "high",
            "low",
            "close",
            "volume",
            "oi",
            "candle_time",
        ],
        "table:historical_candles",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_live_candle_table() {
    // Exact query from market-data.json line 559
    let sql = "\
        SELECT segment, security_id, open, high, low, close, volume, oi, \
        tick_count, ts AS candle_time \
        FROM candles_1m \
        WHERE ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts DESC LIMIT 500;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &[
            "segment",
            "security_id",
            "open",
            "high",
            "low",
            "close",
            "volume",
            "oi",
            "tick_count",
            "candle_time",
        ],
        "table:live_candles",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_fno_underlyings_table() {
    // Exact query from market-data.json line 603
    let sql = "\
        SELECT underlying_symbol, underlying_security_id, price_feed_segment, \
        price_feed_security_id, derivative_segment, kind, lot_size, contract_count, \
        timestamp AS loaded_at \
        FROM fno_underlyings ORDER BY underlying_symbol;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &[
            "underlying_symbol",
            "underlying_security_id",
            "price_feed_segment",
            "price_feed_security_id",
            "derivative_segment",
            "kind",
            "lot_size",
            "contract_count",
            "loaded_at",
        ],
        "table:fno_underlyings",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_derivative_contracts_table() {
    // Exact query from market-data.json line 645
    let sql = "\
        SELECT underlying_symbol, instrument_kind, exchange_segment, security_id, \
        symbol_name, option_type, strike_price, expiry_date, lot_size, display_name, \
        timestamp AS loaded_at \
        FROM derivative_contracts \
        ORDER BY underlying_symbol, expiry_date, strike_price LIMIT 1000;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &[
            "underlying_symbol",
            "instrument_kind",
            "exchange_segment",
            "security_id",
            "symbol_name",
            "option_type",
            "strike_price",
            "expiry_date",
            "lot_size",
            "display_name",
            "loaded_at",
        ],
        "table:derivative_contracts",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_subscribed_indices_table() {
    // Exact query from market-data.json line 689
    let sql = "\
        SELECT symbol, security_id, exchange, category, subcategory, \
        timestamp AS loaded_at \
        FROM subscribed_indices ORDER BY symbol;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &[
            "symbol",
            "security_id",
            "exchange",
            "category",
            "subcategory",
            "loaded_at",
        ],
        "table:subscribed_indices",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_market_depth_table() {
    // Exact query from market-data.json line 766
    let sql = "\
        SELECT segment, security_id, level, bid_qty, bid_price, ask_price, ask_qty, \
        ts AS depth_time \
        FROM market_depth \
        WHERE ts >= '2026-01-01T00:00:00Z' AND ts < '2026-12-31T23:59:59Z' \
        ORDER BY ts DESC, security_id, level LIMIT 500;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &[
            "segment",
            "security_id",
            "level",
            "bid_qty",
            "bid_price",
            "ask_price",
            "ask_qty",
            "depth_time",
        ],
        "table:market_depth",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_previous_close_table() {
    // Exact query from market-data.json line 815
    let sql = "\
        SELECT segment, security_id, prev_close, prev_oi, ts AS time \
        FROM previous_close ORDER BY security_id LIMIT 500;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &["segment", "security_id", "prev_close", "prev_oi", "time"],
        "table:previous_close",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_instrument_build_metadata_table() {
    // Exact query from market-data.json line 864
    let sql = "\
        SELECT csv_source, csv_row_count, parsed_row_count, index_count, equity_count, \
        underlying_count, derivative_count, option_chain_count, build_duration_ms, \
        timestamp AS build_time \
        FROM instrument_build_metadata ORDER BY timestamp DESC LIMIT 50;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &[
            "csv_source",
            "csv_row_count",
            "parsed_row_count",
            "index_count",
            "equity_count",
            "underlying_count",
            "derivative_count",
            "option_chain_count",
            "build_duration_ms",
            "build_time",
        ],
        "table:instrument_build_metadata",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_nse_holidays_table() {
    // Exact query from market-data.json line 930
    let sql = "\
        SELECT name, holiday_type, ts AS holiday_date \
        FROM nse_holidays ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &["name", "holiday_type", "holiday_date"],
        "table:nse_holidays",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn functional_nse_holiday_annotations() {
    // Exact annotation query from market-data.json line 25
    let sql = "\
        SELECT ts AS time, name || ' (' || holiday_type || ')' AS text, \
        holiday_type AS tags FROM nse_holidays ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_columns_present(
        &response,
        &["time", "text", "tags"],
        "annotation:nse_holidays",
    );
}

// =========================================================================
// 3. REGRESSION — edge cases that must not break Grafana
// =========================================================================

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn regression_empty_time_range_historical_candles() {
    // A time range with no data should return 0 rows, not an error.
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM historical_candles \
        WHERE timeframe = '1m' AND security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2020-01-01T00:00:00Z' AND ts < '2020-01-01T00:00:01Z' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_eq!(response.count, 0, "empty range must return 0 rows");
    // Columns must still be present for Grafana schema detection
    assert_columns_present(
        &response,
        &["time", "open", "high", "low", "close", "volume"],
        "regression:empty_range",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn regression_empty_time_range_live_candles() {
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM candles_1m \
        WHERE security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2020-01-01T00:00:00Z' AND ts < '2020-01-01T00:00:01Z' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_eq!(response.count, 0);
    assert_columns_present(
        &response,
        &["time", "open", "high", "low", "close", "volume"],
        "regression:empty_range_live",
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn regression_nonexistent_security_id_returns_zero_rows() {
    // security_id 999999999 does not exist — query must succeed with 0 rows.
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM historical_candles \
        WHERE timeframe = '1m' AND security_id = 999999999 AND segment = 'IDX_I' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_eq!(
        response.count, 0,
        "non-existent security_id must return 0 rows"
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn regression_future_timestamp_range() {
    // Far-future range — no data, no error.
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM historical_candles \
        WHERE timeframe = '1m' AND security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2099-01-01T00:00:00Z' AND ts < '2099-12-31T23:59:59Z' \
        ORDER BY ts;";
    let response = execute_query(sql).await;
    assert_eq!(response.count, 0, "future range must return 0 rows");
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn regression_union_all_with_no_matching_rows() {
    // Both sides of UNION ALL return 0 rows — should still return columns.
    let sql = "\
        SELECT DISTINCT symbol AS __text, security_id AS __value \
        FROM subscribed_indices WHERE category = 'NONEXISTENT' \
        UNION ALL \
        SELECT DISTINCT underlying_symbol AS __text, price_feed_security_id AS __value \
        FROM fno_underlyings WHERE kind = 'NONEXISTENT' \
        ORDER BY 1;";
    let response = execute_query(sql).await;
    assert_eq!(response.count, 0, "UNION ALL with no matches must return 0");
    assert_columns_present(&response, &["__text", "__value"], "regression:empty_union");
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn regression_sma_window_on_empty_data() {
    // Window function on empty result set — must not error.
    let sql = "\
        SELECT time, avg(close) OVER (ORDER BY time ROWS BETWEEN 19 PRECEDING AND CURRENT ROW) AS \"SMA 20\" \
        FROM ( \
          SELECT ts AS time, close \
          FROM historical_candles \
          WHERE timeframe = '1m' AND security_id = 999999999 AND segment = 'IDX_I' \
          ORDER BY ts \
        );";
    let response = execute_query(sql).await;
    assert_eq!(response.count, 0);
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn regression_contract_picker_unknown_underlying() {
    // Unknown underlying — Spot row always present, 0 derivatives.
    let sql = "\
        SELECT 'ZZZZZZ (Spot)' AS __text, 0 AS __value \
        UNION ALL \
        SELECT display_name AS __text, security_id AS __value \
        FROM derivative_contracts WHERE underlying_symbol = 'ZZZZZZ' \
        ORDER BY 1;";
    let response = execute_query(sql).await;
    // At minimum the synthetic Spot row
    assert!(response.count >= 1, "must have at least the Spot row");
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn regression_segment_resolver_for_stock() {
    // RELIANCE (stock) — should resolve from fno_underlyings, not subscribed_indices.
    let sql = "\
        SELECT 'IDX_I' AS segment FROM subscribed_indices \
        WHERE security_id = 1333 AND symbol = 'RELIANCE' \
        UNION ALL \
        SELECT price_feed_segment AS segment FROM fno_underlyings \
        WHERE price_feed_security_id = 1333 AND underlying_symbol = 'RELIANCE' \
        UNION ALL \
        SELECT exchange_segment AS segment FROM derivative_contracts \
        WHERE security_id = 1333 AND underlying_symbol = 'RELIANCE';";
    let response = execute_query(sql).await;
    assert_columns_present(&response, &["segment"], "regression:stock_segment");
}

// =========================================================================
// 4. STRESS — queries complete within bounded time
// =========================================================================

/// Maximum acceptable query duration for simple SELECT queries.
const SIMPLE_QUERY_MAX_MS: u128 = 2_000;

/// Maximum acceptable query duration for complex queries (UNION ALL, window fns).
const COMPLEX_QUERY_MAX_MS: u128 = 5_000;

/// Maximum acceptable query duration for full-table scans.
const SCAN_QUERY_MAX_MS: u128 = 10_000;

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn stress_historical_ohlcv_full_year() {
    // Full year of data — must complete within bound.
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM historical_candles \
        WHERE timeframe = '1m' AND security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2026-01-01T00:00:00Z' AND ts < '2027-01-01T00:00:00Z' \
        ORDER BY ts;";
    let (response, elapsed) = execute_query_timed(sql).await;
    assert!(
        elapsed.as_millis() < SIMPLE_QUERY_MAX_MS,
        "stress:historical_ohlcv took {}ms (max {SIMPLE_QUERY_MAX_MS}ms), {} rows",
        elapsed.as_millis(),
        response.count
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn stress_live_ohlcv_full_year() {
    let sql = "\
        SELECT ts AS time, open, high, low, close, volume \
        FROM candles_1m \
        WHERE security_id = 13 AND segment = 'IDX_I' \
          AND ts >= '2026-01-01T00:00:00Z' AND ts < '2027-01-01T00:00:00Z' \
        ORDER BY ts;";
    let (response, elapsed) = execute_query_timed(sql).await;
    assert!(
        elapsed.as_millis() < SIMPLE_QUERY_MAX_MS,
        "stress:live_ohlcv took {}ms (max {SIMPLE_QUERY_MAX_MS}ms), {} rows",
        elapsed.as_millis(),
        response.count
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn stress_underlying_picker_union_all() {
    let sql = "\
        SELECT DISTINCT symbol AS __text, security_id AS __value \
        FROM subscribed_indices WHERE category = 'FnoUnderlying' \
        UNION ALL \
        SELECT DISTINCT underlying_symbol AS __text, price_feed_security_id AS __value \
        FROM fno_underlyings WHERE kind = 'Stock' \
        ORDER BY 1;";
    let (response, elapsed) = execute_query_timed(sql).await;
    assert!(
        elapsed.as_millis() < COMPLEX_QUERY_MAX_MS,
        "stress:underlying_picker took {}ms (max {COMPLEX_QUERY_MAX_MS}ms), {} rows",
        elapsed.as_millis(),
        response.count
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn stress_contract_picker_nifty() {
    let sql = "\
        SELECT 'NIFTY (Spot)' AS __text, 13 AS __value \
        UNION ALL \
        SELECT display_name AS __text, security_id AS __value \
        FROM derivative_contracts WHERE underlying_symbol = 'NIFTY' \
        ORDER BY 1;";
    let (response, elapsed) = execute_query_timed(sql).await;
    assert!(
        elapsed.as_millis() < COMPLEX_QUERY_MAX_MS,
        "stress:contract_picker took {}ms (max {COMPLEX_QUERY_MAX_MS}ms), {} rows",
        elapsed.as_millis(),
        response.count
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn stress_sma_window_function() {
    let sql = "\
        SELECT time, avg(close) OVER (ORDER BY time ROWS BETWEEN 19 PRECEDING AND CURRENT ROW) AS \"SMA 20\" \
        FROM ( \
          SELECT ts AS time, close \
          FROM historical_candles \
          WHERE timeframe = '1m' AND security_id = 13 AND segment = 'IDX_I' \
            AND ts >= '2026-01-01T00:00:00Z' AND ts < '2027-01-01T00:00:00Z' \
          ORDER BY ts \
        );";
    let (response, elapsed) = execute_query_timed(sql).await;
    assert!(
        elapsed.as_millis() < COMPLEX_QUERY_MAX_MS,
        "stress:sma_window took {}ms (max {COMPLEX_QUERY_MAX_MS}ms), {} rows",
        elapsed.as_millis(),
        response.count
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn stress_tick_explorer_200_rows() {
    let sql = "\
        SELECT segment, security_id, ltp, open, high, low, close, volume, oi, \
        avg_price, last_trade_qty, total_buy_qty, total_sell_qty, \
        ts AS exchange_time, received_at \
        FROM ticks \
        WHERE ts >= '2026-01-01T00:00:00Z' AND ts < '2027-01-01T00:00:00Z' \
        ORDER BY ts DESC LIMIT 200;";
    let (_response, elapsed) = execute_query_timed(sql).await;
    assert!(
        elapsed.as_millis() < SIMPLE_QUERY_MAX_MS,
        "stress:tick_explorer took {}ms (max {SIMPLE_QUERY_MAX_MS}ms)",
        elapsed.as_millis()
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn stress_derivative_contracts_1000_rows() {
    let sql = "\
        SELECT underlying_symbol, instrument_kind, exchange_segment, security_id, \
        symbol_name, option_type, strike_price, expiry_date, lot_size, display_name, \
        timestamp AS loaded_at \
        FROM derivative_contracts \
        ORDER BY underlying_symbol, expiry_date, strike_price LIMIT 1000;";
    let (_response, elapsed) = execute_query_timed(sql).await;
    assert!(
        elapsed.as_millis() < SIMPLE_QUERY_MAX_MS,
        "stress:derivative_contracts took {}ms (max {SIMPLE_QUERY_MAX_MS}ms)",
        elapsed.as_millis()
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn stress_all_stat_counters() {
    // All 6 stat panels must complete within bound (run sequentially to measure total).
    let queries = [
        "SELECT count() AS total FROM ticks;",
        "SELECT count() AS total FROM historical_candles WHERE timeframe = '1m';",
        "SELECT count() AS total FROM fno_underlyings;",
        "SELECT count() AS total FROM derivative_contracts;",
        "SELECT count() AS total FROM subscribed_indices;",
        "SELECT count_distinct(security_id) AS total FROM ticks;",
    ];
    let start = Instant::now();
    for sql in &queries {
        execute_query(sql).await;
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < SCAN_QUERY_MAX_MS,
        "stress:all_stats took {}ms total (max {SCAN_QUERY_MAX_MS}ms)",
        elapsed.as_millis()
    );
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn stress_market_depth_500_rows() {
    let sql = "\
        SELECT segment, security_id, level, bid_qty, bid_price, ask_price, ask_qty, \
        ts AS depth_time \
        FROM market_depth \
        WHERE ts >= '2026-01-01T00:00:00Z' AND ts < '2027-01-01T00:00:00Z' \
        ORDER BY ts DESC, security_id, level LIMIT 500;";
    let (_response, elapsed) = execute_query_timed(sql).await;
    assert!(
        elapsed.as_millis() < SIMPLE_QUERY_MAX_MS,
        "stress:market_depth took {}ms (max {SIMPLE_QUERY_MAX_MS}ms)",
        elapsed.as_millis()
    );
}

// =========================================================================
// 5. MATERIALIZED VIEW EXISTENCE — all 18 views queryable
// =========================================================================

/// All materialized view names that must exist.
const MATERIALIZED_VIEWS: &[&str] = &[
    "candles_5s",
    "candles_10s",
    "candles_15s",
    "candles_30s",
    "candles_1m",
    "candles_2m",
    "candles_3m",
    "candles_5m",
    "candles_10m",
    "candles_15m",
    "candles_30m",
    "candles_1h",
    "candles_2h",
    "candles_3h",
    "candles_4h",
    "candles_1d",
    "candles_7d",
    "candles_1M",
];

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn materialized_views_all_exist_and_queryable() {
    for view in MATERIALIZED_VIEWS {
        let sql = format!(
            "SELECT ts AS time, open, high, low, close, volume \
             FROM {view} LIMIT 0;"
        );
        let response = execute_query(&sql).await;
        assert_columns_present(
            &response,
            &["time", "open", "high", "low", "close", "volume"],
            &format!("view:{view}"),
        );
    }
}

#[tokio::test]
#[ignore = "requires live QuestDB"]
async fn materialized_views_ohlcv_columns_are_double() {
    // Grafana candlestick requires numeric columns — verify DOUBLE type.
    for view in MATERIALIZED_VIEWS {
        let sql = format!("SELECT * FROM {view} LIMIT 0;");
        let response = execute_query(&sql).await;
        for col in &response.columns {
            match col.name.as_str() {
                "open" | "high" | "low" | "close" => {
                    assert_eq!(
                        col.type_name, "DOUBLE",
                        "[{view}] column '{}' must be DOUBLE for candlestick, got '{}'",
                        col.name, col.type_name
                    );
                }
                "volume" => {
                    assert_eq!(
                        col.type_name, "LONG",
                        "[{view}] column 'volume' must be LONG, got '{}'",
                        col.type_name
                    );
                }
                _ => {}
            }
        }
    }
}
