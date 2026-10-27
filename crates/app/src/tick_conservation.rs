//! Daily tick conservation check and the boot crash-gap row (plan item K4,
//! 2026-10-26).
//!
//! ## 1. Daily check (once per trading day, after the persist window)
//!
//! At [`TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST`] (15:45 IST, five minutes
//! after `TICK_PERSIST_END_SECS_OF_DAY_IST` closes the `ticks` window) it
//! compares:
//!
//! - **accepted**: `ticks` rows the Dhan writer appended for today, from the
//!   in-process ledger (`tickvault_storage::tick_conservation_ledger`, one
//!   relaxed add per accepted row);
//! - **database**: `select count() from ticks where ts in '<today>' and
//!   feed = 'dhan'`, through the shared QuestDB client builder with a
//!   timeout;
//! - **not landed**: rows already counted as spilled, failed or handed to the
//!   WAL replay (the same sites that increment `tv_ticks_dropped_total`).
//!
//! and writes one `tick_conservation_audit` row. Verdicts
//! ([`classify`]):
//!
//! | verdict | when |
//! |---|---|
//! | `unverified` | the count could not be read (error, timeout, bad body), or nothing was accepted and nothing is stored (a zero baseline proves nothing) |
//! | `partial` | this process started after 09:00 IST today (or on another day): it only knows its own rows, so no mismatch is claimed |
//! | `match` | `accepted == database`, or the shortfall is within the rows counted as not landed |
//! | `mismatch` | the database holds MORE rows than accepted, or the shortfall exceeds every counted loss |
//!
//! An `unverified` or `mismatch` read is retried up to
//! [`TICK_CONSERVATION_MAX_READS`] times, [`TICK_CONSERVATION_READ_SPACING_SECS`]
//! apart, because an acknowledged row is only countable once QuestDB has
//! applied its WAL. A final `mismatch` logs one coded `error!`
//! (`TICK-CONSERVE-01`), log only: no CloudWatch filter, no EMF name, no
//! alarm, no Telegram (Dhan noise lock).
//!
//! ## 2. Boot crash-gap row
//!
//! At boot, inside the session (09:15 to 15:40 IST) on a trading day, the
//! downtime since the previous process's last WAL receipt is written as one
//! `feed_gap_audit` row: `reason = process_down`, `ws_type = main_feed`,
//! `connection_index = -1` (the whole process, never a socket slot). The
//! table's DEDUP key is unchanged. Outside the window, with no previous
//! frame, or with a gap at or under five seconds, nothing is written. A
//! previous frame from before today's 09:15 open starts the gap at the open.
//!
//! ## Where it runs
//!
//! Its own tokio task, never the frame drain. Market-hours aware: the daily
//! check sleeps until 15:45 IST on a trading day and skips the day when the
//! process starts at or after it.
//!
//! ## Honest limits
//!
//! - The not-landed total is per process and can include `feed_aux_packets`
//!   rows that shared a rescued batch, so it is a tolerance, not an exact
//!   term; a real loss smaller than the counted spills reads as `match`.
//! - Rows the boot WAL replay of a mid-day restart re-appends would be counted
//!   twice against a database that already holds them; that process is
//!   `partial` by construction, which is why `partial` never claims a
//!   mismatch.
//! - The session close is 15:40 IST since the 2026-08-03 NSE change, so the
//!   crash-gap window is 09:15 to 15:40, not 15:30.

use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDate;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    IST_UTC_OFFSET_NANOS, MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::ws_event_types::{WS_EVENT_NO_DHAN_CODE, WsType};
use tickvault_storage::feed_gap_audit_persistence::FeedGapRow;
use tickvault_storage::tick_conservation_audit_persistence::TickConservationRow;
use tickvault_storage::tick_conservation_ledger::TickConservationLedger;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_DAY: i64 = 86_400 * NANOS_PER_SEC;

/// IST seconds-of-day of the daily check: 15:45:00, five minutes after the
/// `ticks` persist window closes (so the last flushes have landed).
pub const TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST: u32 = TICK_PERSIST_END_SECS_OF_DAY_IST + 300;

/// Database count attempts per day.
pub const TICK_CONSERVATION_MAX_READS: u32 = 3;

/// Seconds between count attempts (WAL apply lag can hold rows back).
pub const TICK_CONSERVATION_READ_SPACING_SECS: u64 = 120;

/// Timeout on one count query.
pub const TICK_COUNT_QUERY_TIMEOUT_SECS: u64 = 30;

/// A gap at or under this is a restart, not downtime worth a row (the WAL
/// replay's own process-boundary threshold).
pub const PROCESS_DOWN_MIN_GAP_NANOS: i64 =
    tickvault_storage::ws_frame_spill::WAL_REPLAY_PROCESS_GAP_NANOS;

/// `feed_gap_audit.reason` for the boot downtime row.
pub const PROCESS_DOWN_REASON: &str = "process_down";

/// `feed_gap_audit.connection_index` for the boot downtime row: the whole
/// process, distinct from every socket slot (0..32).
pub const PROCESS_DOWN_CONNECTION_INDEX: i64 = -1;

// The check must fire after the window closes and before the 16:30 daily stop.
const _: () = assert!(TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST > TICK_PERSIST_END_SECS_OF_DAY_IST);
const _: () = assert!(TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST < 16 * 3600);

/// The outcome of one daily check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConservationVerdict {
    /// Every accepted row is stored or counted as not landed.
    Match,
    /// Rows are unaccounted for, or the database holds extra rows.
    Mismatch,
    /// The database count was not read, or there was nothing to compare.
    Unverified,
    /// This process did not cover the whole day; it never claims a mismatch.
    Partial,
}

impl ConservationVerdict {
    /// The `verdict` SYMBOL value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Mismatch => "mismatch",
            Self::Unverified => "unverified",
            Self::Partial => "partial",
        }
    }
}

/// What one check compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConservationInputs {
    /// Rows this process accepted for the day.
    pub accepted_rows: u64,
    /// Rows counted as spilled, failed or deferred to the WAL replay.
    pub not_landed_rows: u64,
    /// Rows QuestDB holds for the day; `None` when the count was not read.
    pub db_rows: Option<u64>,
    /// `true` when this process started before 09:00 IST on the day.
    pub full_coverage: bool,
}

/// Classifies one check. Pure, O(1). See the module table.
#[must_use]
pub const fn classify(i: ConservationInputs) -> (ConservationVerdict, &'static str) {
    let Some(db) = i.db_rows else {
        return (ConservationVerdict::Unverified, "count_unreadable");
    };
    if !i.full_coverage {
        return (ConservationVerdict::Partial, "process_started_mid_day");
    }
    if i.accepted_rows == 0 && db == 0 {
        return (ConservationVerdict::Unverified, "no_rows");
    }
    if db > i.accepted_rows {
        return (ConservationVerdict::Mismatch, "extra_db_rows");
    }
    let shortfall = i.accepted_rows - db;
    if shortfall == 0 {
        (ConservationVerdict::Match, "exact")
    } else if shortfall <= i.not_landed_rows {
        (ConservationVerdict::Match, "within_counted_losses")
    } else {
        (ConservationVerdict::Mismatch, "unexplained_shortfall")
    }
}

/// `true` when a process that started at `process_start_ist_nanos` saw the
/// whole persist window of the IST day containing `day_ist_nanos`: it started
/// that same day, before 09:00 IST. Pure.
#[must_use]
pub const fn process_covers_day(process_start_ist_nanos: i64, day_ist_nanos: i64) -> bool {
    let start_day = process_start_ist_nanos.div_euclid(NANOS_PER_DAY);
    let day = day_ist_nanos.div_euclid(NANOS_PER_DAY);
    let start_secs_of_day = process_start_ist_nanos.rem_euclid(NANOS_PER_DAY) / NANOS_PER_SEC;
    start_day == day && start_secs_of_day < TICK_PERSIST_START_SECS_OF_DAY_IST as i64
}

/// Seconds to sleep before today's check, or `None` when the check time has
/// already passed (the day is skipped). Pure.
#[must_use]
pub const fn secs_until_check(now_secs_of_day_ist: u32) -> Option<u64> {
    if now_secs_of_day_ist < TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST {
        Some((TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST - now_secs_of_day_ist) as u64)
    } else {
        None
    }
}

/// The count query for `day` (IST date). Pure.
#[must_use]
pub fn tick_count_sql(day: NaiveDate) -> String {
    format!(
        "select count() from ticks where ts in '{}' and feed = '{}'",
        day.format("%Y-%m-%d"),
        Feed::Dhan.as_str()
    )
}

/// The `feed_gap_audit` row for the downtime before this boot, or `None`
/// (see the module header). Inputs are UTC epoch nanos. Pure.
#[must_use]
pub fn process_down_gap_row(
    previous_receipt_utc_nanos: Option<i64>,
    now_utc_nanos: i64,
    is_trading_day: bool,
) -> Option<FeedGapRow> {
    if !is_trading_day {
        return None;
    }
    let previous = previous_receipt_utc_nanos?;
    let now_ist = now_utc_nanos.saturating_add(IST_UTC_OFFSET_NANOS);
    let day_start = now_ist - now_ist.rem_euclid(NANOS_PER_DAY);
    let now_of_day = now_ist - day_start;
    if !(MARKET_OPEN_IST_NANOS..MARKET_CLOSE_IST_NANOS).contains(&now_of_day) {
        return None;
    }
    let start = previous
        .saturating_add(IST_UTC_OFFSET_NANOS)
        .max(day_start + MARKET_OPEN_IST_NANOS);
    if now_ist.saturating_sub(start) <= PROCESS_DOWN_MIN_GAP_NANOS {
        return None;
    }
    Some(FeedGapRow {
        gap_start_ist_nanos: start,
        gap_end_ist_nanos: now_ist,
        feed: Feed::Dhan,
        ws_type: WsType::MainFeed,
        connection_index: PROCESS_DOWN_CONNECTION_INDEX,
        reason: PROCESS_DOWN_REASON,
        dhan_code: WS_EVENT_NO_DHAN_CODE,
        instruments_held: 0,
        attempts: 0,
        open_at_shutdown: false,
    })
}

fn now_utc_nanos() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
}

fn ist_date_of(ist_nanos: i64) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp(ist_nanos.div_euclid(NANOS_PER_SEC), 0)
        .map(|dt| dt.date_naive())
}

fn secs_of_day(ist_nanos: i64) -> u32 {
    u32::try_from(ist_nanos.rem_euclid(NANOS_PER_DAY) / NANOS_PER_SEC).unwrap_or(0)
}

/// Spawns the boot crash-gap write and the daily check on their own task.
/// Returns the task handle.
// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested pure parts (classify, process_covers_day, secs_until_check, process_down_gap_row, tick_count_sql); spawn site pinned by test_main_wires_spawn_tick_conservation.
pub fn spawn_tick_conservation(
    questdb: QuestDbConfig,
    calendar: Arc<TradingCalendar>,
    process_start_ist_nanos: i64,
    previous_receipt_utc_nanos: Option<i64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        write_process_down_row(&questdb, &calendar, previous_receipt_utc_nanos).await;
        run_daily_checks(&questdb, &calendar, process_start_ist_nanos).await;
    })
}

async fn write_process_down_row(
    questdb: &QuestDbConfig,
    calendar: &TradingCalendar,
    previous_receipt_utc_nanos: Option<i64>,
) {
    use tickvault_storage::feed_gap_audit_persistence::{
        FeedGapAuditWriter, ensure_feed_gap_audit_table,
    };
    let now = now_utc_nanos();
    let today = ist_date_of(now.saturating_add(IST_UTC_OFFSET_NANOS));
    let trading = today.is_some_and(|d| calendar.is_trading_day(d));
    let Some(row) = process_down_gap_row(previous_receipt_utc_nanos, now, trading) else {
        return;
    };
    ensure_feed_gap_audit_table(questdb).await;
    let mut writer = FeedGapAuditWriter::new(questdb);
    let written = writer
        .append_row(&row)
        .and_then(|()| crate::order_observability::blocking_flush(|| writer.flush()));
    match written {
        Ok(()) => info!(
            down_secs = row.down_secs(),
            reason = PROCESS_DOWN_REASON,
            "feed_gap_audit: the downtime before this mid-session boot was recorded"
        ),
        Err(err) => error!(
            code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
            stage = "process_down_gap_row",
            down_secs = row.down_secs(),
            ?err,
            "feed_gap_audit: the downtime row for this mid-session boot was not written"
        ),
    }
}

async fn run_daily_checks(
    questdb: &QuestDbConfig,
    calendar: &TradingCalendar,
    process_start_ist_nanos: i64,
) {
    use tickvault_storage::tick_conservation_audit_persistence::ensure_tick_conservation_audit_table;
    ensure_tick_conservation_audit_table(questdb).await;
    loop {
        let now_ist = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);
        let today = ist_date_of(now_ist);
        let trading = today.is_some_and(|d| calendar.is_trading_day(d));
        if let (true, Some(day), Some(wait)) =
            (trading, today, secs_until_check(secs_of_day(now_ist)))
        {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            run_one_check(questdb, day, now_ist, process_start_ist_nanos).await;
        }
        // Sleep to just past the next IST midnight (a multi-day process).
        let now_ist = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);
        let to_midnight = NANOS_PER_DAY - now_ist.rem_euclid(NANOS_PER_DAY);
        let secs = u64::try_from(to_midnight / NANOS_PER_SEC).unwrap_or(0) + 5;
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

async fn read_tick_count(day: NaiveDate, questdb: &QuestDbConfig) -> Option<u64> {
    let client =
        tickvault_storage::http_client::build_probe_client(TICK_COUNT_QUERY_TIMEOUT_SECS).ok()?;
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let sql = tick_count_sql(day);
    let resp = tokio::time::timeout(
        Duration::from_secs(TICK_COUNT_QUERY_TIMEOUT_SECS),
        client.get(&url).query(&[("query", sql.as_str())]).send(),
    )
    .await
    .ok()?
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    crate::feed_scoreboard_boot::parse_questdb_count(&body).and_then(|n| u64::try_from(n).ok())
}

async fn run_one_check(
    questdb: &QuestDbConfig,
    day: NaiveDate,
    day_ist_nanos: i64,
    process_start_ist_nanos: i64,
) {
    let ledger = tickvault_storage::tick_conservation_ledger::ledger();
    let ledger_day = TickConservationLedger::ist_day_of(day_ist_nanos);
    let full_coverage = process_covers_day(process_start_ist_nanos, day_ist_nanos);
    let mut reads = 0_u32;
    let (inputs, verdict, reason) = loop {
        reads += 1;
        let inputs = ConservationInputs {
            accepted_rows: ledger.accepted_rows_on(ledger_day),
            not_landed_rows: ledger.rows_not_landed(),
            db_rows: read_tick_count(day, questdb).await,
            full_coverage,
        };
        let (verdict, reason) = classify(inputs);
        let settled = matches!(
            verdict,
            ConservationVerdict::Match | ConservationVerdict::Partial
        ) || reason == "no_rows";
        if settled || reads >= TICK_CONSERVATION_MAX_READS {
            break (inputs, verdict, reason);
        }
        tokio::time::sleep(Duration::from_secs(TICK_CONSERVATION_READ_SPACING_SECS)).await;
    };
    let checked_at = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);
    let row = TickConservationRow {
        checked_at_ist_nanos: checked_at,
        day_ist_nanos,
        feed: Feed::Dhan,
        verdict: verdict.as_str(),
        reason,
        accepted_rows: i64::try_from(inputs.accepted_rows).unwrap_or(i64::MAX),
        db_tick_rows: inputs
            .db_rows
            .map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX)),
        not_landed_rows: i64::try_from(inputs.not_landed_rows).unwrap_or(i64::MAX),
        count_reads: i64::from(reads),
        process_start_ist_nanos,
    };
    report(&row, inputs);
    persist(questdb, &row).await;
}

/// Logs the verdict: a coded `error!` on `mismatch` (log only), a `warn!` on
/// `unverified`, `info!` otherwise.
fn report(row: &TickConservationRow, inputs: ConservationInputs) {
    match row.verdict {
        "mismatch" => error!(
            code = ErrorCode::TickConserve01DailyMismatch.code_str(),
            reason = row.reason,
            accepted_rows = inputs.accepted_rows,
            db_tick_rows = row.db_tick_rows,
            not_landed_rows = inputs.not_landed_rows,
            shortfall = row.shortfall(),
            count_reads = row.count_reads,
            "TICK-CONSERVE-01: today's ticks rows do not reconcile with the rows this \
             process accepted, after every counted spill, failure and WAL deferral"
        ),
        "unverified" => warn!(
            reason = row.reason,
            accepted_rows = inputs.accepted_rows,
            count_reads = row.count_reads,
            "tick conservation: today's check is unverified (the database count was not \
             read, or there was nothing to compare)"
        ),
        _ => info!(
            verdict = row.verdict,
            reason = row.reason,
            accepted_rows = inputs.accepted_rows,
            db_tick_rows = row.db_tick_rows,
            not_landed_rows = inputs.not_landed_rows,
            "tick conservation: daily check done"
        ),
    }
}

async fn persist(questdb: &QuestDbConfig, row: &TickConservationRow) {
    use tickvault_storage::tick_conservation_audit_persistence::TickConservationAuditWriter;
    let mut writer = TickConservationAuditWriter::new(questdb);
    let written = writer
        .append_row(row)
        .and_then(|()| crate::order_observability::blocking_flush(|| writer.flush()));
    if let Err(err) = written {
        error!(
            code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
            stage = "tick_conservation_audit_write",
            verdict = row.verdict,
            ?err,
            "tick_conservation_audit: today's row was not written (the verdict is in this log)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-26 00:00 IST, in IST-epoch nanos.
    const DAY: i64 = 1_792_972_800_000_000_000;

    fn at(secs_of_day: i64) -> i64 {
        DAY + secs_of_day * NANOS_PER_SEC
    }

    fn inputs(accepted: u64, not_landed: u64, db: Option<u64>) -> ConservationInputs {
        ConservationInputs {
            accepted_rows: accepted,
            not_landed_rows: not_landed,
            db_rows: db,
            full_coverage: true,
        }
    }

    #[test]
    fn test_classify_exact_match() {
        assert_eq!(
            classify(inputs(1_000, 0, Some(1_000))),
            (ConservationVerdict::Match, "exact")
        );
    }

    #[test]
    fn test_classify_shortfall_within_counted_losses_is_match() {
        assert_eq!(
            classify(inputs(1_000, 40, Some(960))),
            (ConservationVerdict::Match, "within_counted_losses")
        );
        // Boundary: shortfall == not landed.
        assert_eq!(
            classify(inputs(1_000, 40, Some(960))).0,
            ConservationVerdict::Match
        );
    }

    #[test]
    fn test_classify_unexplained_shortfall_is_mismatch() {
        assert_eq!(
            classify(inputs(1_000, 39, Some(960))),
            (ConservationVerdict::Mismatch, "unexplained_shortfall")
        );
    }

    #[test]
    fn test_classify_extra_db_rows_is_mismatch() {
        assert_eq!(
            classify(inputs(1_000, 500, Some(1_001))),
            (ConservationVerdict::Mismatch, "extra_db_rows")
        );
    }

    #[test]
    fn test_classify_unreadable_count_is_unverified_never_mismatch() {
        for accepted in [0, 1, u64::MAX] {
            let (v, r) = classify(inputs(accepted, 0, None));
            assert_eq!(v, ConservationVerdict::Unverified);
            assert_eq!(r, "count_unreadable");
            let partial = ConservationInputs {
                full_coverage: false,
                ..inputs(accepted, 0, None)
            };
            assert_eq!(classify(partial).0, ConservationVerdict::Unverified);
        }
    }

    #[test]
    fn test_classify_zero_baseline_is_unverified_not_match() {
        assert_eq!(
            classify(inputs(0, 0, Some(0))),
            (ConservationVerdict::Unverified, "no_rows")
        );
    }

    #[test]
    fn test_classify_mid_day_process_is_partial_never_mismatch() {
        for db in [0, 10, 1_000_000] {
            let i = ConservationInputs {
                full_coverage: false,
                ..inputs(10, 0, Some(db))
            };
            assert_eq!(
                classify(i),
                (ConservationVerdict::Partial, "process_started_mid_day")
            );
        }
    }

    #[test]
    fn test_classify_extremes_do_not_overflow() {
        assert_eq!(
            classify(inputs(u64::MAX, u64::MAX, Some(0))).0,
            ConservationVerdict::Match
        );
        assert_eq!(
            classify(inputs(u64::MAX, 0, Some(0))).0,
            ConservationVerdict::Mismatch
        );
        assert_eq!(
            classify(inputs(0, 0, Some(u64::MAX))).0,
            ConservationVerdict::Mismatch
        );
    }

    #[test]
    fn test_verdict_labels_are_stable() {
        assert_eq!(ConservationVerdict::Match.as_str(), "match");
        assert_eq!(ConservationVerdict::Mismatch.as_str(), "mismatch");
        assert_eq!(ConservationVerdict::Unverified.as_str(), "unverified");
        assert_eq!(ConservationVerdict::Partial.as_str(), "partial");
    }

    #[test]
    fn test_process_covers_day_only_when_started_before_0900_same_day() {
        assert!(process_covers_day(at(8 * 3600 + 30 * 60), at(56_700)));
        assert!(!process_covers_day(at(9 * 3600), at(56_700)));
        assert!(!process_covers_day(at(11 * 3600), at(56_700)));
        // Started the day before: does not cover today.
        assert!(!process_covers_day(
            at(8 * 3600) - NANOS_PER_DAY,
            at(56_700)
        ));
    }

    #[test]
    fn test_secs_until_check() {
        assert_eq!(TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST, 56_700);
        assert_eq!(secs_until_check(0), Some(56_700));
        assert_eq!(secs_until_check(56_699), Some(1));
        assert_eq!(secs_until_check(56_700), None);
        assert_eq!(secs_until_check(86_399), None);
    }

    #[test]
    fn test_tick_count_sql_filters_day_and_feed() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 26).expect("date");
        assert_eq!(
            tick_count_sql(day),
            "select count() from ticks where ts in '2026-10-26' and feed = 'dhan'"
        );
    }

    fn utc(ist_nanos: i64) -> i64 {
        ist_nanos - IST_UTC_OFFSET_NANOS
    }

    #[test]
    fn test_process_down_gap_row_inside_the_session() {
        let prev = utc(at(10 * 3600));
        let now = utc(at(10 * 3600 + 120));
        let row = process_down_gap_row(Some(prev), now, true).expect("row");
        assert_eq!(row.gap_start_ist_nanos, at(10 * 3600));
        assert_eq!(row.gap_end_ist_nanos, at(10 * 3600 + 120));
        assert_eq!(row.down_secs(), 120);
        assert_eq!(row.reason, "process_down");
        assert_eq!(row.connection_index, -1);
        assert_eq!(row.ws_type, WsType::MainFeed);
        assert_eq!(row.feed, Feed::Dhan);
        assert!(!row.open_at_shutdown);
    }

    #[test]
    fn test_process_down_gap_row_starts_at_the_open_for_an_older_frame() {
        let prev = utc(at(15 * 3600 + 40 * 60) - NANOS_PER_DAY); // yesterday's close
        let now = utc(at(9 * 3600 + 20 * 60));
        let row = process_down_gap_row(Some(prev), now, true).expect("row");
        assert_eq!(row.gap_start_ist_nanos, at(9 * 3600 + 15 * 60));
        assert_eq!(row.down_secs(), 300);
    }

    #[test]
    fn test_process_down_gap_row_none_outside_the_window_or_without_a_frame() {
        let prev = Some(utc(at(10 * 3600)));
        // Before 09:15, at 15:40 (exclusive close), and after.
        assert!(process_down_gap_row(prev, utc(at(9 * 3600 + 14 * 60)), true).is_none());
        assert!(process_down_gap_row(prev, utc(at(56_400)), true).is_none());
        assert!(process_down_gap_row(prev, utc(at(20 * 3600)), true).is_none());
        // No previous frame.
        assert!(process_down_gap_row(None, utc(at(11 * 3600)), true).is_none());
        // Holiday.
        assert!(process_down_gap_row(prev, utc(at(11 * 3600)), false).is_none());
    }

    #[test]
    fn test_process_down_gap_row_none_for_a_short_or_negative_gap() {
        let now = utc(at(11 * 3600));
        assert!(process_down_gap_row(Some(now - 5 * NANOS_PER_SEC), now, true).is_none());
        assert!(process_down_gap_row(Some(now + NANOS_PER_SEC), now, true).is_none());
        assert!(process_down_gap_row(Some(now - 6 * NANOS_PER_SEC), now, true).is_some());
    }

    #[test]
    fn test_main_wires_spawn_tick_conservation() {
        let main = include_str!("main.rs");
        let prod = main.split("\n#[cfg(test)]").next().unwrap_or(main);
        assert!(prod.contains("tick_conservation::spawn_tick_conservation("));
        assert!(prod.contains("wal_last_receipt::newest_receipt_nanos_on_disk(&ws_wal_path)"));
        // The receipt is read after the WAL directory is claimed and before
        // this process's own writer can append.
        let lock = prod.find("lock_wal_dir(&ws_wal_path)").expect("lock");
        let probe = prod
            .find("newest_receipt_nanos_on_disk(&ws_wal_path)")
            .expect("probe");
        let writer = prod.find("WsFrameSpill::new_with_guard(").expect("writer");
        assert!(lock < probe && probe < writer);
    }
}
