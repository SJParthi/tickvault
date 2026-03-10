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
pub fn persist_calendar(calendar: &TradingCalendar, questdb_config: &QuestDbConfig) -> Result<()> {
    match persist_inner(calendar, questdb_config) {
        Ok(count) => {
            info!(
                entries = count,
                table = QUESTDB_TABLE_NSE_HOLIDAYS,
                "trading calendar persisted to QuestDB"
            );
            Ok(())
        }
        Err(err) => {
            warn!(
                ?err,
                "QuestDB calendar persistence failed — trading continues"
            );
            Ok(())
        }
    }
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

        let ts_nanos = TimestampNanos::new(midnight_epoch_secs * 1_000_000_000);

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
