//! The once-a-day after-close run of the contract name repair
//! ([`tickvault_storage::contract_name_repair`]).
//!
//! After the close (from [`REPAIR_START_SECS_OF_DAY_IST`], 15:45 IST) the
//! task repairs every day from today back [`REPAIR_LOOKBACK_DAYS`] days whose
//! contract files are still on disk: rows of feed `dhan` that carry no
//! `contract` get the name that day's files give them. It runs once per IST
//! day per process, and up to [`REPAIR_MAX_ATTEMPTS_PER_DAY`] times when a
//! run hits a failure; a boot after the close (the after-close deploy) runs it
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
//! - A table is repaired only once QuestDB has applied every write to it and
//!   no new write has arrived for a poll interval; a table still being written
//!   is left for the next attempt.
//! - A writer that starts while a repair statement runs can still have its
//!   newer copy of a row replaced by the stored copy the statement read (see
//!   the storage module's limits). The boot grace keeps the after-deploy run
//!   clear of the boot's own frame-log replay.

use std::time::Duration;

use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_storage::contract_name_repair::{
    CONTRACT_NAME_REPAIR_REQUEST_TIMEOUT_SECS, NameRow, RepairDayTally, begin_repair_run,
    repair_contract_names_for_day,
};

/// Earliest IST second-of-day the repair runs: five minutes after the last
/// tick is persisted (15:40), so the close's final writes have landed.
pub const REPAIR_START_SECS_OF_DAY_IST: u32 =
    tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST + 300;

/// Days before today the repair also covers. Contract files are kept seven
/// days (`ARTIFACT_RETENTION_DAYS`), so a day older than this has no names to
/// give and is never tried. The partition archiver locks only partitions of
/// these days for the repair, so it reads the same storage constant.
pub const REPAIR_LOOKBACK_DAYS: u32 =
    tickvault_storage::contract_name_repair::CONTRACT_NAME_REPAIR_LOOKBACK_DAYS;

/// Wait after boot before the first check, so a boot's own frame-log replay
/// and seal drain have finished before the repair reads the tables (the 9 Oct
/// evening boots took about 16 minutes to replay). The repair also waits per
/// table until QuestDB reports it quiet.
pub const REPAIR_BOOT_GRACE_SECS: u64 = 1800;

/// Runs per IST day per process. A run with no failure ends the day; a run
/// with one is tried again at the next check, up to this many times.
pub const REPAIR_MAX_ATTEMPTS_PER_DAY: u32 = 3;

/// How often the task checks whether today's run is due.
const REPAIR_CHECK_INTERVAL_SECS: u64 = 600;

/// What this process has done today.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepairRunState {
    /// The IST day the counts below belong to.
    pub day: Option<String>,
    /// Runs started on `day`.
    pub attempts: u32,
    /// A run on `day` ended with no failure.
    pub finished: bool,
}

impl RepairRunState {
    /// Record a run on `today` that ended with `failures`.
    pub fn record(&mut self, today: &str, failures: u32) {
        if self.day.as_deref() != Some(today) {
            *self = Self {
                day: Some(today.to_string()),
                ..Self::default()
            };
        }
        self.attempts = self.attempts.saturating_add(1);
        self.finished = self.finished || failures == 0;
    }
}

/// The IST days (days since 1970-01-01) whose after-close repair this
/// process ended: `finished` for a run with no failure, `unclean` for a day
/// whose attempts ran out with a failure left. Read by the daily Brutex
/// export (owner Quote 60), which never runs the session day before the
/// repair ended, and names an unclean one in its manifest (2026-11-14
/// review: an exhausted day had read as cleanly finished, so a day could be
/// sealed complete with blank contract names).
pub(crate) struct RepairDayCells {
    finished: std::sync::atomic::AtomicI64,
    unclean: std::sync::atomic::AtomicI64,
}

impl RepairDayCells {
    /// Nothing ended yet.
    pub(crate) const fn new() -> Self {
        Self {
            finished: std::sync::atomic::AtomicI64::new(i64::MIN),
            unclean: std::sync::atomic::AtomicI64::new(i64::MIN),
        }
    }
}

/// This process's repair days.
static REPAIR_DAYS: RepairDayCells = RepairDayCells::new();

/// How a day's after-close name repair stands in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairDayState {
    /// Still running, or not run yet.
    Pending,
    /// A run ended with no failure.
    Clean,
    /// Every attempt ran and a failure is left: names may still be blank.
    GaveUpWithFailures,
}

/// Days since 1970-01-01 of an IST `YYYY-MM-DD`; `None` for a bad date.
fn repair_day_key(day: &str) -> Option<i64> {
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .map(|d| {
            d.signed_duration_since(chrono::NaiveDate::default())
                .num_days()
        })
}

/// Records that `today`'s repair is over for this process when `state` says
/// so: finished cleanly, or out of attempts with a failure left. O(1).
fn note_repair_over(cells: &RepairDayCells, today: &str, state: &RepairRunState) {
    let Some(key) = repair_day_key(today) else {
        return;
    };
    if state.finished {
        cells
            .finished
            .fetch_max(key, std::sync::atomic::Ordering::Relaxed);
    } else if state.attempts >= REPAIR_MAX_ATTEMPTS_PER_DAY {
        cells
            .unclean
            .fetch_max(key, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Records one finished run of `today` and, when the day is over for this
/// process, publishes it to the Brutex export's readiness gate (owner
/// Quote 60). The repair loop's only bookkeeping call (pinned by
/// `test_the_repair_loop_publishes_through_record_run`). O(1).
fn record_run(cells: &RepairDayCells, state: &mut RepairRunState, today: &str, failures: u32) {
    state.record(today, failures);
    note_repair_over(cells, today, state);
}

/// `day`'s state in `cells`. A clean run wins over an earlier exhaustion of
/// the same day. O(1).
fn state_in(cells: &RepairDayCells, day: chrono::NaiveDate) -> RepairDayState {
    let key = day
        .signed_duration_since(chrono::NaiveDate::default())
        .num_days();
    if cells.finished.load(std::sync::atomic::Ordering::Relaxed) == key {
        RepairDayState::Clean
    } else if cells.unclean.load(std::sync::atomic::Ordering::Relaxed) == key {
        RepairDayState::GaveUpWithFailures
    } else {
        RepairDayState::Pending
    }
}

/// How `day`'s names stand against this process's repair runs, counting a
/// run of a LATER day whose lookback reaches it (2026-11-14 third review):
/// the newest cleanly finished run of a day from `day` to
/// [`REPAIR_LOOKBACK_DAYS`] after it makes `day` `Clean` (it named every
/// blank its files can name, so no later run rewrites it); else the newest
/// exhausted run in that reach makes it `GaveUpWithFailures`; else
/// `Pending` (a run still to come may rewrite it). The cells keep only the
/// newest day of each kind, so an older clean run beside a newer one out of
/// reach reads `Pending` (the direction that waits). O(1).
fn covered_state_in(cells: &RepairDayCells, day: chrono::NaiveDate) -> RepairDayState {
    let key = day
        .signed_duration_since(chrono::NaiveDate::default())
        .num_days();
    let in_reach = |run: i64| run >= key && run - key <= i64::from(REPAIR_LOOKBACK_DAYS);
    if in_reach(cells.finished.load(std::sync::atomic::Ordering::Relaxed)) {
        RepairDayState::Clean
    } else if in_reach(cells.unclean.load(std::sync::atomic::Ordering::Relaxed)) {
        RepairDayState::GaveUpWithFailures
    } else {
        RepairDayState::Pending
    }
}

/// [`covered_state_in`] on this process's repair days (owner Quote 60):
/// what the Brutex export reads for a catch-up day. O(1).
#[must_use]
// TEST-EXEMPT: a load of the process cells; the rule is test_covered_state_counts_a_later_run_that_reaches_the_day
pub fn repair_covered_state(day: chrono::NaiveDate) -> RepairDayState {
    covered_state_in(&REPAIR_DAYS, day)
}

/// How this process's after-close name repair of `day` stands (owner
/// Quote 60). O(1).
#[must_use]
// TEST-EXEMPT: a load of the process cells; the rule is test_repair_state_is_set_only_when_the_day_is_over
pub fn repair_state_for(day: chrono::NaiveDate) -> RepairDayState {
    state_in(&REPAIR_DAYS, day)
}

/// `true` when the repair should run now: after the close, and today has
/// neither finished nor used up [`REPAIR_MAX_ATTEMPTS_PER_DAY`].
#[must_use]
pub fn repair_is_due(secs_of_day_ist: u32, today: &str, state: &RepairRunState) -> bool {
    if secs_of_day_ist < REPAIR_START_SECS_OF_DAY_IST {
        return false;
    }
    if state.day.as_deref() != Some(today) {
        return true;
    }
    !state.finished && state.attempts < REPAIR_MAX_ATTEMPTS_PER_DAY
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
/// summed tally. The run's days are published to the partition archiver for
/// as long as it lasts ([`begin_repair_run`]), so a run that crosses IST
/// midnight keeps its oldest day locked.
async fn run_once(client: &reqwest::Client, url: &str, today: &str) -> RepairDayTally {
    let _run_days = begin_repair_run(today);
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
            names_refused = tally.names_refused,
            "contract name repair: day checked"
        );
        total.rows_restored = total.rows_restored.saturating_add(tally.rows_restored);
        total.tables_repaired += tally.tables_repaired;
        total.tables_clean += tally.tables_clean;
        total.tables_skipped += tally.tables_skipped;
        total.failures += tally.failures;
        total.names_refused = total.names_refused.saturating_add(tally.names_refused);
    }
    total
}

/// The outcome label of a finished run.
#[must_use]
pub fn run_outcome(failures: u32) -> &'static str {
    if failures == 0 { "ok" } else { "partial" }
}

/// Runs forever: after the boot grace, checks every
/// `REPAIR_CHECK_INTERVAL_SECS` and repairs once per IST day after the
/// close (retrying a run that hit a failure). Spawned once per process.
// TEST-EXEMPT: an endless timer loop over a live QuestDB; its decisions are the pure `repair_is_due` / `RepairRunState::record` / `run_outcome` / `repair_days` / `repair_tables`, each tested below.
pub async fn run_contract_name_repair_loop(config: QuestDbConfig) {
    // Registered at zero so a first failure is a rise from 0, not a new series.
    for outcome in ["ok", "partial"] {
        metrics::counter!("tv_contract_name_repair_runs_total", "outcome" => outcome).increment(0);
    }
    metrics::counter!("tv_contract_name_repair_rows_total").increment(0);
    let url = exec_url(&config);
    tokio::time::sleep(Duration::from_secs(REPAIR_BOOT_GRACE_SECS)).await;
    let mut client: Option<reqwest::Client> = None;
    let mut state = RepairRunState::default();
    loop {
        if client.is_none() {
            match tickvault_storage::http_client::build_probe_client(
                CONTRACT_NAME_REPAIR_REQUEST_TIMEOUT_SECS,
            ) {
                Ok(built) => client = Some(built),
                Err(err) => warn!(
                    ?err,
                    "contract name repair: no HTTP client yet, trying again at the next check"
                ),
            }
        }
        let today = crate::dhan_universe::today_ist_date();
        let now = tickvault_common::market_hours::now_ist_secs_of_day();
        if let Some(client) = client.as_ref()
            && repair_is_due(now, &today, &state)
        {
            let total = run_once(client, &url, &today).await;
            let outcome = run_outcome(total.failures);
            metrics::counter!("tv_contract_name_repair_runs_total", "outcome" => outcome)
                .increment(1);
            record_run(&REPAIR_DAYS, &mut state, &today, total.failures);
            info!(
                rows_restored = total.rows_restored,
                failures = total.failures,
                attempt = state.attempts,
                outcome,
                "contract name repair: after-close run finished"
            );
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
        let fresh = RepairRunState::default();
        assert!(!repair_is_due(close - 1, "2026-10-12", &fresh));
        assert!(repair_is_due(close, "2026-10-12", &fresh));
        let mut done = RepairRunState::default();
        done.record("2026-10-09", 0);
        assert!(repair_is_due(close + 3600, "2026-10-12", &done));
        done.record("2026-10-12", 0);
        assert!(!repair_is_due(close + 3600, "2026-10-12", &done));
    }

    #[test]
    fn test_a_failed_run_is_retried_up_to_the_cap() {
        // Regression (review 2026-10-10): a failed run used to end the day.
        let close = REPAIR_START_SECS_OF_DAY_IST;
        let mut state = RepairRunState::default();
        for attempt in 1..=REPAIR_MAX_ATTEMPTS_PER_DAY {
            assert!(repair_is_due(close, "2026-10-12", &state), "{attempt}");
            state.record("2026-10-12", 1);
        }
        assert!(!repair_is_due(close, "2026-10-12", &state));
        // A new day starts over.
        assert!(repair_is_due(close, "2026-10-13", &state));
        state.record("2026-10-13", 1);
        assert_eq!(state.attempts, 1);
        state.record("2026-10-13", 0);
        assert!(state.finished);
        assert!(!repair_is_due(close, "2026-10-13", &state));
    }

    /// Owner Quote 60 (and the 2026-11-14 review): the export reads the
    /// repair as over only once a run finished or the attempts ran out, and
    /// tells the two apart.
    #[test]
    fn test_repair_state_is_set_only_when_the_day_is_over() {
        let cells = RepairDayCells::new();
        let day = chrono::NaiveDate::from_ymd_opt(2031, 1, 7).expect("date");
        let mut state = RepairRunState::default();
        record_run(&cells, &mut state, "2031-01-07", 2);
        assert_eq!(
            state_in(&cells, day),
            RepairDayState::Pending,
            "one failure is not the end"
        );
        record_run(&cells, &mut state, "2031-01-07", 0);
        assert_eq!(state_in(&cells, day), RepairDayState::Clean);
        assert_eq!(
            state_in(&cells, day.pred_opt().expect("d")),
            RepairDayState::Pending
        );
        assert_eq!(repair_day_key("bad"), None);
    }

    /// Regression (2026-11-14 third review, MEDIUM): a catch-up day reads
    /// clean once a later run whose lookback reaches it finished cleanly;
    /// before any such run it is pending (a run to come may rewrite it).
    #[test]
    fn test_covered_state_counts_a_later_run_that_reaches_the_day() {
        let cells = RepairDayCells::new();
        let past = chrono::NaiveDate::from_ymd_opt(2031, 1, 10).expect("date");
        assert_eq!(covered_state_in(&cells, past), RepairDayState::Pending);
        let mut state = RepairRunState::default();
        record_run(&cells, &mut state, "2031-01-13", 0);
        assert_eq!(covered_state_in(&cells, past), RepairDayState::Clean);
        assert_eq!(
            covered_state_in(&cells, past.succ_opt().expect("d")),
            RepairDayState::Clean
        );
        let later = chrono::NaiveDate::from_ymd_opt(2031, 1, 14).expect("d");
        assert_eq!(
            covered_state_in(&cells, later),
            RepairDayState::Pending,
            "a later day than the run"
        );
        let early = chrono::NaiveDate::from_ymd_opt(2031, 1, 5).expect("d");
        assert_eq!(
            covered_state_in(&cells, early),
            RepairDayState::Pending,
            "out of the run's reach"
        );
        let cells = RepairDayCells::new();
        let mut state = RepairRunState::default();
        for _ in 0..REPAIR_MAX_ATTEMPTS_PER_DAY {
            record_run(&cells, &mut state, "2031-01-12", 1);
        }
        assert_eq!(
            covered_state_in(&cells, past),
            RepairDayState::GaveUpWithFailures
        );
        assert_eq!(
            repair_covered_state(chrono::NaiveDate::from_ymd_opt(1999, 1, 1).expect("d")),
            RepairDayState::Pending
        );
    }

    /// Regression (2026-11-14 review, MEDIUM): a day whose attempts all ran
    /// with a failure left is NOT cleanly finished: the export names it.
    #[test]
    fn test_exhausted_repair_with_failures_is_not_clean() {
        let cells = RepairDayCells::new();
        let day = chrono::NaiveDate::from_ymd_opt(2031, 1, 8).expect("date");
        let mut state = RepairRunState::default();
        for _ in 0..REPAIR_MAX_ATTEMPTS_PER_DAY {
            record_run(&cells, &mut state, "2031-01-08", 1);
        }
        assert_eq!(state_in(&cells, day), RepairDayState::GaveUpWithFailures);
    }

    /// The loop publishes through `record_run` on the process cells (a
    /// revert to a bare `state.record` would leave the export gate forever
    /// pending).
    #[test]
    fn test_the_repair_loop_publishes_through_record_run() {
        let src = include_str!("contract_name_repair_boot.rs");
        let body = src
            .split("pub async fn run_contract_name_repair_loop")
            .nth(1)
            .and_then(|rest| rest.split("#[cfg(test)]").next())
            .expect("loop body");
        assert!(
            body.contains("record_run(&REPAIR_DAYS, &mut state, &today, total.failures)"),
            "the loop must publish each run through record_run"
        );
        assert!(
            !body.contains("state.record("),
            "no bare state.record in the loop"
        );
        assert_eq!(
            repair_state_for(chrono::NaiveDate::from_ymd_opt(1999, 1, 1).expect("d")),
            RepairDayState::Pending
        );
    }

    #[test]
    fn test_run_outcome_labels() {
        assert_eq!(run_outcome(0), "ok");
        assert_eq!(run_outcome(2), "partial");
    }

    #[test]
    fn test_boot_grace_outlasts_the_measured_replay() {
        // The 9 Oct 22:29 boot replayed 2,079,899 frames in about 16 minutes.
        assert!(REPAIR_BOOT_GRACE_SECS >= 16 * 60 * 3 / 2);
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

    /// Recheck of PR AA finding 1: the archiver holds the quiesce lock across
    /// the day path of exactly the tables the repair can rewrite. A table
    /// added here and not there would be rewritten between an export and a
    /// drop again.
    #[test]
    fn test_every_repaired_table_is_locked_by_the_archiver() {
        for table in repair_tables() {
            assert!(
                tickvault_storage::contract_name_repair::repair_may_rewrite(table),
                "{table} is repaired but the archiver would not hold the repair out"
            );
        }
    }

    /// Recheck of PR AA finding 1 (narrowed): the archiver locks a partition
    /// for the repair only when its day is one the repair visits. Every day
    /// `repair_days` returns must be locked; a day well before them is not.
    #[test]
    fn test_every_repaired_day_is_locked_by_the_archiver() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 8).expect("date");
        for day in repair_days("2026-11-08") {
            for table in repair_tables() {
                let hour = format!("{day}T10");
                assert!(
                    tickvault_storage::contract_name_repair::repair_may_rewrite_partition(
                        table, &day, today
                    ) && tickvault_storage::contract_name_repair::repair_may_rewrite_partition(
                        table, &hour, today
                    ),
                    "{table} {day} is repaired but the archiver would not hold the repair out"
                );
            }
        }
        assert!(
            !tickvault_storage::contract_name_repair::repair_may_rewrite_partition(
                "ticks",
                "2026-10-20T10",
                today
            )
        );
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
