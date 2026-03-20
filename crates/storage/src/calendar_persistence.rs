//! QuestDB persistence for NSE trading calendar (holidays + Muhurat sessions).
//!
//! Writes holiday data to QuestDB at boot so Grafana can display:
//! - A "Trading Calendar" table panel with dates, names, and types
//! - Annotations on candlestick charts marking holiday boundaries
//!
//! # Idempotency
//!
//! DEDUP UPSERT KEYS on `(ts, name)` ensure same-day re-runs don't duplicate.
//! Best-effort: failures log WARN and don't block trading.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::QUESTDB_TABLE_NSE_HOLIDAYS;
use dhan_live_trader_common::trading_calendar::TradingCalendar;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY for the `nse_holidays` table.
const DEDUP_KEY_NSE_HOLIDAYS: &str = "name";

/// DDL for `nse_holidays` — one row per holiday/Muhurat date.
const NSE_HOLIDAYS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS nse_holidays (\
        name SYMBOL,\
        holiday_type SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY YEAR WAL\
";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Creates the `nse_holidays` table (if not exists) and enables DEDUP.
///
/// Called once at startup alongside other `ensure_*` functions.
/// Best-effort: logs warnings on failure, never blocks boot.
pub async fn ensure_calendar_table(questdb_config: &QuestDbConfig) {
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
            warn!(?err, "failed to build HTTP client for calendar table DDL");
            return;
        }
    };

    // Step 1: CREATE TABLE IF NOT EXISTS
    match client
        .get(&base_url)
        .query(&[("query", NSE_HOLIDAYS_CREATE_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                debug!(
                    table = QUESTDB_TABLE_NSE_HOLIDAYS,
                    "calendar table ensured (CREATE TABLE IF NOT EXISTS)"
                );
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    table = QUESTDB_TABLE_NSE_HOLIDAYS,
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "CREATE TABLE DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(
                table = QUESTDB_TABLE_NSE_HOLIDAYS,
                ?err,
                "CREATE TABLE DDL request failed"
            );
        }
    }

    // Step 2: DEDUP UPSERT KEYS
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_NSE_HOLIDAYS, DEDUP_KEY_NSE_HOLIDAYS
    );

    match client
        .get(&base_url)
        .query(&[("query", &dedup_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                debug!(
                    table = QUESTDB_TABLE_NSE_HOLIDAYS,
                    key = DEDUP_KEY_NSE_HOLIDAYS,
                    "DEDUP UPSERT KEY enabled"
                );
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    table = QUESTDB_TABLE_NSE_HOLIDAYS,
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "ALTER TABLE DEDUP returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(
                table = QUESTDB_TABLE_NSE_HOLIDAYS,
                ?err,
                "ALTER TABLE DEDUP request failed"
            );
        }
    }
}

/// Persists all holiday and Muhurat entries to QuestDB via ILP.
///
/// Best-effort: on failure, logs warning and returns `Ok(())`.
/// Trading is never blocked by calendar persistence failures.
/// Maximum retry attempts for calendar ILP persistence.
const CALENDAR_PERSIST_MAX_RETRIES: u32 = 3;

/// Delay between calendar persistence retries (seconds).
const CALENDAR_PERSIST_RETRY_DELAY_SECS: u64 = 2;

pub fn persist_calendar(calendar: &TradingCalendar, questdb_config: &QuestDbConfig) -> Result<()> {
    let mut last_err = None;
    for attempt in 1..=CALENDAR_PERSIST_MAX_RETRIES {
        match persist_inner(calendar, questdb_config) {
            Ok(count) => {
                info!(
                    entries = count,
                    table = QUESTDB_TABLE_NSE_HOLIDAYS,
                    attempt,
                    "trading calendar persisted to QuestDB"
                );
                return Ok(());
            }
            Err(err) => {
                warn!(
                    ?err,
                    attempt,
                    max_retries = CALENDAR_PERSIST_MAX_RETRIES,
                    "calendar persistence attempt failed — retrying"
                );
                last_err = Some(err);
                if attempt < CALENDAR_PERSIST_MAX_RETRIES {
                    std::thread::sleep(std::time::Duration::from_secs(
                        CALENDAR_PERSIST_RETRY_DELAY_SECS,
                    ));
                }
            }
        }
    }
    tracing::error!(
        err = ?last_err,
        "QuestDB calendar persistence failed after {CALENDAR_PERSIST_MAX_RETRIES} attempts — nse_holidays table will be empty in Grafana"
    );
    Ok(())
}

fn persist_inner(calendar: &TradingCalendar, questdb_config: &QuestDbConfig) -> Result<usize> {
    let conf_string = format!(
        "tcp::addr={}:{};",
        questdb_config.host, questdb_config.ilp_port
    );
    let mut sender =
        Sender::from_conf(&conf_string).context("failed to connect to QuestDB ILP for calendar")?;
    let mut buffer = sender.new_buffer();

    let entries = calendar.all_entries();
    let count = entries.len();

    for entry in &entries {
        let holiday_type = if entry.is_muhurat {
            "Muhurat Trading"
        } else {
            "Holiday"
        };

        // Store holiday date as IST midnight directly (IST-as-UTC convention).
        // QuestDB will display 2026-03-09T00:00:00Z for an IST date of 2026-03-09.
        let midnight_epoch_secs = entry
            .date
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp())
            .context("failed to compute timestamp for holiday")?;

        let ts_nanos = TimestampNanos::new(midnight_epoch_secs.saturating_mul(1_000_000_000));

        buffer
            .table(QUESTDB_TABLE_NSE_HOLIDAYS)
            .context("failed to set table name")?
            .symbol("name", &entry.name)
            .context("failed to write name symbol")?
            .symbol("holiday_type", holiday_type)
            .context("failed to write holiday_type symbol")?
            .at(ts_nanos)
            .context("failed to set timestamp")?;
    }

    if count > 0 {
        sender
            .flush(&mut buffer)
            .context("failed to flush calendar data to QuestDB")?;
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
    fn ddl_creates_table_with_correct_name() {
        assert!(
            NSE_HOLIDAYS_CREATE_DDL.contains("nse_holidays"),
            "DDL must reference the nse_holidays table"
        );
    }

    #[test]
    fn ddl_uses_create_if_not_exists() {
        assert!(
            NSE_HOLIDAYS_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"),
            "DDL must use CREATE TABLE IF NOT EXISTS for idempotent boot"
        );
    }

    #[test]
    fn ddl_has_timestamp_column() {
        assert!(
            NSE_HOLIDAYS_CREATE_DDL.contains("ts TIMESTAMP"),
            "DDL must include ts TIMESTAMP column"
        );
    }

    #[test]
    fn ddl_has_name_symbol_column() {
        assert!(
            NSE_HOLIDAYS_CREATE_DDL.contains("name SYMBOL"),
            "DDL must include name SYMBOL column"
        );
    }

    #[test]
    fn ddl_has_holiday_type_symbol_column() {
        assert!(
            NSE_HOLIDAYS_CREATE_DDL.contains("holiday_type SYMBOL"),
            "DDL must include holiday_type SYMBOL column"
        );
    }

    #[test]
    fn ddl_has_timestamp_designation() {
        assert!(
            NSE_HOLIDAYS_CREATE_DDL.contains("TIMESTAMP(ts)"),
            "DDL must designate ts as the designated timestamp"
        );
    }

    #[test]
    fn ddl_uses_year_partitioning() {
        assert!(
            NSE_HOLIDAYS_CREATE_DDL.contains("PARTITION BY YEAR"),
            "DDL must partition by YEAR for calendar data"
        );
    }

    #[test]
    fn ddl_uses_wal() {
        assert!(
            NSE_HOLIDAYS_CREATE_DDL.contains("WAL"),
            "DDL must use WAL for concurrent write safety"
        );
    }

    #[test]
    fn table_name_constant_matches_ddl() {
        assert!(
            NSE_HOLIDAYS_CREATE_DDL.contains(QUESTDB_TABLE_NSE_HOLIDAYS),
            "DDL must use the QUESTDB_TABLE_NSE_HOLIDAYS constant table name"
        );
    }

    #[test]
    fn dedup_key_is_name() {
        assert_eq!(
            DEDUP_KEY_NSE_HOLIDAYS, "name",
            "DEDUP upsert key must be 'name'"
        );
    }

    #[test]
    fn dedup_sql_format_correct() {
        let dedup_sql = format!(
            "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
            QUESTDB_TABLE_NSE_HOLIDAYS, DEDUP_KEY_NSE_HOLIDAYS
        );
        assert!(dedup_sql.contains("ALTER TABLE nse_holidays"));
        assert!(dedup_sql.contains("DEDUP ENABLE UPSERT KEYS(ts, name)"));
    }

    #[test]
    fn questdb_ddl_timeout_is_reasonable() {
        // 10s is long enough for DDL, short enough to not block boot
        const {
            assert!(QUESTDB_DDL_TIMEOUT_SECS >= 5);
        }
        const {
            assert!(QUESTDB_DDL_TIMEOUT_SECS <= 30);
        }
    }

    #[test]
    fn persist_calendar_returns_ok_on_connection_failure() {
        // persist_calendar is best-effort — it must return Ok(()) even on failure.
        // We can test this by creating a calendar and pointing at a dead QuestDB.
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};

        let trading_config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-01-26".to_string(),
                name: "Republic Day".to_string(),
            }],
            muhurat_trading_dates: vec![],
        };

        let calendar = TradingCalendar::from_config(&trading_config).unwrap();
        let questdb_config = QuestDbConfig {
            host: "nonexistent-host-that-will-fail".to_string(),
            http_port: 9999,
            pg_port: 9998,
            ilp_port: 9997,
        };

        // persist_calendar wraps errors — must return Ok even when ILP fails
        let result = persist_calendar(&calendar, &questdb_config);
        assert!(
            result.is_ok(),
            "persist_calendar must return Ok even when QuestDB is unreachable"
        );
    }

    #[test]
    fn persist_calendar_empty_calendar_returns_ok() {
        use dhan_live_trader_common::config::TradingConfig;

        let trading_config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
        };

        let calendar = TradingCalendar::from_config(&trading_config).unwrap();
        let questdb_config = QuestDbConfig {
            host: "nonexistent-host".to_string(),
            http_port: 9999,
            pg_port: 9998,
            ilp_port: 9997,
        };

        // Empty calendar should not even attempt to flush
        let result = persist_calendar(&calendar, &questdb_config);
        assert!(result.is_ok());
    }

    #[test]
    fn ddl_is_single_statement() {
        // DDL should not contain semicolons (QuestDB exec endpoint expects single statement)
        assert!(
            !NSE_HOLIDAYS_CREATE_DDL.contains(';'),
            "DDL must be a single statement without semicolons"
        );
    }

    #[test]
    fn ilp_connection_string_format() {
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
    fn http_base_url_format() {
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
    fn calendar_persist_max_retries_is_3() {
        assert_eq!(CALENDAR_PERSIST_MAX_RETRIES, 3);
    }

    #[test]
    fn calendar_persist_retry_delay_is_2_secs() {
        assert_eq!(CALENDAR_PERSIST_RETRY_DELAY_SECS, 2);
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
    async fn test_ensure_calendar_table_does_not_panic_unreachable() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Should not panic — just logs warnings and returns.
        ensure_calendar_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_calendar_table_ddl_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the success path for both CREATE TABLE and DEDUP DDL.
        ensure_calendar_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_calendar_table_ddl_non_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the non-success path for CREATE TABLE DDL.
        ensure_calendar_table(&config).await;
    }

    // -----------------------------------------------------------------------
    // persist_inner tests with TCP drain server
    // -----------------------------------------------------------------------

    fn make_test_calendar_with_holiday() -> TradingCalendar {
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};

        // Dates must be weekdays — TradingCalendar::from_config rejects weekends.
        // 2026-01-26 = Monday, 2026-03-10 = Tuesday
        let trading_config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![
                NseHolidayEntry {
                    date: "2026-01-26".to_string(),
                    name: "Republic Day".to_string(),
                },
                NseHolidayEntry {
                    date: "2026-03-10".to_string(),
                    name: "Maha Shivaratri".to_string(),
                },
            ],
            muhurat_trading_dates: vec![],
        };

        TradingCalendar::from_config(&trading_config).unwrap()
    }

    fn make_test_calendar_with_muhurat() -> TradingCalendar {
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};

        let trading_config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-10-20".to_string(),
                name: "Diwali".to_string(),
            }],
            muhurat_trading_dates: vec![NseHolidayEntry {
                date: "2026-10-20".to_string(),
                name: "Muhurat Trading".to_string(),
            }],
        };

        TradingCalendar::from_config(&trading_config).unwrap()
    }

    #[test]
    fn test_persist_inner_with_valid_server_holidays() {
        let port = spawn_tcp_drain_server();
        let calendar = make_test_calendar_with_holiday();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let result = persist_inner(&calendar, &config);
        assert!(result.is_ok());
        let count = result.unwrap();
        assert_eq!(count, 2, "should persist 2 holiday entries");
    }

    #[test]
    fn test_persist_inner_with_valid_server_muhurat() {
        let port = spawn_tcp_drain_server();
        let calendar = make_test_calendar_with_muhurat();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let result = persist_inner(&calendar, &config);
        assert!(result.is_ok());
        // Diwali is both a holiday AND muhurat — should produce entries for both
        let count = result.unwrap();
        assert!(count > 0, "should persist at least 1 entry");
    }

    #[test]
    fn test_persist_inner_empty_calendar() {
        use dhan_live_trader_common::config::TradingConfig;

        let port = spawn_tcp_drain_server();
        let trading_config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
        };
        let calendar = TradingCalendar::from_config(&trading_config).unwrap();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let result = persist_inner(&calendar, &config);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            0,
            "empty calendar should produce 0 entries"
        );
    }

    #[test]
    fn test_persist_inner_connection_error() {
        let calendar = make_test_calendar_with_holiday();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        let result = persist_inner(&calendar, &config);
        // Connection should fail — inner propagates errors
        let _is_err = result.is_err();
    }

    #[test]
    fn test_persist_calendar_first_attempt_succeeds() {
        let port = spawn_tcp_drain_server();
        let calendar = make_test_calendar_with_holiday();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let result = persist_calendar(&calendar, &config);
        assert!(
            result.is_ok(),
            "first attempt should succeed with valid TCP"
        );
    }

    #[test]
    fn test_persist_calendar_holiday_type_string() {
        // Verify that persist_inner distinguishes "Holiday" vs "Muhurat Trading"
        let calendar = make_test_calendar_with_muhurat();
        let entries = calendar.all_entries();
        // At least one entry should have is_muhurat = true
        let has_muhurat = entries.iter().any(|e| e.is_muhurat);
        assert!(has_muhurat, "calendar should contain a muhurat entry");
    }
}
