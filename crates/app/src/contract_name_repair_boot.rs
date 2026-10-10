//! The once-a-day after-close run of the contract name repair
//! ([`tickvault_storage::contract_name_repair`]).
//!
//! After the close (from [`REPAIR_START_SECS_OF_DAY_IST`], 15:45 IST) the
//! task repairs every day from today back [`REPAIR_LOOKBACK_DAYS`] days whose
//! contract files are still on disk: rows of feed `dhan` that carry no
//! `contract` get the name that day's files give them. It runs once per IST
//! day per process; a boot after the close (the after-close deploy) runs it
//! after [`REPAIR_BOOT_GRACE_SECS`].
//!
//! Why after the close: nothing writes the live tables then, so the repair
//! cannot race a live write, and the INSERT … SELECT it runs (one per table
//! per day) does not compete with the frame drain for QuestDB.
//!
//! Tables: every candle table the fold emits, `ticks` and `feed_aux_packets`.
//! `market_depth` is left out on purpose (about 1.5 billion rows a day).
//!
//! Honest limits:
//! - A day whose files are gone (kept 7 days) cannot be repaired.
//! - A day already copied to the cold bucket keeps blank names in that copy.
//! - Only rows QuestDB has already applied are seen; a row still in the WAL
//!   backlog is repaired by the next day's run, if its files are still kept.
//! - A boot that is still replaying its frame log when the repair runs can
//!   rewrite a row the repair just read; both writes carry the same values and
//!   a name, so the row ends named either way.

use std::time::Duration;

use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_storage::contract_name_repair::{
    CONTRACT_NAME_REPAIR_REQUEST_TIMEOUT_SECS, NameRow, RepairDayTally,
    repair_contract_names_for_day,
};

/// Earliest IST second-of-day the repair runs: five minutes after the last
/// tick is persisted (15:40), so the close's final writes have landed.
pub const REPAIR_START_SECS_OF_DAY_IST: u32 =
    tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST + 300;

/// Days before today the repair also covers. Contract files are kept seven
/// days (`ARTIFACT_RETENTION_DAYS`), so a day older than this has no names to
/// give and is never tried.
pub const REPAIR_LOOKBACK_DAYS: u32 = 7;

/// Wait after boot before the first check, so a boot's own frame-log replay
/// and seal drain are well under way before the repair reads the tables.
pub const REPAIR_BOOT_GRACE_SECS: u64 = 900;

/// How often the task checks whether today's run is due.
const REPAIR_CHECK_INTERVAL_SECS: u64 = 600;

/// `true` when the repair should run now: after the close and not yet run
/// today in this process.
#[must_use]
pub fn repair_is_due(secs_of_day_ist: u32, today: &str, last_run_day: Option<&str>) -> bool {
    secs_of_day_ist >= REPAIR_START_SECS_OF_DAY_IST && last_run_day != Some(today)
}

/// `today` and the [`REPAIR_LOOKBACK_DAYS`] days before it, newest first, as
/// `YYYY-MM-DD`. Empty when `today` does not parse.
#[must_use]
pub fn repair_days(today: &str) -> Vec<String> {
    let Ok(mut day) = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d") else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(REPAIR_LOOKBACK_DAYS as usize + 1);
    for _ in 0..=REPAIR_LOOKBACK_DAYS {
        out.push(day.format("%Y-%m-%d").to_string());
        let Some(prev) = day.pred_opt() else {
            break;
        };
        day = prev;
    }
    out
}

/// The tables the repair visits: every emitted candle table, `ticks`, and
/// `feed_aux_packets`.
#[must_use]
pub fn repair_tables() -> Vec<&'static str> {
    let mut tables = tickvault_storage::shadow_persistence::emitted_candle_table_names();
    tables.push(tickvault_storage::tick_persistence::TICKS_TABLE);
    tables.push(tickvault_storage::feed_aux_persistence::FEED_AUX_PACKETS_TABLE);
    tables
}

/// The `/exec` URL of the configured QuestDB.
#[must_use]
pub fn exec_url(config: &QuestDbConfig) -> String {
    format!("http://{}:{}/exec", config.host, config.http_port)
}

/// Repair every day of [`repair_days`] whose files still read. Returns the
/// summed tally.
async fn run_once(client: &reqwest::Client, url: &str, today: &str) -> RepairDayTally {
    let tables = repair_tables();
    let mut total = RepairDayTally::default();
    for day in repair_days(today) {
        let Some(labels) = crate::dhan_contract_universe::contract_names_for_day(&day) else {
            continue;
        };
        let names: Vec<NameRow<'_>> = labels
            .iter()
            .map(|((security_id, segment), contract)| NameRow {
                security_id: *security_id,
                segment,
                contract,
            })
            .collect();
        let tally = repair_contract_names_for_day(client, url, &day, &names, &tables).await;
        info!(
            day = %day,
            names = names.len(),
            rows_restored = tally.rows_restored,
            tables_repaired = tally.tables_repaired,
            tables_clean = tally.tables_clean,
            tables_skipped = tally.tables_skipped,
            failures = tally.failures,
            "contract name repair: day checked"
        );
        total.rows_restored = total.rows_restored.saturating_add(tally.rows_restored);
        total.tables_repaired += tally.tables_repaired;
        total.tables_clean += tally.tables_clean;
        total.tables_skipped += tally.tables_skipped;
        total.failures += tally.failures;
    }
    total
}

/// Runs forever: after the boot grace, checks every
/// `REPAIR_CHECK_INTERVAL_SECS` and repairs once per IST day after the
/// close. Spawned once per process.
// TEST-EXEMPT: an endless timer loop over a live QuestDB; its decisions are the pure `repair_is_due` / `repair_days` / `repair_tables`, each tested below.
pub async fn run_contract_name_repair_loop(config: QuestDbConfig) {
    let client = match tickvault_storage::http_client::build_probe_client(
        CONTRACT_NAME_REPAIR_REQUEST_TIMEOUT_SECS,
    ) {
        Ok(client) => client,
        Err(err) => {
            warn!(
                ?err,
                "contract name repair: no HTTP client, repair not running"
            );
            return;
        }
    };
    let url = exec_url(&config);
    tokio::time::sleep(Duration::from_secs(REPAIR_BOOT_GRACE_SECS)).await;
    let mut last_run_day: Option<String> = None;
    loop {
        let today = crate::dhan_universe::today_ist_date();
        let now = tickvault_common::market_hours::now_ist_secs_of_day();
        if repair_is_due(now, &today, last_run_day.as_deref()) {
            let total = run_once(&client, &url, &today).await;
            let outcome = if total.failures == 0 { "ok" } else { "partial" };
            metrics::counter!("tv_contract_name_repair_runs_total", "outcome" => outcome)
                .increment(1);
            info!(
                rows_restored = total.rows_restored,
                failures = total.failures,
                outcome,
                "contract name repair: after-close run finished"
            );
            last_run_day = Some(today);
        }
        tokio::time::sleep(Duration::from_secs(REPAIR_CHECK_INTERVAL_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repair_is_due_only_after_the_close_and_once_a_day() {
        let close = REPAIR_START_SECS_OF_DAY_IST;
        assert!(!repair_is_due(close - 1, "2026-10-12", None));
        assert!(repair_is_due(close, "2026-10-12", None));
        assert!(repair_is_due(
            close + 3600,
            "2026-10-12",
            Some("2026-10-09")
        ));
        assert!(!repair_is_due(
            close + 3600,
            "2026-10-12",
            Some("2026-10-12")
        ));
    }

    #[test]
    fn test_repair_start_is_after_the_last_persisted_tick() {
        assert_eq!(REPAIR_START_SECS_OF_DAY_IST, 15 * 3600 + 45 * 60);
    }

    #[test]
    fn test_repair_days_covers_the_incident_day_from_the_next_deploy() {
        // Regression (2026-10-09): blank names written that evening must still
        // be repairable by the run after the Monday 12 Oct deploy.
        let days = repair_days("2026-10-12");
        assert_eq!(days.len(), REPAIR_LOOKBACK_DAYS as usize + 1);
        assert_eq!(days.first().map(String::as_str), Some("2026-10-12"));
        assert!(days.iter().any(|d| d == "2026-10-09"));
        assert_eq!(days.last().map(String::as_str), Some("2026-10-05"));
    }

    #[test]
    fn test_repair_days_refuses_a_bad_date() {
        assert!(repair_days("12-10-2026").is_empty());
    }

    #[test]
    fn test_repair_tables_include_candles_ticks_and_aux_but_not_depth() {
        let tables = repair_tables();
        assert!(tables.contains(&"candles_1m"));
        assert!(tables.contains(&"ticks"));
        assert!(tables.contains(&"feed_aux_packets"));
        assert!(!tables.iter().any(|t| t.contains("depth")));
    }

    #[test]
    fn test_run_contract_name_repair_loop_is_spawned_once_from_main() {
        let main = include_str!("main.rs");
        let spawns = main
            .matches("contract_name_repair_boot::run_contract_name_repair_loop(")
            .count();
        assert_eq!(
            spawns, 1,
            "the after-close repair must be spawned exactly once per process"
        );
    }

    #[test]
    fn test_exec_url_uses_the_configured_host_and_port() {
        let config = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        assert_eq!(exec_url(&config), "http://tv-questdb:9000/exec");
    }
}
