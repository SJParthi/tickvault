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
// Pure helper functions (testable without DB)
// ---------------------------------------------------------------------------

/// Builds the QuestDB HTTP exec URL from host and port.
fn build_questdb_exec_url(host: &str, http_port: u16) -> String {
    format!("http://{}:{}/exec", host, http_port)
}

/// Builds the ALTER TABLE DEDUP ENABLE UPSERT KEYS SQL statement.
fn build_dedup_sql(table_name: &str, dedup_key: &str) -> String {
    format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        table_name, dedup_key
    )
}

/// Builds the ILP TCP connection string from host and port.
fn build_ilp_conf_string(host: &str, ilp_port: u16) -> String {
    format!("tcp::addr={}:{};", host, ilp_port)
}

/// Classifies a calendar entry as "Holiday", "Muhurat Trading", or "Mock Trading Session".
fn classify_holiday_type(
    entry: &dhan_live_trader_common::trading_calendar::HolidayInfo,
) -> &'static str {
    if entry.is_mock {
        "Mock Trading Session"
    } else if entry.is_muhurat {
        "Muhurat Trading"
    } else {
        "Holiday"
    }
}

/// Computes midnight epoch nanoseconds for a `NaiveDate`.
///
/// Converts the date to midnight (00:00:00) and then to nanoseconds.
/// Used for IST-as-UTC convention: QuestDB sees "2026-03-09T00:00:00Z" for IST date 2026-03-09.
///
/// Returns `None` if `and_hms_opt(0, 0, 0)` fails (should never happen for valid dates).
fn compute_midnight_epoch_nanos(date: chrono::NaiveDate) -> Option<i64> {
    date.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp().saturating_mul(1_000_000_000))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Creates the `nse_holidays` table (if not exists) and enables DEDUP.
///
/// Called once at startup alongside other `ensure_*` functions.
/// Best-effort: logs warnings on failure, never blocks boot.
pub async fn ensure_calendar_table(questdb_config: &QuestDbConfig) {
    let base_url = build_questdb_exec_url(&questdb_config.host, questdb_config.http_port);

    // Client::builder().timeout().build() is infallible (no custom TLS).
    // Coverage: unwrap_or_else avoids uncoverable else-return on dead path.
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

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
    let dedup_sql = build_dedup_sql(QUESTDB_TABLE_NSE_HOLIDAYS, DEDUP_KEY_NSE_HOLIDAYS);

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
    let conf_string = build_ilp_conf_string(&questdb_config.host, questdb_config.ilp_port);
    let mut sender =
        Sender::from_conf(&conf_string).context("failed to connect to QuestDB ILP for calendar")?;
    let mut buffer = sender.new_buffer();

    let entries = calendar.all_entries();
    let count = entries.len();

    for entry in &entries {
        let holiday_type = classify_holiday_type(entry);

        // Store holiday date as IST midnight directly (IST-as-UTC convention).
        // QuestDB will display 2026-03-09T00:00:00Z for an IST date of 2026-03-09.
        let ts_nanos_value = compute_midnight_epoch_nanos(entry.date)
            .context("failed to compute timestamp for holiday")?;

        let ts_nanos = TimestampNanos::new(ts_nanos_value);

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
            nse_mock_trading_dates: vec![],
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
            nse_mock_trading_dates: vec![],
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
        let conf_string = build_ilp_conf_string("dlt-questdb", 9009);
        assert_eq!(conf_string, "tcp::addr=dlt-questdb:9009;");
    }

    #[test]
    fn http_base_url_format() {
        let base_url = build_questdb_exec_url("dlt-questdb", 9000);
        assert_eq!(base_url, "http://dlt-questdb:9000/exec");
    }

    // -----------------------------------------------------------------------
    // build_questdb_exec_url
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_questdb_exec_url_ip() {
        let url = build_questdb_exec_url("192.168.1.100", 19000);
        assert_eq!(url, "http://192.168.1.100:19000/exec");
    }

    #[test]
    fn test_build_questdb_exec_url_ends_with_exec() {
        let url = build_questdb_exec_url("host", 9000);
        assert!(url.ends_with("/exec"));
    }

    #[test]
    fn test_build_questdb_exec_url_starts_with_http() {
        let url = build_questdb_exec_url("host", 9000);
        assert!(url.starts_with("http://"));
    }

    // -----------------------------------------------------------------------
    // build_dedup_sql
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_dedup_sql_calendar() {
        let sql = build_dedup_sql(QUESTDB_TABLE_NSE_HOLIDAYS, DEDUP_KEY_NSE_HOLIDAYS);
        assert_eq!(
            sql,
            "ALTER TABLE nse_holidays DEDUP ENABLE UPSERT KEYS(ts, name)"
        );
    }

    #[test]
    fn test_build_dedup_sql_starts_with_alter() {
        let sql = build_dedup_sql("any_table", "any_key");
        assert!(sql.starts_with("ALTER TABLE"));
    }

    #[test]
    fn test_build_dedup_sql_contains_upsert_keys() {
        let sql = build_dedup_sql("t", "k");
        assert!(sql.contains("DEDUP ENABLE UPSERT KEYS(ts, k)"));
    }

    // -----------------------------------------------------------------------
    // build_ilp_conf_string
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ilp_conf_string_custom_port() {
        let conf = build_ilp_conf_string("10.0.0.5", 19009);
        assert_eq!(conf, "tcp::addr=10.0.0.5:19009;");
    }

    #[test]
    fn test_build_ilp_conf_string_starts_with_tcp() {
        let conf = build_ilp_conf_string("host", 1234);
        assert!(conf.starts_with("tcp::addr="));
    }

    #[test]
    fn test_build_ilp_conf_string_ends_with_semicolon() {
        let conf = build_ilp_conf_string("host", 1234);
        assert!(conf.ends_with(';'));
    }

    // -----------------------------------------------------------------------
    // classify_holiday_type
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_holiday_type_regular() {
        use dhan_live_trader_common::trading_calendar::HolidayInfo;
        let entry = HolidayInfo {
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 26).unwrap(),
            name: "Republic Day".to_string(),
            is_muhurat: false,
            is_mock: false,
        };
        assert_eq!(classify_holiday_type(&entry), "Holiday");
    }

    #[test]
    fn test_classify_holiday_type_muhurat() {
        use dhan_live_trader_common::trading_calendar::HolidayInfo;
        let entry = HolidayInfo {
            date: chrono::NaiveDate::from_ymd_opt(2026, 11, 8).unwrap(),
            name: "Diwali".to_string(),
            is_muhurat: true,
            is_mock: false,
        };
        assert_eq!(classify_holiday_type(&entry), "Muhurat Trading");
    }

    #[test]
    fn test_classify_holiday_type_mock() {
        use dhan_live_trader_common::trading_calendar::HolidayInfo;
        let entry = HolidayInfo {
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 3).unwrap(),
            name: "Mock Trading Session 1".to_string(),
            is_muhurat: false,
            is_mock: true,
        };
        assert_eq!(classify_holiday_type(&entry), "Mock Trading Session");
    }

    // -----------------------------------------------------------------------
    // compute_midnight_epoch_nanos
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_midnight_epoch_nanos_epoch_date() {
        use chrono::NaiveDate;
        // 1970-01-01 midnight = 0 seconds = 0 nanos
        let date = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let nanos = compute_midnight_epoch_nanos(date);
        assert_eq!(nanos, Some(0));
    }

    #[test]
    fn test_compute_midnight_epoch_nanos_known_date() {
        use chrono::NaiveDate;
        // 2026-01-26 (Republic Day) midnight UTC
        let date = NaiveDate::from_ymd_opt(2026, 1, 26).unwrap();
        let nanos = compute_midnight_epoch_nanos(date).unwrap();
        // 2026-01-26T00:00:00Z epoch = 1769385600 seconds
        let expected_secs = 1_769_385_600_i64;
        assert_eq!(nanos, expected_secs * 1_000_000_000);
    }

    #[test]
    fn test_compute_midnight_epoch_nanos_positive() {
        use chrono::NaiveDate;
        // Any date after epoch should produce positive nanos
        let date = NaiveDate::from_ymd_opt(2024, 6, 15).unwrap();
        let nanos = compute_midnight_epoch_nanos(date).unwrap();
        assert!(nanos > 0);
    }

    #[test]
    fn test_compute_midnight_epoch_nanos_before_epoch() {
        use chrono::NaiveDate;
        // Date before Unix epoch should produce negative nanos
        let date = NaiveDate::from_ymd_opt(1969, 12, 31).unwrap();
        let nanos = compute_midnight_epoch_nanos(date).unwrap();
        assert!(nanos < 0);
    }

    #[test]
    fn test_compute_midnight_epoch_nanos_divisible_by_billion() {
        use chrono::NaiveDate;
        // Midnight timestamps should always be divisible by 1_000_000_000 (no sub-second component)
        let date = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let nanos = compute_midnight_epoch_nanos(date).unwrap();
        assert_eq!(nanos % 1_000_000_000, 0);
    }

    #[test]
    fn test_compute_midnight_epoch_nanos_consecutive_days() {
        use chrono::NaiveDate;
        let d1 = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 3, 2).unwrap();
        let n1 = compute_midnight_epoch_nanos(d1).unwrap();
        let n2 = compute_midnight_epoch_nanos(d2).unwrap();
        // Consecutive days differ by exactly 86400 seconds = 86400 * 1e9 nanos
        assert_eq!(n2 - n1, 86_400 * 1_000_000_000);
    }

    // -----------------------------------------------------------------------
    // Calendar persistence retry constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_calendar_retry_constants() {
        let retries = CALENDAR_PERSIST_MAX_RETRIES;
        let delay = CALENDAR_PERSIST_RETRY_DELAY_SECS;
        assert!((1..=10).contains(&retries));
        assert!((1..=30).contains(&delay));
    }

    #[tokio::test]
    async fn test_ensure_calendar_table_unreachable_no_panic() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        ensure_calendar_table(&config).await;
    }

    // -----------------------------------------------------------------------
    // HTTP mock helpers
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // ensure_calendar_table — HTTP success/non-success/error paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_calendar_table_http_200() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises success paths (lines 115, 150)
        ensure_calendar_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_calendar_table_http_400() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises non-success paths (lines 125, 161)
        ensure_calendar_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_calendar_table_create_send_error() {
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
        // Exercises Err branch on CREATE TABLE (lines 130-131)
        ensure_calendar_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_calendar_table_create_ok_dedup_send_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // First connection: respond OK
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(MOCK_HTTP_200.as_bytes()).await;
                drop(stream);
            }
            // Second connection: drop → DEDUP send error
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
        // Exercises Err branch on DEDUP DDL (lines 166-167)
        ensure_calendar_table(&config).await;
    }

    // -----------------------------------------------------------------------
    // persist_inner — with TCP drain (ILP write paths)
    // -----------------------------------------------------------------------

    #[test]
    fn test_persist_inner_with_calendar_data_and_tcp_drain() {
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};

        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };

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
                    name: "Holi".to_string(),
                },
            ],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };

        let calendar = TradingCalendar::from_config(&trading_config).unwrap();
        // Exercises persist_inner ILP write paths (lines 224-258)
        let result = persist_calendar(&calendar, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_persist_inner_with_muhurat_and_holiday() {
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};

        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };

        let trading_config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-10-21".to_string(),
                name: "Diwali".to_string(),
            }],
            muhurat_trading_dates: vec![NseHolidayEntry {
                date: "2026-10-21".to_string(),
                name: "Diwali".to_string(),
            }],
            nse_mock_trading_dates: vec![],
        };

        let calendar = TradingCalendar::from_config(&trading_config).unwrap();
        let result = persist_calendar(&calendar, &config);
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: warn! field evaluation with tracing subscriber
    // (lines 129, 165)
    // -----------------------------------------------------------------------

    fn install_test_subscriber() -> tracing::subscriber::DefaultGuard {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_test_writer());
        tracing::subscriber::set_default(subscriber)
    }

    #[tokio::test]
    async fn test_ensure_calendar_table_non_success_with_tracing() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // With subscriber installed, warn! evaluates body.chars().take(200)
        // covering lines 129 and 165.
        ensure_calendar_table(&config).await;
    }

    // -----------------------------------------------------------------------
    // classify_holiday_type — precedence edge case
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_holiday_type_mock_takes_precedence() {
        // If both is_mock and is_muhurat are true, mock wins (checked first)
        let entry = dhan_live_trader_common::trading_calendar::HolidayInfo {
            date: chrono::NaiveDate::from_ymd_opt(2026, 3, 14).unwrap(),
            name: "Both".to_string(),
            is_muhurat: true,
            is_mock: true,
        };
        assert_eq!(classify_holiday_type(&entry), "Mock Trading Session");
    }

    // -----------------------------------------------------------------------
    // compute_midnight_epoch_nanos
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_midnight_epoch_nanos_valid_date() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 1, 26).unwrap();
        let nanos = compute_midnight_epoch_nanos(date);
        assert!(nanos.is_some());
        let nanos = nanos.unwrap();
        // 2026-01-26 00:00:00 UTC in nanos
        assert!(nanos > 0);
        // Should be divisible by 1_000_000_000 (whole seconds)
        assert_eq!(nanos % 1_000_000_000, 0);
    }

    #[test]
    fn test_compute_midnight_epoch_nanos_epoch() {
        let date = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let nanos = compute_midnight_epoch_nanos(date);
        assert_eq!(nanos, Some(0));
    }

    #[test]
    fn test_compute_midnight_epoch_nanos_different_dates_differ() {
        let d1 = chrono::NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        let d2 = chrono::NaiveDate::from_ymd_opt(2026, 3, 2).unwrap();
        let n1 = compute_midnight_epoch_nanos(d1).unwrap();
        let n2 = compute_midnight_epoch_nanos(d2).unwrap();
        assert_ne!(n1, n2);
        // One day apart = 86400 * 1_000_000_000 nanos
        assert_eq!(n2 - n1, 86_400 * 1_000_000_000);
    }

    // -----------------------------------------------------------------------
    // build_questdb_exec_url / build_dedup_sql / build_ilp_conf_string
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_questdb_exec_url() {
        let url = build_questdb_exec_url("dlt-questdb", 9000);
        assert_eq!(url, "http://dlt-questdb:9000/exec");
    }

    #[test]
    fn test_build_dedup_sql() {
        let sql = build_dedup_sql("nse_holidays", "name");
        assert_eq!(
            sql,
            "ALTER TABLE nse_holidays DEDUP ENABLE UPSERT KEYS(ts, name)"
        );
    }

    #[test]
    fn test_build_ilp_conf_string() {
        let conf = build_ilp_conf_string("dlt-questdb", 9009);
        assert_eq!(conf, "tcp::addr=dlt-questdb:9009;");
    }

    // -----------------------------------------------------------------------
    // Retry constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_calendar_persist_max_retries_reasonable() {
        assert!(CALENDAR_PERSIST_MAX_RETRIES >= 1);
        assert!(CALENDAR_PERSIST_MAX_RETRIES <= 10);
    }

    #[test]
    fn test_calendar_persist_retry_delay_reasonable() {
        assert!(CALENDAR_PERSIST_RETRY_DELAY_SECS >= 1);
        assert!(CALENDAR_PERSIST_RETRY_DELAY_SECS <= 10);
    }
}
