//! Daily tick conservation check and the boot crash-gap row (plan item K4,
//! 2026-10-26).
//!
//! ## 1. Daily check (once per trading day, after the persist window)
//!
//! At [`TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST`] (15:45 IST, five minutes
//! after `TICK_PERSIST_END_SECS_OF_DAY_IST` closes the `ticks` window) it
//! compares the following quantities. Once the archiver verifies an hourly
//! or daily partition of either destination, its receipt covers every feed.
//! The persisted database value is then a hot-table observation. A hot excess
//! is still a mismatch. An equal or smaller count is unverified.
//! Two WAL snapshots bracket the hot counts. If either table is unavailable,
//! suspended, behind or advances across the reads, equal/smaller hot counts
//! remain unverified. If admissions increase during the reads, even a hot
//! excess stays unverified. Each retry samples a new numerator and new counts.
//!
//! - **accepted**: `ticks` rows the Dhan writer appended for today, from the
//!   in-process ledger (`tickvault_storage::tick_conservation_ledger`, one
//!   relaxed add per accepted row);
//! - **database**: `select count() from ticks where ts in '<today>' and
//!   feed = 'dhan'`, plus the same count over `tick_quotes` (plan item M5:
//!   repeat quotes moved there and the ledger counts them too), through the shared QuestDB client builder with a
//!   timeout;
//! - **not landed**: rows already counted as spilled, failed or handed to the
//!   WAL replay (the same sites that increment `tv_ticks_dropped_total`).
//!
//! and writes one `tick_conservation_audit` row. Verdicts
//! ([`classify`]):
//!
//! | verdict | when |
//! |---|---|
//! | `unverified` | a count or the archive state is unreadable, a partition was verified for archive, both counts are zero, or counted losses can explain a shortfall |
//! | `partial` | this process started after 09:00 IST today (or on another day): it only knows its own rows, so no mismatch is claimed |
//! | `match` | the full-day accepted and database counts are equal and positive |
//! | `mismatch` | the database holds MORE rows than accepted, or the shortfall exceeds every counted loss |
//!
//! An `unverified` (except `no_rows` or a verified archive) or `mismatch` read is retried, up to
//! [`TICK_CONSERVATION_MAX_READS`] reads in total, [`TICK_CONSERVATION_READ_SPACING_SECS`]
//! apart, because an acknowledged row is only countable once QuestDB has
//! applied its WAL. A final `mismatch` logs one coded `error!`
//! (`TICK-CONSERVE-01`), log only: no CloudWatch filter, no EMF name, no
//! alarm, no Telegram (Dhan noise lock).
//!
//! ## 2. Boot crash-gap row
//!
//! At boot, inside the session (09:15 to 15:40 IST on a trading day, or the
//! published Muhurat window on a Muhurat date, see [`session_window_nanos`]), the
//! downtime since the previous process's last WAL receipt is written as one
//! `feed_gap_audit` row: `reason = process_down`, `ws_type = main_feed`,
//! `connection_index = -1` (the whole process, never a socket slot). The
//! table's DEDUP key is unchanged. Outside the window, with no previous
//! frame, or with a gap at or under five seconds, nothing is written. A
//! previous frame from before the day's session open starts the gap at the
//! open.
//!
//! ## Where it runs
//!
//! Its own tokio task, never the frame drain. Market-hours aware: the daily
//! check sleeps until 15:45 IST on a session day (a trading day or a Muhurat
//! date, `TradingCalendar::is_session_day`) and skips the day when the
//! process starts at or after it.
//!
//! ## Honest limits
//!
//! - The not-landed total is per process and can include `feed_aux_packets`
//!   rows that shared a rescued batch. It cannot establish which of this
//!   day's missing rows were rescued. Even an exact count compares totals,
//!   not row identities.
//! - Rows the boot WAL replay of a mid-day restart re-appends would be counted
//!   twice against a database that already holds them; that process is
//!   `partial` by construction, which is why `partial` never claims a
//!   mismatch.
//! - The session close is 15:40 IST since the 2026-08-03 NSE change, so the
//!   crash-gap window is 09:15 to 15:40, not 15:30.
//! - A Muhurat date's check also runs at 15:45 IST. That fits the 2026
//!   session (14:05 to 15:40); a session that ended after 15:40 would be
//!   checked before its end (the `ticks` persist window closes at 15:40
//!   anyway). A Muhurat date with no published times gets the daily row but
//!   no downtime row, and its coverage reads against 09:00 like any day.

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
use tickvault_storage::tick_conservation_audit_persistence::{
    AuditReceiptWrite, TickConservationRow, write_or_queue_tick_conservation_row,
};
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
    /// The positive accepted and database counts are equal.
    Match,
    /// Rows are unaccounted for, or the database holds extra rows.
    Mismatch,
    /// An unreadable count, empty baseline or possibly accounted-for shortfall.
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
        (ConservationVerdict::Unverified, "within_counted_losses")
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

/// The tables the daily count reads. The writer counts a row of either in the
/// ledger (plan item M5, 2026-10-27: repeat quotes moved to `tick_quotes`),
/// so the database side sums both.
pub const TICK_COUNT_TABLES: [&str; 2] = [
    tickvault_storage::tick_persistence::TICKS_TABLE,
    tickvault_storage::tick_quotes_persistence::TICK_QUOTES_TABLE,
];

/// The count query for `table` on `day` (IST date). Pure.
#[must_use]
pub fn tick_count_sql(table: &str, day: NaiveDate) -> String {
    format!(
        "select count() from {table} where ts in '{}' and feed = '{}'",
        day.format("%Y-%m-%d"),
        Feed::Dhan.as_str()
    )
}

/// The session NSE holds on `day`, as IST nanos of day `(open, close)`,
/// close exclusive, or `None` when there is none. A regular trading day is
/// 09:15 to 15:40. A calendar-listed Muhurat session (operator Quote 49,
/// 2026-10-30: a Muhurat session is a session day; it can fall on a weekend
/// or a weekday holiday) is its own published window from
/// `config/base.toml`. A Muhurat date whose times are not published yet, an
/// ordinary weekend and a holiday are `None`. Pure, O(1).
#[must_use]
pub fn session_window_nanos(calendar: &TradingCalendar, day: NaiveDate) -> Option<(i64, i64)> {
    if calendar.is_trading_day(day) {
        return Some((MARKET_OPEN_IST_NANOS, MARKET_CLOSE_IST_NANOS));
    }
    calendar.muhurat_session_secs(day).map(|(open, close)| {
        (
            i64::from(open) * NANOS_PER_SEC,
            i64::from(close) * NANOS_PER_SEC,
        )
    })
}

/// The `feed_gap_audit` row for the downtime before this boot, or `None`
/// (see the module header). Times are UTC epoch nanos; `session` is the
/// day's session from [`session_window_nanos`] (`None` on a day with no
/// session). Pure.
#[must_use]
pub fn process_down_gap_row(
    previous_receipt_utc_nanos: Option<i64>,
    now_utc_nanos: i64,
    session: Option<(i64, i64)>,
) -> Option<FeedGapRow> {
    let (open, close) = session?;
    let previous = previous_receipt_utc_nanos?;
    let now_ist = now_utc_nanos.saturating_add(IST_UTC_OFFSET_NANOS);
    let day_start = now_ist - now_ist.rem_euclid(NANOS_PER_DAY);
    let now_of_day = now_ist - day_start;
    if !(open..close).contains(&now_of_day) {
        return None;
    }
    let start = previous
        .saturating_add(IST_UTC_OFFSET_NANOS)
        .max(day_start + open);
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

/// The wall clock, UTC epoch nanos. Named `wall_now_...` on purpose (B3
/// review, 2026-11-13): the clocked task's functions take their clock as a
/// parameter named `now_utc_nanos`. If this function had that name too, a
/// refactor that dropped the parameter would still compile, the call would
/// bind here, and the task would read the wall clock again with every test
/// still green. With distinct names, a dropped parameter fails to compile.
fn wall_now_utc_nanos() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
}

fn ist_date_of(ist_nanos: i64) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp(ist_nanos.div_euclid(NANOS_PER_SEC), 0)
        .map(|dt| dt.date_naive())
}

fn secs_of_day(ist_nanos: i64) -> u32 {
    u32::try_from(ist_nanos.rem_euclid(NANOS_PER_DAY) / NANOS_PER_SEC).unwrap_or(0)
}

/// Spawns the boot crash-gap write and the daily check on their own task,
/// reading the wall clock. Returns the task handle.
// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested pure parts (classify, process_covers_day, secs_until_check, process_down_gap_row, tick_count_sql); spawn site pinned by test_main_wires_spawn_tick_conservation; the task body is driven end to end through spawn_tick_conservation_with_clock in tests/cov_xvs_tick_conservation_spawn.rs.
pub fn spawn_tick_conservation(
    questdb: QuestDbConfig,
    calendar: Arc<TradingCalendar>,
    process_start_ist_nanos: i64,
    previous_receipt_utc_nanos: Option<i64>,
) -> tokio::task::JoinHandle<()> {
    spawn_tick_conservation_with_clock(
        questdb,
        calendar,
        process_start_ist_nanos,
        previous_receipt_utc_nanos,
        wall_now_utc_nanos,
    )
}

/// [`spawn_tick_conservation`] with the clock passed in: `now_utc_nanos`
/// returns the current UTC epoch nanos and is read at every point the task
/// reads the time (the downtime decision, each day's check decision, the
/// sleep to the next IST midnight, and the row's `checked_at`). Production
/// passes the wall clock; a test passes a clock that starts at a chosen
/// instant and moves with tokio's paused clock, so every branch (in
/// session, after the check time, a weekend, across IST midnight) runs on
/// every test run instead of only when CI happens to start at that hour.
/// That the midnight sleep reads this clock (not the wall) is pinned by a
/// source scan, `test_run_daily_checks_loop_reads_only_the_injected_clock`,
/// because the spawn harness alone catches a wall read there only at some
/// hours.
/// The sleeps are tokio sleeps either way. O(1) per call.
// TEST-EXEMPT: tokio::spawn wrapper; driven end to end with a fixed clock by tests/cov_xvs_tick_conservation_spawn.rs (in session, after the check, weekend, IST midnight).
pub fn spawn_tick_conservation_with_clock<C>(
    questdb: QuestDbConfig,
    calendar: Arc<TradingCalendar>,
    process_start_ist_nanos: i64,
    previous_receipt_utc_nanos: Option<i64>,
    now_utc_nanos: C,
) -> tokio::task::JoinHandle<()>
where
    C: Fn() -> i64 + Send + Sync + 'static,
{
    tokio::spawn(async move {
        write_process_down_row(
            &questdb,
            &calendar,
            previous_receipt_utc_nanos,
            &now_utc_nanos,
        )
        .await;
        run_daily_checks(&questdb, &calendar, process_start_ist_nanos, &now_utc_nanos).await;
    })
}

async fn write_process_down_row(
    questdb: &QuestDbConfig,
    calendar: &TradingCalendar,
    previous_receipt_utc_nanos: Option<i64>,
    now_utc_nanos: &impl Fn() -> i64,
) {
    use tickvault_storage::feed_gap_audit_persistence::{
        FeedGapAuditWriter, ensure_feed_gap_audit_table,
    };
    let now = now_utc_nanos();
    let today = ist_date_of(now.saturating_add(IST_UTC_OFFSET_NANOS));
    let session = today.and_then(|d| session_window_nanos(calendar, d));
    let Some(row) = process_down_gap_row(previous_receipt_utc_nanos, now, session) else {
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
    now_utc_nanos: &impl Fn() -> i64,
) {
    use tickvault_storage::audit_spill::{AuditSpillTable, spawn_audit_spill_drain_once};
    use tickvault_storage::tick_conservation_audit_persistence::ensure_tick_conservation_audit_table;
    spawn_audit_spill_drain_once(AuditSpillTable::TickConservationAudit, questdb);
    ensure_tick_conservation_audit_table(questdb).await;
    loop {
        let now_ist = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);
        let today = ist_date_of(now_ist);
        // A session day, Muhurat included (operator Quote 49): before B3
        // this read `is_trading_day`, so a Muhurat session on a weekend or a
        // holiday, whose ticks the ledger counts, got no row at all.
        let session_day = today.is_some_and(|d| calendar.is_session_day(d));
        if let (true, Some(day), Some(wait)) =
            (session_day, today, secs_until_check(secs_of_day(now_ist)))
        {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            run_one_check(
                questdb,
                day,
                now_ist,
                process_start_ist_nanos,
                now_utc_nanos,
            )
            .await;
        }
        // Retry can run to midnight. Begin the new day's scheduling immediately.
        let now_ist = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);
        if ist_date_of(now_ist) != today {
            continue;
        }
        let to_midnight = NANOS_PER_DAY - now_ist.rem_euclid(NANOS_PER_DAY);
        let secs = u64::try_from(to_midnight / NANOS_PER_SEC).unwrap_or(0) + 5;
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

/// Archived partition metadata combines all feeds. Retain the observed hot
/// count, but make no full-day comparison when the Dhan share is unknown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArchiveCoverage {
    HotOnly,
    VerifiedPartition,
    Unknown,
}

const ARCHIVE_AUDIT_APPLIED_SQL: &str = "select count() from wal_tables() \
    where name = 'partition_archive_audit' and suspended = false and writerTxn = sequencerTxn";

const DESTINATION_WAL_SQL: &str = "select name, suspended, writerTxn, sequencerTxn \
    from wal_tables() where name in ('ticks','tick_quotes')";

/// Each position is meaningful only for its named destination. Missing or
/// duplicate rows, a suspended writer (even with equal transactions), lag or
/// an invalid response cannot establish that an acknowledged row is visible.
fn applied_positions(body: &str) -> Option<[i64; 2]> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    if json.get("error").is_some() || json.get("count").is_some_and(|n| n.as_u64() != Some(2)) {
        return None;
    }
    let (rows, skipped) =
        tickvault_storage::wal_suspension_watcher::parse_wal_tables_response(&json).ok()?;
    if skipped != 0 || rows.len() != TICK_COUNT_TABLES.len() {
        return None;
    }
    let mut positions = [None; 2];
    for row in rows {
        let slot = TICK_COUNT_TABLES
            .iter()
            .position(|&name| name == row.name)?;
        let applied = row.writer_txn?;
        if row.suspended
            || applied < 0
            || Some(applied) != row.sequencer_txn
            || positions[slot].replace(applied).is_some()
        {
            return None;
        }
    }
    Some([positions[0]?, positions[1]?])
}

async fn destination_positions(questdb: &QuestDbConfig) -> Option<[i64; 2]> {
    applied_positions(&read_query(DESTINATION_WAL_SQL, questdb).await?)
}

fn archive_marker_sql(day: NaiveDate) -> String {
    // The partition name identifies its trading date. The audit timestamp is
    // when it was copied, potentially on a later day. A verified receipt
    // precedes DROP even if the subsequent dropped receipt fails to land.
    format!(
        "select count() from (select partition_name from partition_archive_audit \
        where ts >= '{day}T00:00:00.000000Z' and table_name in ('ticks','tick_quotes') \
        and partition_name like '{day}%' and outcome in ('verified','dropped') limit 1)"
    )
}

/// Called AFTER the hot counts. For a partition removed before those reads,
/// the archiver had already flushed its verified receipt before DROP. First
/// establish that such receipts are query-visible, then look them up. A
/// subsequent unrelated drop cannot invalidate a previously read hot count.
async fn archive_coverage(day: NaiveDate, questdb: &QuestDbConfig) -> ArchiveCoverage {
    if read_count(ARCHIVE_AUDIT_APPLIED_SQL, questdb).await != Some(1) {
        return ArchiveCoverage::Unknown;
    }
    match read_count(&archive_marker_sql(day), questdb).await {
        Some(0) => ArchiveCoverage::HotOnly,
        Some(1) => ArchiveCoverage::VerifiedPartition,
        _ => ArchiveCoverage::Unknown,
    }
}

// More cold Dhan rows can only increase the database side. An already
// observed hot excess remains an independently demonstrated mismatch.
fn needs_storage_evidence(i: ConservationInputs) -> bool {
    i.full_coverage && i.db_rows.is_some_and(|hot| hot <= i.accepted_rows)
}

fn classify_coverage(
    i: ConservationInputs,
    archive: ArchiveCoverage,
) -> (ConservationVerdict, &'static str) {
    // Preserve failed count reads and partial-process coverage. Their raw
    // fields already explain why a whole-day comparison was impossible.
    if needs_storage_evidence(i) {
        match archive {
            ArchiveCoverage::VerifiedPartition => {
                return (ConservationVerdict::Unverified, "archived_partition");
            }
            ArchiveCoverage::Unknown => {
                return (ConservationVerdict::Unverified, "archive_coverage_unknown");
            }
            ArchiveCoverage::HotOnly => {}
        }
    }
    classify(i)
}

/// The day's rows across [`TICK_COUNT_TABLES`]. `None` when any one count
/// cannot be read: a sum missing a table would read as a shortfall.
async fn read_tick_count(day: NaiveDate, questdb: &QuestDbConfig) -> Option<u64> {
    let mut total = 0_u64;
    for table in TICK_COUNT_TABLES {
        total = total.checked_add(read_count(&tick_count_sql(table, day), questdb).await?)?;
    }
    Some(total)
}

async fn read_count(sql: &str, questdb: &QuestDbConfig) -> Option<u64> {
    let body = read_query(sql, questdb).await?;
    crate::feed_scoreboard_boot::parse_questdb_count(&body).and_then(|n| u64::try_from(n).ok())
}

async fn read_query(sql: &str, questdb: &QuestDbConfig) -> Option<String> {
    let client =
        tickvault_storage::http_client::build_probe_client(TICK_COUNT_QUERY_TIMEOUT_SECS).ok()?;
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let resp = tokio::time::timeout(
        Duration::from_secs(TICK_COUNT_QUERY_TIMEOUT_SECS),
        client.get(&url).query(&[("query", sql)]).send(),
    )
    .await
    .ok()?
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

async fn run_one_check(
    questdb: &QuestDbConfig,
    day: NaiveDate,
    day_ist_nanos: i64,
    process_start_ist_nanos: i64,
    now_utc_nanos: &impl Fn() -> i64,
) {
    let ledger = tickvault_storage::tick_conservation_ledger::ledger();
    let ledger_day = TickConservationLedger::ist_day_of(day_ist_nanos);
    let full_coverage = process_covers_day(process_start_ist_nanos, day_ist_nanos);
    let mut reads = 0_u32;
    let (inputs, verdict, reason) = loop {
        reads += 1;
        let accepted_rows = ledger.accepted_rows_on(ledger_day);
        let not_landed_rows = ledger.rows_not_landed();
        let before = if full_coverage {
            destination_positions(questdb).await
        } else {
            None
        };
        let inputs = ConservationInputs {
            accepted_rows,
            not_landed_rows,
            db_rows: read_tick_count(day, questdb).await,
            full_coverage,
        };
        // Keep the original numerator as evidence. If a later admission
        // could already be in either count, even a hot excess is ambiguous.
        let moving_admissions = full_coverage
            && inputs.db_rows.is_some()
            && ledger.accepted_rows_on(ledger_day) != accepted_rows;
        let (verdict, reason) = if moving_admissions {
            (ConservationVerdict::Unverified, "admissions_changed")
        } else if needs_storage_evidence(inputs) {
            // The two counts are separate reads. A later healthy WAL cannot
            // certify an earlier stale read. Require both positions to be
            // applied before, and identical after, these reads.
            if before.is_some() && destination_positions(questdb).await == before {
                classify_coverage(inputs, archive_coverage(day, questdb).await)
            } else {
                (
                    ConservationVerdict::Unverified,
                    "destination_wal_unverified",
                )
            }
        } else {
            classify(inputs)
        };
        let settled = reason == "archived_partition"
            || matches!(
                verdict,
                ConservationVerdict::Match | ConservationVerdict::Partial
            )
            || reason == "no_rows";
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
    let retry_window = receipt_retry_window(day_ist_nanos, checked_at);
    persist(&row, retry_window, |receipt| {
        write_or_queue_tick_conservation_row(questdb, receipt)
    })
    .await;
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
            "TICK-CONSERVE-01: accepted buffer writes and stored trade and quote rows differ. \
             Compare capture keys to distinguish repeated writes from missing rows"
        ),
        "unverified" => warn!(
            reason = row.reason,
            accepted_rows = inputs.accepted_rows,
            db_tick_rows = row.db_tick_rows,
            not_landed_rows = inputs.not_landed_rows,
            count_reads = row.count_reads,
            "tick conservation: today's accepted and stored counts could not be confirmed"
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

// A delayed check still gets an immediate write, even if its audited day has ended.
fn receipt_retry_window(day: i64, checked_at: i64) -> Duration {
    if day.div_euclid(NANOS_PER_DAY) == checked_at.div_euclid(NANOS_PER_DAY) {
        Duration::from_nanos((NANOS_PER_DAY - checked_at.rem_euclid(NANOS_PER_DAY)).unsigned_abs())
    } else {
        Duration::ZERO
    }
}

/// Keep the original receipt for another attempt when both durable paths refuse it.
/// Waiting happens on the daily audit task, outside the feed and dashboard paths.
async fn persist<'a, F>(
    row: &'a TickConservationRow,
    retry_window: Duration,
    mut write: impl FnMut(&'a TickConservationRow) -> F,
) where
    F: std::future::Future<Output = anyhow::Result<AuditReceiptWrite>>,
{
    let deadline = tokio::time::Instant::now() + retry_window;
    let result = match write(row).await {
        Ok(result) => result,
        Err(err) => {
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "tick_conservation_audit_write",
                ?row,
                ?err,
                "daily receipt has no confirmed durable copy; keeping it in memory for retry until IST midnight"
            );
            let interval = Duration::from_secs(
                tickvault_storage::audit_spill::AUDIT_SPILL_DRAIN_INTERVAL_SECS,
            );
            let result = loop {
                let next_attempt = tokio::time::Instant::now() + interval;
                tokio::time::sleep_until(next_attempt.min(deadline)).await;
                if tokio::time::Instant::now() >= deadline {
                    error!(
                        code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                        stage = "tick_conservation_audit_retry_expired",
                        ?row,
                        "daily receipt retry window ended with no confirmed database or journal copy"
                    );
                    return;
                }
                // A write already in flight can have published its journal. Let it
                // report the outcome even if the audited day ends during the request.
                match write(row).await {
                    Ok(result) => break result,
                    Err(err) => {
                        tracing::debug!(?err, "daily receipt retry still has no confirmed copy")
                    }
                }
            };
            info!(
                ?result,
                ?row,
                "daily receipt recovered after a failed write"
            );
            result
        }
    };
    if result == AuditReceiptWrite::QueuedForReplay {
        warn!(
            code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
            stage = "tick_conservation_audit_queued",
            ?row,
            "tick conservation receipt queued on disk; database delivery unconfirmed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn test_persist_reoffers_original_receipt_only_while_both_durable_paths_fail() {
        let row = TickConservationRow {
            checked_at_ist_nanos: DAY + 100,
            day_ist_nanos: DAY,
            feed: Feed::Dhan,
            verdict: "mismatch",
            reason: "unexplained_shortfall",
            accepted_rows: 4,
            db_tick_rows: 2,
            not_landed_rows: 0,
            count_reads: 3,
            process_start_ist_nanos: DAY,
        };
        let interval = tickvault_storage::audit_spill::AUDIT_SPILL_DRAIN_INTERVAL_SECS;
        for target in [
            AuditReceiptWrite::Acknowledged,
            AuditReceiptWrite::QueuedForReplay,
        ] {
            for failures in [0, 2] {
                let mut calls = 0;
                let started = tokio::time::Instant::now();
                persist(&row, Duration::from_secs(3 * interval), |receipt| {
                    assert!(std::ptr::eq(receipt, &row));
                    calls += 1;
                    std::future::ready(if calls <= failures {
                        Err(anyhow::anyhow!("database and journal unavailable"))
                    } else {
                        Ok(target)
                    })
                })
                .await;
                assert_eq!(calls, failures + 1);
                assert_eq!(started.elapsed(), Duration::from_secs(failures * interval));
            }
        }
        assert_eq!(
            receipt_retry_window(DAY, DAY + NANOS_PER_DAY + 1),
            Duration::ZERO
        );
        assert_eq!(
            receipt_retry_window(DAY, DAY + NANOS_PER_DAY - 23),
            Duration::from_nanos(23)
        );
        let calls = std::cell::Cell::new(0);
        let finished = std::cell::Cell::new(false);
        persist(&row, Duration::from_secs(interval + 1), |_| async {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                return Err(anyhow::anyhow!("both refused"));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            finished.set(true);
            Ok(AuditReceiptWrite::QueuedForReplay)
        })
        .await;
        assert!(
            finished.get(),
            "an attempt started before midnight finished"
        );
        assert_eq!(calls.get(), 2);
        let mut late_calls = 0;
        persist(&row, Duration::ZERO, |_| {
            late_calls += 1;
            std::future::ready(Err(anyhow::anyhow!("prior-day check")))
        })
        .await;
        assert_eq!(late_calls, 1);
        let mut calls = 0;
        let window = Duration::from_secs(2 * interval + 1);
        let started = tokio::time::Instant::now();
        persist(&row, window, |receipt| {
            assert!(std::ptr::eq(receipt, &row));
            calls += 1;
            std::future::ready(Err(anyhow::anyhow!("persistent dual failure")))
        })
        .await;
        assert_eq!(calls, 3);
        assert_eq!(started.elapsed(), window);
    }

    /// 2026-10-26 00:00 IST, in IST-epoch nanos.
    const DAY: i64 = 1_792_972_800_000_000_000;

    fn at(secs_of_day: i64) -> i64 {
        DAY + secs_of_day * NANOS_PER_SEC
    }

    /// The regular session, 09:15 to 15:40 IST.
    const REGULAR: Option<(i64, i64)> = Some((MARKET_OPEN_IST_NANOS, MARKET_CLOSE_IST_NANOS));

    /// A calendar with one Muhurat Sunday (2026-11-08, 14:05 to 15:40, the
    /// session `config/base.toml` carries), one Muhurat date with no
    /// published times (2026-11-15) and one weekday holiday (2026-11-09).
    fn muhurat_calendar() -> TradingCalendar {
        use tickvault_common::config::{MuhuratTradingEntry, NseHolidayEntry, TradingConfig};
        let cfg = TradingConfig {
            market_open_time: "09:15:00".to_owned(),
            market_close_time: "15:30:00".to_owned(),
            order_cutoff_time: "15:29:00".to_owned(),
            data_collection_start: "09:00:00".to_owned(),
            data_collection_end: "16:00:00".to_owned(),
            timezone: "Asia/Kolkata".to_owned(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-11-09".to_owned(),
                name: "a weekday holiday".to_owned(),
            }],
            muhurat_trading_dates: vec![
                MuhuratTradingEntry {
                    date: "2026-11-08".to_owned(),
                    name: "Muhurat".to_owned(),
                    session_start_ist: "14:05".to_owned(),
                    session_end_ist: "15:40".to_owned(),
                },
                MuhuratTradingEntry {
                    date: "2026-11-15".to_owned(),
                    name: "Muhurat, times not published".to_owned(),
                    session_start_ist: String::new(),
                    session_end_ist: String::new(),
                },
            ],
            nse_mock_trading_dates: Vec::new(),
        };
        TradingCalendar::from_config(&cfg).expect("calendar builds")
    }

    /// Regression: 2026-11-13 (B3 review). The downtime row was gated on
    /// `is_trading_day`, which is false for a Muhurat session on a weekend,
    /// and used the fixed 09:15 to 15:40 window. A Muhurat date now reads
    /// its own published window; a holiday and an unpublished Muhurat read
    /// no session.
    #[test]
    fn test_session_window_nanos_reads_the_muhurat_window() {
        let cal = muhurat_calendar();
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).expect("date");
        assert_eq!(session_window_nanos(&cal, d(2026, 11, 10)), REGULAR);
        assert_eq!(
            session_window_nanos(&cal, d(2026, 11, 8)),
            Some((
                (14 * 3600 + 5 * 60) * NANOS_PER_SEC,
                (15 * 3600 + 40 * 60) * NANOS_PER_SEC
            )),
            "the Muhurat Sunday reads its published window"
        );
        assert_eq!(session_window_nanos(&cal, d(2026, 11, 9)), None, "holiday");
        assert_eq!(
            session_window_nanos(&cal, d(2026, 11, 14)),
            None,
            "Saturday"
        );
        assert_eq!(
            session_window_nanos(&cal, d(2026, 11, 15)),
            None,
            "a Muhurat date with no published times has no window"
        );
    }

    /// The Muhurat window decides the downtime row: a restart at 15:00 after
    /// a frame at 13:00 starts the gap at the 14:05 open (3,300 s), never at
    /// 09:15, and a restart before 14:05 writes nothing.
    #[test]
    fn test_process_down_gap_row_uses_the_muhurat_window() {
        let muhurat = Some((
            (14 * 3600 + 5 * 60) * NANOS_PER_SEC,
            (15 * 3600 + 40 * 60) * NANOS_PER_SEC,
        ));
        let prev = Some(utc(at(13 * 3600)));
        let row = process_down_gap_row(prev, utc(at(15 * 3600)), muhurat).expect("row");
        assert_eq!(row.gap_start_ist_nanos, at(14 * 3600 + 5 * 60));
        assert_eq!(row.down_secs(), 3_300);
        assert!(process_down_gap_row(prev, utc(at(11 * 3600)), muhurat).is_none());
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
    fn applied_positions_requires_two_distinct_healthy_destinations() {
        use serde_json::json;
        let response = |rows: serde_json::Value| {
            json!({
            "columns": [{"name":"name"}, {"name":"suspended"}, {"name":"writerTxn"}, {"name":"sequencerTxn"}],
            "dataset": rows
        }).to_string()
        };
        let tick = json!(["ticks", false, 3, 3]);
        let quote = json!(["tick_quotes", false, 5, 5]);
        assert_eq!(
            applied_positions(&response(json!([quote, tick]))),
            Some([3, 5])
        );
        for rows in [
            json!([]),
            json!([tick]),
            json!([tick, tick]),
            json!([quote, quote]),
            json!([tick, quote, quote]),
            json!([tick, ["other", false, 3, 3]]),
            json!([tick, ["tick_quotes", true, 5, 5]]),
            json!([tick, ["tick_quotes", false, 5, 6]]),
            json!([tick, ["tick_quotes", false, -1, -1]]),
            json!([quote, ["ticks", false, null, 3]]),
            json!([null, tick]),
        ] {
            assert_eq!(applied_positions(&response(rows)), None);
        }
        for raw in [
            "{}",
            "not json",
            r#"{"count":1,"dataset":[]}"#,
            r#"{"error":"suspended"}"#,
        ] {
            assert_eq!(applied_positions(raw), None);
        }
    }

    #[test]
    fn archive_marker_uses_partition_date_and_covers_verified_before_drop() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();
        let sql = archive_marker_sql(day);
        assert!(sql.contains("table_name in ('ticks','tick_quotes')"));
        assert!(sql.contains("partition_name like '2026-11-26%'"));
        assert!(sql.contains("outcome in ('verified','dropped') limit 1"));
        // The audit time can be later than the partition day. A cap at
        // midnight or ts in '<day>' would hide next-day archive receipts.
        assert!(sql.contains("ts >= '2026-11-26T00:00:00.000000Z'"));
        assert!(!sql.contains("ts <") && !sql.contains("ts in"));
    }

    #[test]
    fn archive_state_changes_the_verdict_without_changing_the_hot_count() {
        for hot in [0, 98, 100] {
            let sample = inputs(100, 0, Some(hot));
            for (evidence, reason) in [
                (ArchiveCoverage::VerifiedPartition, "archived_partition"),
                (ArchiveCoverage::Unknown, "archive_coverage_unknown"),
            ] {
                assert_eq!(
                    classify_coverage(sample, evidence),
                    (ConservationVerdict::Unverified, reason)
                );
                assert_eq!(sample.db_rows, Some(hot));
            }
            assert_eq!(
                classify_coverage(sample, ArchiveCoverage::HotOnly),
                classify(sample)
            );
        }
        let excess = inputs(100, 0, Some(101));
        assert!(!needs_storage_evidence(excess));
        for archive in [
            ArchiveCoverage::VerifiedPartition,
            ArchiveCoverage::Unknown,
            ArchiveCoverage::HotOnly,
        ] {
            assert_eq!(
                classify_coverage(excess, archive),
                (ConservationVerdict::Mismatch, "extra_db_rows")
            );
        }
        let partial = ConservationInputs {
            full_coverage: false,
            ..inputs(100, 0, Some(12))
        };
        for evidence in [
            ArchiveCoverage::VerifiedPartition,
            ArchiveCoverage::Unknown,
            ArchiveCoverage::HotOnly,
        ] {
            assert_eq!(classify_coverage(partial, evidence), classify(partial));
            assert_eq!(
                classify_coverage(inputs(100, 0, None), evidence),
                (ConservationVerdict::Unverified, "count_unreadable")
            );
        }
    }

    #[test]
    fn test_classify_shortfall_within_counted_losses_is_unverified() {
        for (accepted, not_landed, db) in [(1_000, 40, 960), (1_000, 40, 999), (100, 100, 0)] {
            assert_eq!(
                classify(inputs(accepted, not_landed, Some(db))),
                (ConservationVerdict::Unverified, "within_counted_losses")
            );
        }
        assert_eq!(
            classify(inputs(1_000, 40, Some(1_000))),
            (ConservationVerdict::Match, "exact")
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
            ConservationVerdict::Unverified
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
            tick_count_sql("ticks", day),
            "select count() from ticks where ts in '2026-10-26' and feed = 'dhan'"
        );
    }

    /// Regression (2026-10-27, plan item M5): the writer's ledger counts
    /// `tick_quotes` rows too, so the database side must count both tables or
    /// every quote row reads as a shortfall.
    #[test]
    fn test_regression_tick_count_reads_ticks_and_tick_quotes() {
        assert_eq!(TICK_COUNT_TABLES, ["ticks", "tick_quotes"]);
        let day = NaiveDate::from_ymd_opt(2026, 10, 27).expect("date");
        assert_eq!(
            tick_count_sql("tick_quotes", day),
            "select count() from tick_quotes where ts in '2026-10-27' and feed = 'dhan'"
        );
    }

    fn utc(ist_nanos: i64) -> i64 {
        ist_nanos - IST_UTC_OFFSET_NANOS
    }

    #[test]
    fn test_process_down_gap_row_inside_the_session() {
        let prev = utc(at(10 * 3600));
        let now = utc(at(10 * 3600 + 120));
        let row = process_down_gap_row(Some(prev), now, REGULAR).expect("row");
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
        let row = process_down_gap_row(Some(prev), now, REGULAR).expect("row");
        assert_eq!(row.gap_start_ist_nanos, at(9 * 3600 + 15 * 60));
        assert_eq!(row.down_secs(), 300);
    }

    #[test]
    fn test_process_down_gap_row_none_outside_the_window_or_without_a_frame() {
        let prev = Some(utc(at(10 * 3600)));
        // Before 09:15, at 15:40 (exclusive close), and after.
        assert!(process_down_gap_row(prev, utc(at(9 * 3600 + 14 * 60)), REGULAR).is_none());
        assert!(process_down_gap_row(prev, utc(at(56_400)), REGULAR).is_none());
        assert!(process_down_gap_row(prev, utc(at(20 * 3600)), REGULAR).is_none());
        // No previous frame.
        assert!(process_down_gap_row(None, utc(at(11 * 3600)), REGULAR).is_none());
        // Holiday.
        assert!(process_down_gap_row(prev, utc(at(11 * 3600)), None).is_none());
    }

    #[test]
    fn test_process_down_gap_row_none_for_a_short_or_negative_gap() {
        let now = utc(at(11 * 3600));
        assert!(process_down_gap_row(Some(now - 5 * NANOS_PER_SEC), now, REGULAR).is_none());
        assert!(process_down_gap_row(Some(now + NANOS_PER_SEC), now, REGULAR).is_none());
        assert!(process_down_gap_row(Some(now - 6 * NANOS_PER_SEC), now, REGULAR).is_some());
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

    /// B3 review (2026-11-13): production must keep reading the WALL clock.
    /// `spawn_tick_conservation_with_clock` exists so tests can pass a fixed
    /// one; the production spawn passes `wall_now_utc_nanos` (chrono's UTC now)
    /// and nothing else.
    #[test]
    fn test_spawn_tick_conservation_with_clock_gets_the_wall_clock_in_production() {
        let me = include_str!("tick_conservation.rs");
        let prod = me.split("\n#[cfg(test)]").next().unwrap_or(me);
        let outer = prod
            .find("pub fn spawn_tick_conservation(")
            .expect("production spawn");
        let inner = prod
            .find("pub fn spawn_tick_conservation_with_clock<")
            .expect("clock spawn");
        let body = &prod[outer..inner];
        assert!(body.contains("spawn_tick_conservation_with_clock("));
        assert!(body.contains("wall_now_utc_nanos,"));
        assert!(prod.contains("chrono::Utc::now().timestamp_nanos_opt()"));
        assert!(wall_now_utc_nanos() > 1_700_000_000_000_000_000);
        // The wall-clock fn must not share the clock parameter's name, or a
        // dropped parameter would still compile and read the wall clock.
        assert!(
            !prod.contains("fn now_utc_nanos("),
            "the wall-clock fn is named wall_now_utc_nanos, never now_utc_nanos"
        );
    }

    /// B3 review round 3 (2026-11-13): the midnight-rollover sleep in
    /// `run_daily_checks` is the one clock read the spawn harness checks
    /// only through which day's row appears, and that depends on the hour CI
    /// runs (a wall read there passes whenever the wall time is past about
    /// 08:15 IST). So the loop body is pinned line by line here, and no
    /// clocked function may read the wall clock directly.
    #[test]
    fn test_run_daily_checks_loop_reads_only_the_injected_clock() {
        let me = include_str!("tick_conservation.rs");
        let prod = me.split("\n#[cfg(test)]").next().unwrap_or(me);
        let body_of = |start: &str, end: &str| -> &str {
            let from = prod.find(start).expect("fn start");
            let to = from + prod[from..].find(end).expect("next fn");
            &prod[from..to]
        };
        let daily = body_of("async fn run_daily_checks(", "async fn read_tick_count(");
        let steps = [
            "loop {",
            "let now_ist = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);",
            "let today = ist_date_of(now_ist);",
            "tokio::time::sleep(Duration::from_secs(wait)).await;",
            "run_one_check(",
            "let now_ist = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);",
            "let to_midnight = NANOS_PER_DAY - now_ist.rem_euclid(NANOS_PER_DAY);",
            "tokio::time::sleep(Duration::from_secs(secs)).await;",
        ];
        let mut at = 0;
        for step in steps {
            let found = daily[at..].find(step);
            assert!(found.is_some(), "loop step missing or out of order: {step}");
            at += found.unwrap_or_default() + step.len();
        }
        assert_eq!(daily.matches("now_utc_nanos()").count(), 2);
        for (name, body) in [
            ("run_daily_checks", daily),
            (
                "write_process_down_row",
                body_of(
                    "async fn write_process_down_row(",
                    "async fn run_daily_checks(",
                ),
            ),
            (
                "run_one_check",
                body_of("async fn run_one_check(", "fn report("),
            ),
        ] {
            for wall in ["Utc::now", "wall_now_utc_nanos", "SystemTime", "Local::now"] {
                assert!(
                    !body.contains(wall),
                    "{name} must read only the injected clock, found `{wall}`"
                );
            }
        }
    }
}
