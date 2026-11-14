//! The daily export for Brutex (owner Quote 60, 2026-11-14).
//!
//! After the close, every partition of trading day D of the export tables
//! (`ticks`, `tick_quotes`, `feed_aux_packets`, `market_depth`,
//! `market_depth_book` and every emitted candle table) is written to the
//! archive's OWN canonical key `questdb-partitions/<table>/<partition>.csv.gz`
//! through the archiver's own export (gzip, SHA-256, quote-aware record
//! count) and a create-only upload (`If-None-Match: "*"`). Rows are ordered
//! by the table's DEDUP key ([`export_order_by`]). The archiver orders the
//! same way whenever an object already sits at the canonical key and the
//! clock is outside 08:30 to the session's ingest end ([`archiver_export_order`]), so
//! when it later moves the partition off the box it finds an identical
//! object and reuses it: no second copy. A partition whose content differs
//! from the canonical object goes to the archiver's existing
//! content-addressed copy; nothing is ever overwritten.
//!
//! Each partition is exported under the shared side of the archive quiesce
//! lock, after listing it again under that lock, so the hour-path archiver
//! (which drops under the exclusive side) cannot drop it mid-export. The
//! archiver's exact-gate path (`process_one`: `ticks` always, and
//! `tick_quotes`, `feed_aux_packets` and the candle tables on a pass not
//! under pressure) drops WITHOUT that lock (2026-11-14 third review), so a
//! failed or empty export is followed by a re-list: a partition gone from
//! QuestDB is recorded as dropped mid-export ([`PartitionStep::Vanished`]),
//! never a failed attempt, and must then be covered by a verified archive
//! copy. The verified archive audit rows of day D are read AFTER every
//! table, so a partition the archiver copied and dropped before or between
//! listings is still in the manifest; every archived copy of a partition is
//! listed, not only the last ([`ManifestTable::rows_may_overlap`]).
//!
//! `tick_quotes` and `feed_aux_packets` are stamped with ARRIVAL time, and
//! the main feed stays connected after the close, so their hours keep taking
//! rows until the process stops (2026-11-14 third review). For the session
//! day the export leaves out every hour of those two tables at or after the
//! first hour that is still open ([`arrival_open_from`]) and names that
//! instant in the manifest ([`ManifestTable::open_from_ist`]); the scheduler
//! never runs the session day before the close's own hour has ended
//! ([`arrival_close_secs`]). A left-out hour reaches S3 through the archive
//! and an amend record, as one object (no export object sits at its key).
//!
//! Day D is then described by, in this order, each create-only:
//! `_daily/<D>/instrument-counts-<n>.csv.gz`, `_daily/<D>/manifest-<n>.json`,
//! and LAST `_daily/<D>/DONE`, naming that manifest and its SHA-256. Every
//! listed key is checked again in S3 between the manifest and DONE. A re-run
//! with no DONE writes attempt `n + 1`, where `n` is the highest attempt any
//! counts file or manifest of the day carries; with DONE it does nothing.
//! Every verified archiver copy of an export-table partition, a reuse of the
//! canonical object included, gets a create-only amend record
//! `_daily/<D>/amend-*.json` before its drop, so a copy no manifest names
//! (an hour archived between two attempts whose audit row was not yet
//! readable) still reaches Brutex.
//!
//! Before each partition the export needs the after-close depth pass's disk
//! floor AND the disk-pressure line ([`pressure_free_floor_bytes`]: below
//! it the pressure archiver needs the exclusive side of the lock) plus the
//! partition's size free, and [`EXPORT_PARTITION_WORST_SECS`] left before
//! 08:30 on a session day; otherwise it stops (not a failure) before taking
//! the quiesce lock, so it never holds the lock a disk reclaim needs and
//! never reads on into the session.
//!
//! **This module never deletes, drops or overwrites anything and writes no
//! `partition_archive_audit` row** (pinned by
//! `test_daily_export_module_never_deletes_or_drops`).
//!
//! Cold path, its own task, after 15:45 IST: O(rows) per export (the export,
//! the gzip and the instrument tally each read every byte once), a
//! `HashMap` per table keyed on `(security_id, segment, feed)`, never the
//! frame drain.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{NaiveDate, Timelike};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Serialize;
use tracing::{info, warn};

use tickvault_common::config::{PartitionRetentionConfig, QuestDbConfig};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::market_hours::SessionDayInfo;

use super::snapshot_gate::{WalSnapshotVerdict, wait_for_snapshot};
use super::{
    ARCHIVE_MIN_FREE_BYTES, ARCHIVE_S3_PREFIX, ArchiveSpaceVerdict, ExportedCsv,
    PARTITION_ARCHIVE_AUDIT_TABLE, PartitionArchiver, WalApplyVerdict, archive_space_verdict,
    build_export_sql_ordered, canonical_archive_key, deferred_depth_marks_waiting,
    is_table_absent_response, partition_range_bounds, read_body_capped, remove_temp_file,
    sidecar_archive_key, wal_apply_verdict,
};
use crate::archive_quiesce::ArchiveQuiesce;
use crate::s3_cold::{ExistingObject, S3Cold, base64_encode, classify_existing, hex_encode};

/// Folder under [`ARCHIVE_S3_PREFIX`] that holds each day's manifest and DONE.
pub const DAILY_EXPORT_DIR: &str = "_daily";

/// The export's own scratch folder. Never the archiver's, which the archiver
/// empties at the start of each of its passes.
pub const DAILY_EXPORT_TEMP_DIR: &str = "data/tmp/brutex-export";

/// Manifest format version Brutex reads.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// The five hour-partitioned tables of the export, in manifest order.
const HOUR_EXPORT_TABLES: [&str; 5] = [
    "ticks",
    "tick_quotes",
    "feed_aux_packets",
    "market_depth",
    "market_depth_book",
];

/// The table a session day must have rows in before its day is sealed
/// complete (Rule 11: no rows at all is "nothing captured", never "an empty
/// market").
const CORE_TABLE: &str = "ticks";

/// Longest one table's WAL snapshot wait inside an export, in polls of
/// [`super::snapshot_gate::SNAPSHOT_POLL_SECS`] (2 s): 30 polls, one minute.
/// The scheduler already waited for applied tables before it ran; this only
/// takes a consistent snapshot.
const EXPORT_SNAPSHOT_MAX_POLLS: u32 = 30;

/// Longest wait for the shared side of the archive quiesce lock before one
/// partition: the archiver can hold the exclusive side for one partition's
/// two snapshot waits and its copy. A miss fails the attempt (retried).
pub const EXPORT_QUIESCE_WAIT_SECS: u64 = 900;

/// IST second of day from which a trading day forbids the export (08:30).
pub const EXPORT_FORBIDDEN_FROM_SECS_IST: u32 = 8 * 3600 + 30 * 60;
/// IST second of day a trading day's export window opens (15:45).
pub const EXPORT_OPEN_SECS_IST: u32 = 15 * 3600 + 45 * 60;
/// Wait after a Muhurat session's end before its export (6 minutes).
pub const MUHURAT_END_GRACE_SECS: u32 = 360;

/// What one partition can take after the recount, the tally and the upload
/// begin: a generous bound for a multi-GB depth hour, seconds.
const EXPORT_PARTITION_TAIL_SECS: u64 = 600;

/// The longest one partition can run once started: two ordered exports at
/// the export client's own timeout (`ARCHIVE_EXPORT_TIMEOUT_SECS`, 600 s
/// each, with one retry on a recount mismatch) plus
/// [`EXPORT_PARTITION_TAIL_SECS`]. A partition never starts, and a lock wait
/// never runs, unless this much time is left before 08:30 on a session day
/// (2026-11-14 review): 30 minutes.
pub const EXPORT_PARTITION_WORST_SECS: u64 =
    2 * super::ARCHIVE_EXPORT_TIMEOUT_SECS + EXPORT_PARTITION_TAIL_SECS;

/// Objects this process placed for the export (uploaded or reused), for the
/// drain hold's progress rule. One relaxed add per object.
static OBJECTS_PLACED: AtomicU64 = AtomicU64::new(0);

/// Objects the export placed in this process. O(1).
#[must_use]
// TEST-EXEMPT: covered by test_brutex_export_metrics_registered_at_zero
pub fn objects_placed_total() -> u64 {
    OBJECTS_PLACED.load(Ordering::Relaxed)
}

/// One more placed object. O(1).
fn note_object_placed() {
    OBJECTS_PLACED.fetch_add(1, Ordering::Relaxed);
}

/// Every local `/metrics` series of the export, registered at 0 (no EMF
/// name, no filter, no alarm). Called once when the scheduler arms.
// TEST-EXEMPT: covered by test_brutex_export_metrics_registered_at_zero
pub fn register_brutex_export_metrics() {
    for outcome in [
        "done",
        "failed",
        "stopped",
        "skipped_done",
        "gave_up",
        "incomplete",
        "left_window",
    ] {
        metrics::counter!("tv_brutex_export_runs_total", "outcome" => outcome).increment(0);
    }
    for placement in Placement::ALL {
        metrics::counter!("tv_brutex_export_objects_total", "placement" => placement.as_str())
            .increment(0);
    }
    metrics::counter!("tv_partition_archive_export_unordered_fallback_total").increment(0);
    metrics::counter!("tv_partition_archive_export_unordered_in_session_total").increment(0);
    metrics::counter!("tv_partition_archive_amend_records_total").increment(0);
    metrics::gauge!("tv_brutex_export_duration_secs").set(0.0);
    metrics::gauge!("tv_brutex_export_last_done_day").set(0.0);
}

// ---------------------------------------------------------------------------
// The trading-day window (shared with the scheduler and the archiver)
// ---------------------------------------------------------------------------

/// The IST second of day a session day's export window opens: 15:45 on a
/// trading day, a Muhurat session's end plus 6 minutes, the later of the
/// two on a date with both. `None` on a day with no session. Pure, O(1).
#[must_use]
pub fn export_open_secs(info: SessionDayInfo) -> Option<u32> {
    let muhurat = info
        .muhurat_end_secs
        .map(|end| end.saturating_add(MUHURAT_END_GRACE_SECS));
    match (info.trading, muhurat) {
        (false, None) => None,
        (true, None) => Some(EXPORT_OPEN_SECS_IST),
        (false, Some(m)) => Some(m),
        (true, Some(m)) => Some(m.max(EXPORT_OPEN_SECS_IST)),
    }
}

/// Whether the export may run at IST `secs` of a day with `info` (its real
/// date, not the session day): never from 08:30 until that day's window
/// opens. Pure, O(1).
#[must_use]
pub fn export_window_open(info: SessionDayInfo, secs: u32) -> bool {
    match export_open_secs(info) {
        None => true,
        Some(open) => secs < EXPORT_FORBIDDEN_FROM_SECS_IST || secs >= open,
    }
}

/// A date's sessions: the installed calendar, else Monday to Friday. O(1).
#[must_use]
// TEST-EXEMPT: covered by test_export_open_secs_per_kind_of_day
pub fn session_info(date: NaiveDate) -> SessionDayInfo {
    tickvault_common::market_hours::installed_calendar_session_day(date).unwrap_or_else(|| {
        use chrono::Datelike as _;
        SessionDayInfo {
            trading: date.weekday().number_from_monday() <= 5,
            muhurat_end_secs: None,
        }
    })
}

/// IST date and second of day now. O(1).
#[must_use]
// TEST-EXEMPT: covered by test_export_open_secs_per_kind_of_day
pub fn ist_now() -> (NaiveDate, u32) {
    let now = chrono::Utc::now().with_timezone(&tickvault_common::trading_calendar::ist_offset());
    (now.date_naive(), now.time().num_seconds_from_midnight())
}

/// Whether the export may run right now (re-checked before every table and
/// partition, so a long run stops at 08:30 instead of reading on into the
/// session). O(1).
#[must_use]
// TEST-EXEMPT: the clock read over export_window_open, which proptest_export_never_runs_inside_the_session_window_on_a_session_day covers
pub fn export_window_open_now() -> bool {
    let (date, secs) = ist_now();
    export_window_open(session_info(date), secs)
}

/// Seconds left at IST `secs` of a day with `info` before that day's 08:30
/// forbids the export; `None` when nothing forbids it before the day ends
/// (a day with no session, or past 08:30 of a session day, which is either
/// inside the window, where nothing runs, or after it opened). The run that
/// crosses midnight reads the next date's own value. Pure, O(1).
#[must_use]
pub fn secs_until_forbidden(info: SessionDayInfo, secs: u32) -> Option<u32> {
    export_open_secs(info)?;
    (secs < EXPORT_FORBIDDEN_FROM_SECS_IST).then(|| EXPORT_FORBIDDEN_FROM_SECS_IST - secs)
}

/// [`secs_until_forbidden`] now, as a duration. O(1).
#[must_use]
// TEST-EXEMPT: covered by test_secs_until_forbidden_bounds_a_run_before_0830
pub fn export_time_left_now() -> Option<Duration> {
    let (date, secs) = ist_now();
    secs_until_forbidden(session_info(date), secs).map(|s| Duration::from_secs(u64::from(s)))
}

/// The IST second of day the ARCHIVER's no-sort window ends on a day with
/// `info`: the session's ingest end (15:40 on a trading day, the Muhurat
/// session's end on a Muhurat date, the later on a date with both). `None`
/// on a day with no session. Earlier than [`export_open_secs`] on purpose
/// (2026-11-14 review, HIGH): the daily archive leg opens at 15:40:02, and an
/// unordered export there can never match the export's ordered object, so
/// every hour that leg moved before 15:45 went up a second time. Ingest has
/// ended by then, so the sort no longer competes with capture. Pure, O(1).
#[must_use]
pub fn archiver_sort_end_secs(info: SessionDayInfo) -> Option<u32> {
    let trading = info
        .trading
        .then_some(tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST);
    trading.max(info.muhurat_end_secs)
}

/// Whether the archiver must not sort at IST `secs` of a day with `info`:
/// from 08:30 until [`archiver_sort_end_secs`]. Pure, O(1).
#[must_use]
pub fn archiver_sort_forbidden(info: SessionDayInfo, secs: u32) -> bool {
    archiver_sort_end_secs(info)
        .is_some_and(|end| (EXPORT_FORBIDDEN_FROM_SECS_IST..end).contains(&secs))
}

/// [`archiver_sort_forbidden`] on `date` (its real date) at `secs`. O(1).
#[must_use]
pub fn in_trading_day_window_at(date: NaiveDate, secs: u32) -> bool {
    archiver_sort_forbidden(session_info(date), secs)
}

/// `true` from 08:30 until today's session's ingest end (a trading or
/// Muhurat day): the archiver does not sort there. O(1).
#[must_use]
// TEST-EXEMPT: wiring is a fn pointer (`Arc::new(daily_export::in_trading_day_window_now)` in both `PartitionArchiver` constructors); the rule is test_in_trading_day_window_at_bites_on_a_session_day
pub fn in_trading_day_window_now() -> bool {
    let (date, secs) = ist_now();
    in_trading_day_window_at(date, secs)
}

/// The `ORDER BY` the ARCHIVER uses for `table`'s export: the DEDUP key only
/// when an object already sits at the partition's canonical key (only then
/// can ordered bytes match it) and the clock is outside 08:30 to the
/// session's ingest end ([`archiver_sort_forbidden`]; no sort runs on
/// QuestDB during the session; there it exports
/// unordered and, if the bytes differ, writes a content-addressed copy).
/// Independent of the export switch, so turning the export off never makes
/// the archive re-copy what the export already placed. Pure, O(tables).
#[must_use]
// TEST-EXEMPT: covered by test_archiver_orders_only_against_an_existing_object_outside_the_session
pub fn archiver_export_order(
    table: &str,
    canonical_exists: bool,
    in_trading_window: bool,
) -> Option<&'static str> {
    if !canonical_exists || in_trading_window {
        return None;
    }
    export_order_by(table)
}

/// Whether the archiver must treat the canonical key as possibly holding an
/// object, from its HeadObject: present, or the HEAD FAILED (2026-11-14
/// third review). A failed HEAD read as "absent" made the archiver export
/// unordered and then, once a later HEAD saw the export's ordered object,
/// write a second full copy kept forever. Ordering when the answer is
/// unknown is safe both ways: with no object the ordered bytes simply go to
/// the canonical key. Pure, O(1).
#[must_use]
pub fn canonical_maybe_exists<T>(head: &Result<Option<T>>) -> bool {
    !matches!(head, Ok(None))
}

/// The two export tables stamped with ARRIVAL time (the frame's receipt),
/// which take rows after the close for as long as the main feed is
/// connected: out-of-window ticks, market status and open interest in
/// `feed_aux_packets`, late quotes in `tick_quotes`.
pub const ARRIVAL_TIME_TABLES: [&str; 2] = ["tick_quotes", "feed_aux_packets"];

/// Slack after an hour ends before it counts as closed for the arrival-time
/// tables: the tick writer's flush and the WAL apply (the export also waits
/// for the table's WAL snapshot), seconds.
pub const ARRIVAL_SETTLE_SECS: u32 = 120;

/// Seconds per hour.
const SECS_PER_HOUR: u32 = 3_600;

/// The first instant of `day` whose arrival-time hour may still take rows
/// when the export runs at IST `now_date`, `now_secs` (2026-11-14 third
/// review): the start of the hour [`ARRIVAL_SETTLE_SECS`] ago, while the
/// clock is still on `day`'s date; `None` once the clock has passed `day`
/// (every hour of `day` has ended; receipt time is never earlier). A clock
/// before `day` reads every hour open. Pure, O(1).
#[must_use]
pub fn arrival_open_from(
    day: NaiveDate,
    now_date: NaiveDate,
    now_secs: u32,
) -> Option<chrono::NaiveDateTime> {
    use std::cmp::Ordering as Cmp;
    match now_date.cmp(&day) {
        Cmp::Greater => None,
        Cmp::Less => day.and_hms_opt(0, 0, 0),
        Cmp::Equal => {
            let hour = now_secs.saturating_sub(ARRIVAL_SETTLE_SECS) / SECS_PER_HOUR;
            day.and_hms_opt(hour.min(23), 0, 0)
        }
    }
}

/// The IST second of day from which the session day's arrival-time hours
/// that hold session rows have all ended: the end of the hour holding the
/// session's last second (16:00 after a 15:40 close, 20:00 after a 19:15
/// Muhurat end) plus [`ARRIVAL_SETTLE_SECS`]. Before it the export would
/// leave the close's own hour out, so the scheduler waits. `None` on a day
/// with no session. Pure, O(1).
#[must_use]
pub fn arrival_close_secs(info: SessionDayInfo) -> Option<u32> {
    let end = archiver_sort_end_secs(info)?;
    let hour_end = end.saturating_sub(1) / SECS_PER_HOUR * SECS_PER_HOUR + SECS_PER_HOUR;
    Some(hour_end.saturating_add(ARRIVAL_SETTLE_SECS))
}

/// Whether `partition` of `table` is left out because its hour may still
/// take arrival-time rows: an hour partition of an [`ARRIVAL_TIME_TABLES`]
/// table starting at or after `open_from`. Pure, O(1).
#[must_use]
pub fn left_out_as_open(
    table: &str,
    partition: &str,
    open_from: Option<chrono::NaiveDateTime>,
) -> bool {
    let Some(cut) = open_from else {
        return false;
    };
    ARRIVAL_TIME_TABLES.contains(&table)
        && partition.len() == 13
        && chrono::NaiveDateTime::parse_from_str(&format!("{partition}:00:00"), "%Y-%m-%dT%H:%M:%S")
            .is_ok_and(|start| start >= cut)
}

/// Free bytes the volume must keep so the disk-pressure archiver never
/// needs the lock the export holds (2026-11-14 third review): the bytes
/// below `high_water_pct` used (pressure starts AT that line). The export
/// stops (not a failure) before a partition that would take the volume
/// under it. Pure, O(1).
#[must_use]
pub fn pressure_free_floor_bytes(total: u64, high_water_pct: u8) -> u64 {
    let free_pct = 100_u64.saturating_sub(u64::from(high_water_pct.min(100)));
    u64::try_from(u128::from(total) * u128::from(free_pct) / 100).unwrap_or(u64::MAX)
}

/// Every table the export covers: the five hour tables, then every emitted
/// candle table. O(tables).
#[must_use]
// TEST-EXEMPT: covered by test_export_order_by_equals_dedup_key_for_every_export_table
pub fn export_tables() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = HOUR_EXPORT_TABLES.to_vec();
    out.extend(crate::shadow_persistence::emitted_candle_table_names());
    out
}

/// The `ORDER BY` of `table`'s export: its DEDUP key constant (unique per
/// row, so the order is total). `None` for a table outside the export set,
/// which keeps the archiver's unordered query. O(tables).
#[must_use]
pub fn export_order_by(table: &str) -> Option<&'static str> {
    match table {
        "ticks" => Some(crate::tick_persistence::DEDUP_KEY_TICKS),
        "tick_quotes" => Some(crate::tick_quotes_persistence::DEDUP_KEY_TICK_QUOTES),
        "feed_aux_packets" => Some(crate::feed_aux_persistence::DEDUP_KEY_FEED_AUX_PACKETS),
        "market_depth" => Some(crate::depth_persistence::DEDUP_KEY_MARKET_DEPTH),
        "market_depth_book" => Some(crate::depth_persistence::DEDUP_KEY_MARKET_DEPTH_BOOK),
        other => crate::shadow_persistence::emitted_candle_table_names()
            .contains(&other)
            .then_some(crate::shadow_persistence::DEDUP_KEY_CANDLES),
    }
}

/// The partitions of `names` that belong to IST day `day`: the DAY partition
/// `YYYY-MM-DD` and every HOUR partition `YYYY-MM-DDTHH`, sorted. O(n log n).
#[must_use]
pub fn partitions_of_day<'a>(
    names: impl IntoIterator<Item = &'a str>,
    day: NaiveDate,
) -> Vec<String> {
    let day_name = day.format("%Y-%m-%d").to_string();
    let mut out: Vec<String> = names
        .into_iter()
        .filter(|n| {
            *n == day_name
                || (n.len() == 13
                    && n.starts_with(day_name.as_str())
                    && n.as_bytes().get(10) == Some(&b'T'))
        })
        .map(ToString::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `questdb-partitions/_daily/<D>/`.
#[must_use]
// TEST-EXEMPT: covered by test_key_naming_is_under_the_archive_prefix
pub fn daily_prefix(day: NaiveDate) -> String {
    format!(
        "{ARCHIVE_S3_PREFIX}/{DAILY_EXPORT_DIR}/{}/",
        day.format("%Y-%m-%d")
    )
}

/// The key of day D's DONE marker.
#[must_use]
// TEST-EXEMPT: covered by test_key_naming_is_under_the_archive_prefix
pub fn done_key(day: NaiveDate) -> String {
    format!("{}DONE", daily_prefix(day))
}

/// The key of attempt `n`'s manifest.
#[must_use]
// TEST-EXEMPT: covered by test_key_naming_is_under_the_archive_prefix
pub fn manifest_key(day: NaiveDate, attempt: u32) -> String {
    format!("{}manifest-{attempt}.json", daily_prefix(day))
}

/// The key of attempt `n`'s per-instrument row counts.
#[must_use]
// TEST-EXEMPT: covered by test_key_naming_is_under_the_archive_prefix
pub fn counts_key(day: NaiveDate, attempt: u32) -> String {
    format!("{}instrument-counts-{attempt}.csv.gz", daily_prefix(day))
}

/// The key of an amend record: a copy of `partition` the archiver verified
/// in S3 that an export manifest may not name. `sha_hex` is shortened to 12 hex
/// digits for the name; the record carries the full digest.
#[must_use]
// TEST-EXEMPT: covered by test_key_naming_is_under_the_archive_prefix
pub fn amend_key(day: NaiveDate, table: &str, partition: &str, sha_hex: &str) -> String {
    let short: String = sha_hex.chars().take(12).collect();
    format!(
        "{}amend-{table}-{partition}-{short}.json",
        daily_prefix(day)
    )
}

/// The next attempt number for day D from a listing of its `_daily/<D>/`
/// folder: one more than the highest `n` any `manifest-<n>.json` OR
/// `instrument-counts-<n>.csv.gz` carries, 1 when none. A counts file whose
/// manifest write failed (or was cut by a stop) still burns its `n`, so the
/// next attempt never collides with it on the create-only PUT. O(keys).
#[must_use]
pub fn next_attempt(keys: &[(String, u64)], day: NaiveDate) -> u32 {
    let prefix = daily_prefix(day);
    keys.iter()
        .filter_map(|(k, _)| {
            let name = k.strip_prefix(prefix.as_str())?;
            let n = name
                .strip_prefix("manifest-")
                .and_then(|r| r.strip_suffix(".json"))
                .or_else(|| {
                    name.strip_prefix("instrument-counts-")
                        .and_then(|r| r.strip_suffix(".csv.gz"))
                })?;
            n.parse::<u32>().ok()
        })
        .max()
        .map_or(1, |n| n.saturating_add(1))
}

/// Where a partition's object ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// Uploaded to the canonical key now.
    Uploaded,
    /// The canonical key already held these exact bytes.
    Reused,
    /// The canonical key held other bytes; uploaded to the content-addressed key.
    Sidecar,
    /// The content-addressed key already held these exact bytes.
    SidecarReused,
    /// A verified archiver copy this export did not place (archived and
    /// dropped before or during the export, or an earlier copy of a partition
    /// that late rows re-created); listed from its audit row and confirmed in
    /// S3.
    ArchivedBeforeExport,
}

impl Placement {
    /// Every placement, for the zero-registered counter labels.
    pub const ALL: [Self; 5] = [
        Self::Uploaded,
        Self::Reused,
        Self::Sidecar,
        Self::SidecarReused,
        Self::ArchivedBeforeExport,
    ];

    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uploaded => "uploaded",
            Self::Reused => "reused",
            Self::Sidecar => "sidecar",
            Self::SidecarReused => "sidecar_reused",
            Self::ArchivedBeforeExport => "archived_before_export",
        }
    }
}

/// What to do at the canonical key, from its HeadObject. Pure, O(1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CanonicalStep {
    Reuse,
    Upload,
    GoToSidecar,
}

/// [`CanonicalStep`] for what sits at the canonical key. Never an overwrite.
pub(crate) const fn canonical_step(existing: ExistingObject) -> CanonicalStep {
    match existing {
        ExistingObject::Identical => CanonicalStep::Reuse,
        ExistingObject::Absent => CanonicalStep::Upload,
        ExistingObject::Different => CanonicalStep::GoToSidecar,
    }
}

// ---------------------------------------------------------------------------
// Per-instrument tally (quote-aware CSV field split over the exported bytes)
// ---------------------------------------------------------------------------

/// Field parser state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldState {
    /// Outside quotes.
    Plain,
    /// Inside a quoted field.
    Quoted,
    /// Saw a `"` inside a quoted field: the next byte decides.
    QuoteInQuoted,
}

/// Counts data rows per `(security_id, segment, feed)` from a CSV byte
/// stream whose first record is the header. RFC 4180 quoting (`""` is a
/// literal quote; commas and newlines inside quotes belong to the field).
/// O(bytes); one `String` allocation per NEW instrument only.
#[derive(Debug)]
pub(crate) struct InstrumentTally {
    state: FieldState,
    field_idx: usize,
    /// Column indexes of security_id, segment, feed (`None` until the header).
    wanted: Option<[Option<usize>; 3]>,
    header: Vec<String>,
    current: Vec<u8>,
    values: [Vec<u8>; 3],
    record_has_bytes: bool,
    key: String,
    counts: HashMap<String, u64>,
    rows: u64,
}

impl Default for InstrumentTally {
    fn default() -> Self {
        Self {
            state: FieldState::Plain,
            field_idx: 0,
            wanted: None,
            header: Vec::new(),
            current: Vec::new(),
            values: [Vec::new(), Vec::new(), Vec::new()],
            record_has_bytes: false,
            key: String::new(),
            counts: HashMap::new(),
            rows: 0,
        }
    }
}

/// Separator inside a tally key (never in a security id, segment or feed).
const KEY_SEP: char = '\u{1f}';

impl InstrumentTally {
    /// Feeds one chunk of raw CSV bytes.
    pub(crate) fn update(&mut self, chunk: &[u8]) {
        for &b in chunk {
            self.record_has_bytes = true;
            match (self.state, b) {
                (FieldState::Plain, b'"') if self.current.is_empty() => {
                    self.state = FieldState::Quoted;
                }
                (FieldState::Plain | FieldState::QuoteInQuoted, b',') => {
                    self.state = FieldState::Plain;
                    self.end_field();
                }
                (FieldState::Plain | FieldState::QuoteInQuoted, b'\n') => {
                    self.state = FieldState::Plain;
                    self.end_record();
                }
                (FieldState::Plain | FieldState::QuoteInQuoted, b'\r') => {}
                (FieldState::Quoted, b'"') => self.state = FieldState::QuoteInQuoted,
                (FieldState::QuoteInQuoted, b'"') => {
                    self.state = FieldState::Quoted;
                    self.current.push(b'"');
                }
                (FieldState::QuoteInQuoted, other) => {
                    self.state = FieldState::Plain;
                    self.current.push(other);
                }
                (_, other) => self.current.push(other),
            }
        }
    }

    /// Ends the stream: a last record without a trailing newline counts.
    pub(crate) fn finish(&mut self) {
        if self.record_has_bytes {
            self.end_record();
        }
    }

    fn end_field(&mut self) {
        match &self.wanted {
            None => {
                self.header
                    .push(String::from_utf8_lossy(&self.current).into_owned());
            }
            Some(wanted) => {
                for (slot, idx) in wanted.iter().enumerate() {
                    if *idx == Some(self.field_idx) {
                        std::mem::swap(&mut self.values[slot], &mut self.current);
                    }
                }
            }
        }
        self.current.clear();
        self.field_idx = self.field_idx.saturating_add(1);
    }

    fn end_record(&mut self) {
        self.end_field();
        if self.wanted.is_none() {
            let find = |name: &str| self.header.iter().position(|h| h == name);
            self.wanted = Some([find("security_id"), find("segment"), find("feed")]);
        } else {
            self.key.clear();
            for (i, v) in self.values.iter_mut().enumerate() {
                if i > 0 {
                    self.key.push(KEY_SEP);
                }
                self.key.push_str(&String::from_utf8_lossy(v));
                v.clear();
            }
            if let Some(n) = self.counts.get_mut(self.key.as_str()) {
                *n = n.saturating_add(1);
            } else {
                self.counts.insert(self.key.clone(), 1);
            }
            self.rows = self.rows.saturating_add(1);
        }
        self.field_idx = 0;
        self.record_has_bytes = false;
    }

    /// Data rows counted (header excluded).
    pub(crate) const fn rows(&self) -> u64 {
        self.rows
    }

    /// `(security_id, segment, feed) -> rows`.
    pub(crate) fn counts(&self) -> &HashMap<String, u64> {
        &self.counts
    }

    /// The CSV header's column names (empty before the first record).
    pub(crate) fn header(&self) -> &[String] {
        &self.header
    }
}

/// Tallies the gzip'd CSV at `path` (the very bytes uploaded). Blocking;
/// call through `off_worker`. O(bytes).
fn tally_gzip_file(path: &Path) -> Result<InstrumentTally> {
    let file = std::fs::File::open(path).context("open export for the instrument tally")?;
    let mut decoder = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
    let mut tally = InstrumentTally::default();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = decoder
            .read(&mut buf)
            .context("read export for the instrument tally")?;
        if n == 0 {
            break;
        }
        tally.update(&buf[..n]);
    }
    tally.finish();
    Ok(tally)
}

/// Renders the per-instrument counts file body (header + one sorted line per
/// `(table, security_id, segment, feed)`). O(n log n).
// TEST-EXEMPT: covered by test_tally_keys_on_security_id_segment_feed
pub(crate) fn render_counts_csv(by_table: &BTreeMap<String, HashMap<String, u64>>) -> String {
    let mut out = String::from("table,security_id,segment,feed,rows\n");
    for (table, counts) in by_table {
        let sorted: BTreeMap<&String, &u64> = counts.iter().collect();
        for (key, rows) in sorted {
            let fields: Vec<String> = key.split(KEY_SEP).map(csv_field).collect();
            out.push_str(&format!(
                "{},{},{rows}\n",
                csv_field(table),
                fields.join(",")
            ));
        }
    }
    out
}

/// One CSV field, quoted when it holds a comma, quote or newline.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Manifest and DONE
// ---------------------------------------------------------------------------

/// One partition in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestPartition {
    pub partition: String,
    pub key: String,
    pub placement: Placement,
    pub rows: u64,
    pub gzip_bytes: u64,
    pub sha256_hex: String,
    /// Uncompressed CSV bytes; `None` for a partition archived before the export.
    pub csv_bytes: Option<u64>,
    /// Whether this partition's rows are in the instrument counts file.
    pub instrument_counts_known: bool,
    /// Whether the object's rows are known to be in the table's `order_by`
    /// order: `true` for an object this export wrote or matched byte for
    /// byte (uploaded, reused, content-addressed); `false` for a copy the
    /// archiver made before the export ([`Placement::ArchivedBeforeExport`]),
    /// which is often unordered (2026-11-14 third review). A reader that
    /// needs the order sorts a `false` object itself.
    pub ordered_by_dedup_key: bool,
}

/// One table in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestTable {
    pub table: String,
    /// The order of the objects whose `ordered_by_dedup_key` is `true`.
    pub order_by: String,
    /// The CSV header of this export's objects (empty when it exported
    /// none). Byte reuse by the archive depends on it: a column added to the
    /// table later changes every not-yet-archived partition's bytes, and the
    /// archive then writes a second copy (an owner decision, recorded with
    /// Quote 60).
    pub columns: Vec<String>,
    /// For the arrival-time tables on the session day: rows at or after this
    /// IST instant (`YYYY-MM-DDTHH:MM:SS+05:30`) are NOT in this manifest,
    /// because their hours were still taking rows; `None` when every hour of
    /// the day is covered. A later copy of such an hour appears as an amend
    /// record.
    pub open_from_ist: Option<String>,
    /// The partitions left out under `open_from_ist`.
    pub open_partitions_left_out: Vec<String>,
    pub rows: u64,
    /// `true` when a partition is listed more than once (an archived copy
    /// and the rows re-created after it, or two archived copies). `rows` is
    /// then the sum over objects, and a reader takes the union of the
    /// partition's objects keyed on the DEDUP key (`order_by`), which is unique
    /// per row.
    pub rows_may_overlap: bool,
    pub partitions: Vec<ManifestPartition>,
}

/// A small object the manifest points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectRef {
    pub key: String,
    pub bytes: u64,
    pub sha256_hex: String,
}

/// `manifest-<n>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub day: String,
    pub attempt: u32,
    pub export_started_ist: String,
    pub export_finished_ist: String,
    /// `true` when every readiness gate held; otherwise `incomplete_reasons`
    /// says what was still pending when the export ran.
    pub complete: bool,
    pub incomplete_reasons: Vec<String>,
    pub tables: Vec<ManifestTable>,
    pub instrument_counts: ObjectRef,
}

/// The DONE marker body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoneRecord {
    pub manifest_key: String,
    pub manifest_sha256_hex: String,
    pub written_at_ist: String,
}

/// An amend record body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AmendRecord {
    pub table: String,
    pub partition: String,
    pub key: String,
    pub rows: u64,
    pub gzip_bytes: u64,
    pub sha256_hex: String,
    pub written_at_ist: String,
}

/// SHA-256 of `bytes` as (base64, hex). O(bytes).
fn sha256_pair(bytes: &[u8]) -> (String, String) {
    use sha2::Digest as _;
    let digest: [u8; 32] = sha2::Sha256::digest(bytes).into();
    (base64_encode(&digest), hex_encode(&digest))
}

/// Base64 of the bytes a lowercase hex string spells; `None` for bad hex.
fn hex_to_base64(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect();
    bytes.map(|b| base64_encode(&b))
}

/// Now in IST, `YYYY-MM-DDTHH:MM:SS+05:30`.
fn now_ist_text() -> String {
    chrono::Utc::now()
        .with_timezone(&tickvault_common::trading_calendar::ist_offset())
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// gzip of `bytes` with the archiver's settings. O(bytes).
fn gzip_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(bytes).context("gzip counts file")?;
    enc.finish().context("finish gzip counts file")
}

/// Writes the amend record for an archiver copy of an export-table
/// partition: called by the archiver after EVERY verified copy (uploaded or
/// reused, canonical or content-addressed) and before its audit row and drop.
/// With the export switched OFF it is still written for a day that already
/// has a DONE (2026-11-14 third review: the documented rollback must not
/// silently stop advertising later copies of days already sealed), and for
/// a day whose DONE cannot be read (keep more); a day with no DONE gets
/// none, so a box that never exported writes no records. One HeadObject
/// per copy while the switch is off.
/// A reuse of the canonical object is included (2026-11-14 review): an hour
/// an earlier export attempt placed and the archiver then reused and dropped
/// is in no manifest when the next attempt cannot yet read its audit row. Written whatever the
/// day's DONE state, so a copy made between the export's placement and its
/// DONE is never lost to Brutex. Create-only and idempotent: a record that
/// already exists counts as written. `Err` makes the archiver keep the
/// partition (retried next pass). Cold: once per such copy.
///
/// # Errors
/// The record could not be written and is not already in S3.
#[allow(clippy::too_many_arguments)] // APPROVED: one call site, every field is the archived copy's
// TEST-EXEMPT: covered by test_archiver_copy_writes_one_amend_record_without_done, test_amend_failure_keeps_the_partition and test_switch_off_still_amends_a_sealed_day
pub(crate) async fn note_archived_copy(
    cold: &S3Cold,
    export_enabled: bool,
    table: &str,
    partition: &str,
    key: &str,
    rows: u64,
    gzip_bytes: u64,
    sha256_hex: &str,
) -> Result<()> {
    if export_order_by(table).is_none() {
        return Ok(());
    }
    let Some(day) = partition
        .get(..10)
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
    else {
        return Ok(());
    };
    if !export_enabled && matches!(cold.head(&done_key(day)).await, Ok(None)) {
        // Switched off and the day was never sealed: nothing to amend.
        return Ok(());
    }
    let record = AmendRecord {
        table: table.to_string(),
        partition: partition.to_string(),
        key: key.to_string(),
        rows,
        gzip_bytes,
        sha256_hex: sha256_hex.to_string(),
        written_at_ist: now_ist_text(),
    };
    let body = serde_json::to_vec_pretty(&record).context("render the amend record")?;
    let (sha_b64, _) = sha256_pair(&body);
    let amend = amend_key(day, table, partition, sha256_hex);
    match cold.put_if_absent_bytes(&amend, body, &sha_b64, &[]).await {
        Ok(()) => {
            metrics::counter!("tv_partition_archive_amend_records_total").increment(1);
            info!(table, partition, key = %amend, "brutex export: amend record written");
            Ok(())
        }
        // A retried pass finds its own earlier record: already written.
        Err(_) if cold.head(&amend).await.ok().flatten().is_some() => Ok(()),
        Err(err) => Err(err.context(format!("amend record {amend} not written"))),
    }
}

// ---------------------------------------------------------------------------
// The exporter
// ---------------------------------------------------------------------------

/// What one sealed export reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub attempt: u32,
    pub tables: usize,
    pub rows: u64,
    pub objects: usize,
    pub complete: bool,
    pub incomplete_reasons: Vec<String>,
    /// `false` when another writer sealed the day first (its DONE stands).
    pub sealed_by_this_run: bool,
}

/// Why an attempt stopped without a seal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The trading-day window is closing (`keep_going` false, or less than
    /// [`EXPORT_PARTITION_WORST_SECS`] left before 08:30).
    Window,
    /// Less than the disk floor plus the next partition's size is free (or
    /// the volume could not be read). The caller waits before trying again.
    Disk,
}

impl StopReason {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Disk => "disk",
        }
    }
}

/// What one export attempt ended with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportOutcome {
    /// Day D has a DONE (written by this attempt or found).
    Sealed(ExportReport),
    /// Stopped before the seal: nothing failed, nothing is sealed, and the
    /// attempt is not counted. Whatever was placed stays placed; the next
    /// attempt reuses it.
    Stopped(StopReason),
}

/// How the caller runs one attempt.
#[derive(Clone, Copy)]
pub struct ExportOptions<'a> {
    /// Re-checked before every table and every partition (and again after
    /// the quiesce lock is taken), and before the seal: `false` stops the
    /// attempt ([`StopReason::Window`]).
    pub keep_going: &'a (dyn Fn() -> bool + Sync),
    /// Time left before the export is forbidden (08:30 on a session day);
    /// `None` = nothing forbids it before the day ends. A partition never
    /// starts, and the lock wait is cut, so it ends before that instant.
    pub time_left: &'a (dyn Fn() -> Option<Duration> + Sync),
    /// The free bytes the volume must keep for a volume of the given size
    /// (production: the larger of the after-close depth pass's floor and
    /// [`pressure_free_floor_bytes`]). The export needs this plus the
    /// partition's on-disk size free before each partition.
    pub floor_for_total: &'a (dyn Fn(u64) -> u64 + Sync),
    /// For the session day: the first IST instant whose hour of the
    /// arrival-time tables may still take rows ([`arrival_open_from`]); hours
    /// at or after it are left out and named in the manifest. `None` for a
    /// day whose hours have all ended.
    pub open_from: Option<chrono::NaiveDateTime>,
    /// The last attempt this day gets (a forced catch-up day): a day with
    /// no `ticks` partition is then sealed INCOMPLETE with
    /// `no_ticks_captured`; on every other attempt it is refused, because a
    /// replay may still bring its rows.
    pub last_chance: bool,
}

/// Counts and logs a stopped attempt. O(1).
fn stopped(day: NaiveDate, at: &str, reason: StopReason) -> ExportOutcome {
    metrics::counter!("tv_brutex_export_runs_total", "outcome" => "stopped").increment(1);
    info!(
        %day, at, reason = reason.as_str(),
        "brutex export: stopped (not sealed, not a failure)"
    );
    ExportOutcome::Stopped(reason)
}

/// Whether the volume can take the next partition: free bytes at or above
/// `floor_for_total(total)` plus `expected_bytes` (the partition's on-disk
/// size, a bound on its gzip). An unread volume is "no" (the export is
/// optional work; the depth pass and the WAL prune are not). Pure, O(1).
#[must_use]
pub(crate) fn export_space_ok(
    probe: Option<(u64, u64)>,
    expected_bytes: u64,
    floor_for_total: &dyn Fn(u64) -> u64,
) -> bool {
    probe.is_some_and(|(free, total)| free >= floor_for_total(total).saturating_add(expected_bytes))
}

/// One verified `partition_archive_audit` row of an archived partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchivedRow {
    pub table: String,
    pub partition: String,
    pub key: String,
    pub rows: u64,
    pub gzip_bytes: u64,
    pub sha256_hex: String,
}

/// Parses the verified-audit-row query (`table_name, partition_name, s3_key,
/// rows_archived, gzip_bytes, gzip_sha256`). EVERY distinct copy of a
/// partition is kept (an archived bulk and a later copy of rows that
/// re-created the partition are both data); the last row per
/// `(table, partition, key)` wins. Fail-closed: an unreadable body is
/// `None`. O(rows log rows).
pub(crate) fn parse_archived_rows(json: &str) -> Option<Vec<ArchivedRow>> {
    let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut by_copy: BTreeMap<(String, String, String), ArchivedRow> = BTreeMap::new();
    for row in parsed.get("dataset")?.as_array()? {
        let r = row.as_array()?;
        let text = |i: usize| r.get(i)?.as_str().map(ToString::to_string);
        let num = |i: usize| r.get(i)?.as_u64();
        let row = ArchivedRow {
            table: text(0)?,
            partition: text(1)?,
            key: text(2)?,
            rows: num(3)?,
            gzip_bytes: num(4)?,
            sha256_hex: text(5)?,
        };
        by_copy.insert(
            (row.table.clone(), row.partition.clone(), row.key.clone()),
            row,
        );
    }
    Some(by_copy.into_values().collect())
}

/// One exported table, before the archived copies are merged in.
struct TableResult {
    entry: ManifestTable,
    counts: HashMap<String, u64>,
    /// Listed partitions the archiver dropped before this export could take
    /// the quiesce lock for them: an archived copy must cover each one.
    vanished: Vec<String>,
}

/// One placed partition: its manifest entry, its per-instrument counts and
/// the CSV header of its object.
struct PlacedPartition {
    entry: ManifestPartition,
    counts: HashMap<String, u64>,
    columns: Vec<String>,
}

/// One partition's result.
enum PartitionStep {
    Placed(Box<PlacedPartition>),
    /// Gone from QuestDB when re-listed under the lock, or after a failed
    /// or empty export (dropped by the archiver; the exact-gate path drops
    /// without the lock).
    Vanished,
    /// Not started (or given up before the export): the attempt stops.
    Stop(StopReason),
}

/// One table's result.
enum TableStep {
    Done(Box<TableResult>),
    Stop(StopReason),
}

/// Adds every verified archived copy of `t`'s table that the export did not
/// itself place, and checks every vanished partition is covered. `confirm`
/// answers whether a copy is intact in S3. Sorts the partitions and sets
/// `rows` and `rows_may_overlap`. O(copies log copies).
async fn merge_archived<F, Fut>(
    t: &mut TableResult,
    archived: &[ArchivedRow],
    reasons: &mut Vec<String>,
    confirm: F,
) -> Result<()>
where
    F: Fn(ArchivedRow) -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let placed: BTreeSet<String> = t.entry.partitions.iter().map(|p| p.key.clone()).collect();
    for row in archived.iter().filter(|r| r.table == t.entry.table) {
        if placed.contains(&row.key) {
            continue;
        }
        if !confirm(row.clone()).await? {
            reasons.push(format!(
                "archived_copy_unconfirmed:{}/{}",
                row.table, row.partition
            ));
            continue;
        }
        note_object_placed();
        metrics::counter!("tv_brutex_export_objects_total",
            "placement" => Placement::ArchivedBeforeExport.as_str())
        .increment(1);
        t.entry.partitions.push(ManifestPartition {
            partition: row.partition.clone(),
            key: row.key.clone(),
            placement: Placement::ArchivedBeforeExport,
            rows: row.rows,
            gzip_bytes: row.gzip_bytes,
            sha256_hex: row.sha256_hex.clone(),
            csv_bytes: None,
            instrument_counts_known: false,
            ordered_by_dedup_key: false,
        });
    }
    for gone in &t.vanished {
        anyhow::ensure!(
            t.entry.partitions.iter().any(|p| &p.partition == gone),
            "{}/{gone} was dropped during the export and no intact verified archive copy \
             lists it yet; not sealed (retried)",
            t.entry.table
        );
    }
    t.entry
        .partitions
        .sort_by(|a, b| a.partition.cmp(&b.partition).then(a.key.cmp(&b.key)));
    t.entry.rows = t.entry.partitions.iter().map(|p| p.rows).sum();
    t.entry.rows_may_overlap = t
        .entry
        .partitions
        .windows(2)
        .any(|w| w[0].partition == w[1].partition);
    Ok(())
}

/// The daily export, on its own [`PartitionArchiver`] (its own temp folder,
/// its own clients). It uses only the archiver's export, HeadObject, upload
/// and recount steps.
pub struct DailyExporter {
    archiver: PartitionArchiver,
    /// The archive quiesce lock: the process lock in production, a private
    /// one in tests.
    quiesce: &'static ArchiveQuiesce,
    /// How long one partition waits for that lock.
    quiesce_wait: Duration,
}

impl DailyExporter {
    /// Builds the exporter; `Ok(None)` when no cold bucket resolves (dev).
    ///
    /// # Errors
    /// HTTP-client build failure.
    // TEST-EXEMPT: builds live AWS/HTTP clients; the export itself is covered by daily_export_stub_tests through from_archiver
    pub async fn new(
        questdb: &QuestDbConfig,
        cfg: &PartitionRetentionConfig,
    ) -> Result<Option<Self>> {
        let Some(mut archiver) = PartitionArchiver::new(questdb, cfg).await? else {
            return Ok(None);
        };
        archiver.temp_dir = std::path::PathBuf::from(DAILY_EXPORT_TEMP_DIR);
        Ok(Some(Self {
            archiver,
            quiesce: crate::archive_quiesce::process_archive_quiesce(),
            quiesce_wait: Duration::from_secs(EXPORT_QUIESCE_WAIT_SECS),
        }))
    }

    /// Test constructor around a stub-backed archiver, with its own lock.
    #[cfg(test)]
    pub(crate) fn from_archiver(archiver: PartitionArchiver) -> Self {
        Self::with_quiesce(
            archiver,
            Box::leak(Box::new(ArchiveQuiesce::new())),
            Duration::from_secs(EXPORT_QUIESCE_WAIT_SECS),
        )
    }

    /// Test constructor with a given lock (to hold the archiver's side).
    #[cfg(test)]
    // TEST-EXEMPT: covered by test_export_waits_for_the_archive_quiesce_lock
    pub(crate) fn with_quiesce(
        archiver: PartitionArchiver,
        quiesce: &'static ArchiveQuiesce,
        quiesce_wait: Duration,
    ) -> Self {
        Self {
            archiver,
            quiesce,
            quiesce_wait,
        }
    }

    /// Whether day D already has a DONE in S3.
    ///
    /// # Errors
    /// HeadObject failed (not "absent").
    // TEST-EXEMPT: one HeadObject; covered by test_done_present_means_nothing_to_do
    pub async fn is_sealed(&self, day: NaiveDate) -> Result<bool> {
        Ok(self.archiver.cold.head(&done_key(day)).await?.is_some())
    }

    /// What is still pending right now (empty = ready): an export table, or
    /// `partition_archive_audit` (whose verified rows list the partitions
    /// the archiver already dropped), whose WAL is not applied; a spill
    /// folder with unread files; skipped-depth marks. The spill folders are
    /// read for every day (2026-11-14 review): a spill file can hold rows of
    /// any earlier day, so a past day reads them as pending too.
    /// O(tables), one `wal_tables()` query.
    // TEST-EXEMPT: covered by test_pending_reasons_name_a_lagging_table
    pub async fn pending_reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        match self.archiver.fetch_wal_tables().await {
            Ok(rows) => {
                for table in export_tables()
                    .into_iter()
                    .chain([PARTITION_ARCHIVE_AUDIT_TABLE])
                {
                    match wal_apply_verdict(&rows, table) {
                        WalApplyVerdict::Applied | WalApplyVerdict::NotReported => {}
                        _ => reasons.push(format!("wal_not_applied:{table}")),
                    }
                }
            }
            Err(_) => reasons.push("wal_unreadable".to_string()),
        }
        for table in export_tables() {
            if crate::off_worker::off_worker(|| {
                PartitionArchiver::spill_dirs_have_pending_data(table)
            }) {
                reasons.push(format!("spill_pending:{table}"));
            }
        }
        if deferred_depth_marks_waiting() {
            reasons.push("deferred_depth_marks".to_string());
        }
        reasons.dedup();
        reasons
    }

    /// Exports day D and seals it: every table (each partition under the
    /// shared quiesce lock), then the verified archive copies read after the
    /// last table, the counts file, the manifest, a re-check of every listed
    /// key, then DONE. `reasons` are the gates the caller saw unmet; they go
    /// into the manifest. Nothing is ever deleted or overwritten.
    ///
    /// # Errors
    /// Any table could not be exported and verified, a dropped partition has
    /// no intact archived copy yet, a session day has no `ticks` partition
    /// (except on the last chance), or an S3 write failed; no DONE is
    /// written and the caller retries with the next attempt.
    // TEST-EXEMPT: covered by the daily_export_stub_tests (test_daily_export_writes_done_last and siblings)
    pub async fn export_day(
        &mut self,
        day: NaiveDate,
        mut reasons: Vec<String>,
        opts: ExportOptions<'_>,
    ) -> Result<ExportOutcome> {
        let started = now_ist_text();
        let clock = std::time::Instant::now();
        if self.is_sealed(day).await? {
            metrics::counter!("tv_brutex_export_runs_total", "outcome" => "skipped_done")
                .increment(1);
            return Ok(ExportOutcome::Sealed(ExportReport {
                attempt: 0,
                tables: 0,
                rows: 0,
                objects: 0,
                complete: true,
                incomplete_reasons: Vec::new(),
                sealed_by_this_run: false,
            }));
        }
        let listing = self
            .archiver
            .cold
            .list_keys(&daily_prefix(day))
            .await
            .context("list the day's export folder")?;
        let attempt = next_attempt(&listing, day);
        self.archiver.clean_temp_dir();

        let mut results = Vec::new();
        for table in export_tables() {
            if !(opts.keep_going)() {
                return Ok(stopped(day, table, StopReason::Window));
            }
            match self.export_table(table, day, &mut reasons, opts).await {
                Ok(TableStep::Done(t)) => results.push(*t),
                Ok(TableStep::Stop(reason)) => return Ok(stopped(day, table, reason)),
                Err(err) => {
                    metrics::counter!("tv_brutex_export_runs_total", "outcome" => "failed")
                        .increment(1);
                    return Err(err.context(format!("brutex export of {table} for {day}")));
                }
            }
        }

        // Read AFTER every table: a partition the archiver copied and dropped
        // while this export ran is listed from its verified audit row.
        let archived = self.archived_rows(day).await?;
        for t in &mut results {
            let cold = &self.archiver.cold;
            merge_archived(t, &archived, &mut reasons, |row| async move {
                let meta = cold.head(&row.key).await?;
                let want = hex_to_base64(&row.sha256_hex);
                Ok(meta.as_ref().is_some_and(|m| {
                    m.len == row.gzip_bytes && want.is_some() && m.checksum_sha256_b64 == want
                }))
            })
            .await
            .inspect_err(|_| {
                metrics::counter!("tv_brutex_export_runs_total", "outcome" => "failed")
                    .increment(1);
            })?;
            metrics::counter!("tv_brutex_export_rows_total", "table" => t.entry.table.clone())
                .increment(t.entry.rows);
        }
        let core_objects = results
            .iter()
            .find(|t| t.entry.table == CORE_TABLE)
            .map_or(0, |t| t.entry.partitions.len());
        if core_objects == 0 {
            if opts.last_chance {
                reasons.push("no_ticks_captured".to_string());
            } else {
                metrics::counter!("tv_brutex_export_runs_total", "outcome" => "failed")
                    .increment(1);
                anyhow::bail!(
                    "{day}: no `{CORE_TABLE}` partition and no archived copy: nothing captured, \
                     or not replayed yet; not sealed (sealed incomplete only on the last chance)"
                );
            }
        }
        if !(opts.keep_going)() {
            return Ok(stopped(day, "seal", StopReason::Window));
        }

        // Counts file, then manifest, then re-check, then DONE.
        let mut by_table: BTreeMap<String, HashMap<String, u64>> = BTreeMap::new();
        let mut entries = Vec::with_capacity(results.len());
        for t in results {
            by_table.insert(t.entry.table.clone(), t.counts);
            entries.push(t.entry);
        }
        let counts_gz = gzip_bytes(render_counts_csv(&by_table).as_bytes())?;
        let (counts_b64, counts_hex) = sha256_pair(&counts_gz);
        let counts_ref = ObjectRef {
            key: counts_key(day, attempt),
            bytes: counts_gz.len() as u64,
            sha256_hex: counts_hex,
        };
        self.archiver
            .cold
            .put_if_absent_bytes(&counts_ref.key, counts_gz, &counts_b64, &[])
            .await
            .context("write the instrument counts file")?;

        reasons.sort();
        reasons.dedup();
        let rows: u64 = entries.iter().map(|t| t.rows).sum();
        let objects: usize = entries.iter().map(|t| t.partitions.len()).sum();
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            day: day.format("%Y-%m-%d").to_string(),
            attempt,
            export_started_ist: started,
            export_finished_ist: now_ist_text(),
            complete: reasons.is_empty(),
            incomplete_reasons: reasons.clone(),
            tables: entries,
            instrument_counts: counts_ref,
        };
        let body = serde_json::to_vec_pretty(&manifest).context("render the manifest")?;
        let (manifest_b64, manifest_hex) = sha256_pair(&body);
        let mkey = manifest_key(day, attempt);
        self.archiver
            .cold
            .put_if_absent_bytes(&mkey, body, &manifest_b64, &[])
            .await
            .context("write the manifest")?;

        self.recheck_listed(&manifest).await?;

        let done = DoneRecord {
            manifest_key: mkey,
            manifest_sha256_hex: manifest_hex,
            written_at_ist: now_ist_text(),
        };
        let done_body = serde_json::to_vec_pretty(&done).context("render DONE")?;
        let (done_b64, _) = sha256_pair(&done_body);
        let sealed_by_this_run = match self
            .archiver
            .cold
            .put_if_absent_bytes(&done_key(day), done_body, &done_b64, &[])
            .await
        {
            Ok(()) => true,
            // A lost race: another writer sealed the day first; its DONE stands.
            Err(_) if self.is_sealed(day).await.unwrap_or(false) => false,
            Err(err) => return Err(err.context("write DONE")),
        };
        metrics::counter!("tv_brutex_export_runs_total", "outcome" => "done").increment(1);
        if !manifest.complete {
            metrics::counter!("tv_brutex_export_runs_total", "outcome" => "incomplete")
                .increment(1);
        }
        #[allow(clippy::cast_precision_loss)] // APPROVED: seconds and a day number, far below 2^53
        {
            metrics::gauge!("tv_brutex_export_duration_secs").set(clock.elapsed().as_secs_f64());
            metrics::gauge!("tv_brutex_export_last_done_day")
                .set(day.signed_duration_since(NaiveDate::default()).num_days() as f64);
        }
        info!(
            %day, attempt, tables = manifest.tables.len(), rows, objects,
            complete = manifest.complete, reasons = ?manifest.incomplete_reasons,
            secs = clock.elapsed().as_secs(),
            "brutex export: day sealed (manifest written, DONE written last)"
        );
        Ok(ExportOutcome::Sealed(ExportReport {
            attempt,
            tables: manifest.tables.len(),
            rows,
            objects,
            complete: manifest.complete,
            incomplete_reasons: manifest.incomplete_reasons,
            sealed_by_this_run,
        }))
    }

    /// Verified audit rows of export-table partitions of day D (the
    /// partitions an archive pass may already have dropped). An absent
    /// audit table is none.
    async fn archived_rows(&self, day: NaiveDate) -> Result<Vec<ArchivedRow>> {
        let sql = format!(
            "SELECT table_name, partition_name, s3_key, rows_archived, gzip_bytes, gzip_sha256 \
             FROM {PARTITION_ARCHIVE_AUDIT_TABLE} WHERE outcome = 'verified' \
             AND ts >= '{}T00:00:00.000000Z' ORDER BY ts",
            day.format("%Y-%m-%d")
        );
        let response = self
            .archiver
            .ddl_client
            .get(&self.archiver.exec_url)
            .query(&[("query", sql.as_str())])
            .send()
            .await
            .context("archive audit query failed")?;
        let status = response.status();
        let body = read_body_capped(response)
            .await
            .context("archive audit body")?;
        if is_table_absent_response(&body) {
            return Ok(Vec::new());
        }
        anyhow::ensure!(status.is_success(), "archive audit query returned {status}");
        let rows = parse_archived_rows(&body).context("archive audit rows unreadable")?;
        let day_name = day.format("%Y-%m-%d").to_string();
        Ok(rows
            .into_iter()
            .filter(|r| {
                export_order_by(&r.table).is_some()
                    && partitions_of_day([r.partition.as_str()], day).len() == 1
                    && r.partition.starts_with(day_name.as_str())
            })
            .collect())
    }

    /// Exports, verifies and places every partition of `table` for day D.
    /// [`TableStep::Stop`]: the window is closing or the disk is short.
    async fn export_table(
        &mut self,
        table: &'static str,
        day: NaiveDate,
        reasons: &mut Vec<String>,
        opts: ExportOptions<'_>,
    ) -> Result<TableStep> {
        let order_by = export_order_by(table).context("table outside the export set")?;
        let listed = self.archiver.partition_sizes(table).await?;
        let (parts, left_out): (Vec<String>, Vec<String>) =
            partitions_of_day(listed.iter().map(|(n, _)| n.as_str()), day)
                .into_iter()
                .partition(|p| !left_out_as_open(table, p, opts.open_from));
        let open_from_ist = opts
            .open_from
            .filter(|_| ARRIVAL_TIME_TABLES.contains(&table))
            .map(|cut| format!("{}+05:30", cut.format("%Y-%m-%dT%H:%M:%S")));
        if !left_out.is_empty() {
            info!(
                table, %day, left_out = ?left_out, open_from = ?open_from_ist,
                "brutex export: arrival-time hours still taking rows are left out of \
                 this manifest (they reach S3 through the archive and an amend record)"
            );
        }
        let mut result = TableResult {
            entry: ManifestTable {
                table: table.to_string(),
                order_by: order_by.to_string(),
                columns: Vec::new(),
                open_from_ist,
                open_partitions_left_out: left_out,
                rows: 0,
                rows_may_overlap: false,
                partitions: Vec::new(),
            },
            counts: HashMap::new(),
            vanished: Vec::new(),
        };
        if !parts.is_empty() {
            let polls = u32::try_from(self.archiver.cfg.archive_wal_snapshot_wait_secs / 2)
                .unwrap_or(u32::MAX)
                .min(EXPORT_SNAPSHOT_MAX_POLLS);
            match wait_for_snapshot(&mut self.archiver, table, polls).await {
                WalSnapshotVerdict::Reached { .. } | WalSnapshotVerdict::NotReported => {}
                _ => reasons.push(format!("wal_not_applied:{table}")),
            }
        }
        for partition in &parts {
            if !(opts.keep_going)() {
                return Ok(TableStep::Stop(StopReason::Window));
            }
            let expected = listed
                .iter()
                .find(|(n, _)| n == partition)
                .map_or(0, |(_, bytes)| *bytes);
            match self
                .export_partition_quiesced(table, partition, expected, order_by, opts)
                .await?
            {
                PartitionStep::Placed(placed) => {
                    let placed = *placed;
                    for (k, n) in placed.counts {
                        *result.counts.entry(k).or_insert(0) += n;
                    }
                    if result.entry.columns.is_empty() {
                        result.entry.columns = placed.columns;
                    }
                    result.entry.partitions.push(placed.entry);
                }
                PartitionStep::Vanished => result.vanished.push(partition.clone()),
                PartitionStep::Stop(reason) => return Ok(TableStep::Stop(reason)),
            }
        }
        Ok(TableStep::Done(Box::new(result)))
    }

    /// Free and total bytes of the volume holding the export's scratch
    /// folder; `None` when unread. Blocking, run through `off_worker`.
    fn probe_scratch_volume(&self) -> Option<(u64, u64)> {
        crate::off_worker::off_worker(|| {
            // The archiver's rule: an uncreatable folder probes the volume root.
            let probe = if std::fs::create_dir_all(&self.archiver.temp_dir).is_ok() {
                self.archiver.temp_dir.as_path()
            } else {
                Path::new(".")
            };
            match crate::disk_health_watcher::probe_disk_free_bytes(probe) {
                crate::disk_health_watcher::DiskHealthOutcome::Ok {
                    free_bytes,
                    total_bytes,
                } => Some((free_bytes, total_bytes)),
                crate::disk_health_watcher::DiskHealthOutcome::ProbeFailed { .. } => None,
            }
        })
    }

    /// One partition under the SHARED side of the archive quiesce lock: the
    /// hour-path archiver drops only under the exclusive side, so the
    /// partition cannot be dropped between the re-list below and the
    /// placement. A partition the re-list no longer shows was dropped before
    /// the lock: [`PartitionStep::Vanished`].
    ///
    /// Before the lock (2026-11-14 review): it stops ([`StopReason::Window`])
    /// unless [`EXPORT_PARTITION_WORST_SECS`] are left before 08:30, and
    /// ([`StopReason::Disk`]) unless the volume keeps `floor_for_total` (the
    /// after-close floor and the disk-pressure line) plus `expected_bytes`
    /// free, so a filling disk never has the export holding the lock a
    /// pressure archive pass needs (2026-11-14 third review). The lock wait is cut
    /// to the time left; after the lock the window is checked again.
    async fn export_partition_quiesced(
        &mut self,
        table: &str,
        partition: &str,
        expected_bytes: u64,
        order_by: &str,
        opts: ExportOptions<'_>,
    ) -> Result<PartitionStep> {
        let worst = Duration::from_secs(EXPORT_PARTITION_WORST_SECS);
        let left = (opts.time_left)();
        if left.is_some_and(|l| l < worst) {
            return Ok(PartitionStep::Stop(StopReason::Window));
        }
        let probe = self.probe_scratch_volume();
        if !export_space_ok(probe, expected_bytes, opts.floor_for_total) {
            warn!(
                table,
                partition,
                expected_bytes,
                free_bytes = probe.map(|(f, _)| f),
                floor_bytes = probe.map(|(_, t)| (opts.floor_for_total)(t)),
                "brutex export: not enough disk for the next partition above the \
                 after-close floor and the disk-pressure line; stopped (not a failure), \
                 tried again later"
            );
            return Ok(PartitionStep::Stop(StopReason::Disk));
        }
        let wait = left.map_or(self.quiesce_wait, |l| {
            self.quiesce_wait.min(l.saturating_sub(worst))
        });
        let quiesce: &'static ArchiveQuiesce = self.quiesce;
        let Some(guard) = quiesce.shared(wait).await else {
            if wait < self.quiesce_wait {
                // The wait was cut by the clock: the window is closing.
                return Ok(PartitionStep::Stop(StopReason::Window));
            }
            anyhow::bail!(
                "{table}/{partition}: the archive quiesce lock stayed held for {} s",
                wait.as_secs()
            );
        };
        if !(opts.keep_going)() || (opts.time_left)().is_some_and(|l| l < worst) {
            drop(guard);
            return Ok(PartitionStep::Stop(StopReason::Window));
        }
        let still = self.archiver.partition_sizes(table).await?;
        if !still.iter().any(|(n, _)| n == partition) {
            drop(guard);
            return Ok(PartitionStep::Vanished);
        }
        let placed = self.export_partition(table, partition, order_by).await;
        drop(guard);
        match placed {
            Ok(placed) => Ok(PartitionStep::Placed(Box::new(placed))),
            // The exact-gate archiver (`process_one`) drops WITHOUT the
            // quiesce lock (2026-11-14 third review): a partition it dropped
            // mid-export makes the recount miss and the retry export 0 rows.
            // Gone now = dropped mid-export, never a failed attempt; its
            // verified archive copy must then cover it (`merge_archived`).
            Err(err) => match self.archiver.partition_sizes(table).await {
                Ok(now) if !now.iter().any(|(n, _)| n == partition) => {
                    info!(
                        table, partition, error = %format!("{err:#}"),
                        "brutex export: the partition was dropped by the archive during \
                         its export; listed from its archive copy instead"
                    );
                    Ok(PartitionStep::Vanished)
                }
                _ => Err(err),
            },
        }
    }

    /// One partition: free-space check, ordered export, recount (one retry
    /// when rows moved), instrument tally, placement, length check.
    async fn export_partition(
        &mut self,
        table: &str,
        partition: &str,
        order_by: &str,
    ) -> Result<PlacedPartition> {
        let (start, end) =
            partition_range_bounds(partition).context("unparseable partition name")?;
        // The archiver's own floor too (the after-close floor was checked
        // before the lock; this re-reads the volume after the wait).
        let free = self.probe_scratch_volume().map(|(f, _)| f);
        anyhow::ensure!(
            archive_space_verdict(free, ARCHIVE_MIN_FREE_BYTES) != ArchiveSpaceVerdict::Wedged,
            "temp_space: less than the archive floor free for {table}/{partition}"
        );
        let sql = build_export_sql_ordered(table, &start, &end, Some(order_by));
        let path = self
            .archiver
            .temp_dir
            .join(format!("{table}-{partition}.csv.gz"));
        let mut exported: Option<ExportedCsv> = None;
        for _ in 0..2 {
            let e = self.archiver.export_query_csv(&sql, path.clone()).await?;
            let recount = self.archiver.recount_rows(table, &start, &end).await;
            if recount.as_ref().is_some_and(|r| r.rows == e.rows) {
                exported = Some(e);
                break;
            }
            remove_temp_file(&e.path);
        }
        let exported = exported.with_context(|| {
            format!("{table}/{partition}: rows moved during the export twice (recount mismatch)")
        })?;
        // A listed partition holds rows; an empty export of one means it went
        // away under us (never sealed as an empty hour).
        if exported.rows == 0 {
            remove_temp_file(&exported.path);
            anyhow::bail!("{table}/{partition}: a listed partition exported 0 rows");
        }
        let tally_path = exported.path.clone();
        let tally = crate::off_worker::off_worker(|| tally_gzip_file(&tally_path));
        let tally = match tally {
            Ok(t) if t.rows() == exported.rows => t,
            Ok(t) => {
                remove_temp_file(&exported.path);
                anyhow::bail!(
                    "{table}/{partition}: instrument tally {} != exported rows {}",
                    t.rows(),
                    exported.rows
                );
            }
            Err(err) => {
                remove_temp_file(&exported.path);
                return Err(err);
            }
        };
        let placed = self.place(table, partition, &exported).await;
        remove_temp_file(&exported.path);
        let (key, placement) = placed?;
        let meta = self.archiver.cold.head(&key).await?;
        anyhow::ensure!(
            meta.is_some_and(|m| m.len == exported.gzip_bytes),
            "{table}/{partition}: S3 length of {key} != local gzip"
        );
        note_object_placed();
        metrics::counter!("tv_brutex_export_objects_total", "placement" => placement.as_str())
            .increment(1);
        let counts = tally.counts().clone();
        Ok(PlacedPartition {
            entry: ManifestPartition {
                partition: partition.to_string(),
                key,
                placement,
                rows: exported.rows,
                gzip_bytes: exported.gzip_bytes,
                sha256_hex: exported.gzip_sha256_hex.clone(),
                csv_bytes: Some(exported.csv_bytes),
                instrument_counts_known: true,
                ordered_by_dedup_key: true,
            },
            counts,
            columns: tally.header().to_vec(),
        })
    }

    /// Places the exported object with the archiver's rules: identical at the
    /// canonical key → reuse; absent → create-only upload; different → the
    /// content-addressed key (identical → reuse, absent → upload, different →
    /// refuse). A lost create race is re-read, never retried as an overwrite.
    async fn place(
        &self,
        table: &str,
        partition: &str,
        e: &ExportedCsv,
    ) -> Result<(String, Placement)> {
        let canonical = canonical_archive_key(table, partition);
        let cold = &self.archiver.cold;
        let existing = classify_existing(
            cold.head(&canonical).await?.as_ref(),
            e.gzip_bytes,
            &e.gzip_sha256_b64,
        );
        match canonical_step(existing) {
            CanonicalStep::Reuse => return Ok((canonical, Placement::Reused)),
            CanonicalStep::Upload => {
                match cold
                    .put_if_absent_path(&e.path, &canonical, &e.gzip_sha256_b64)
                    .await
                {
                    Ok(()) => return Ok((canonical, Placement::Uploaded)),
                    Err(err) => {
                        let now = classify_existing(
                            cold.head(&canonical).await?.as_ref(),
                            e.gzip_bytes,
                            &e.gzip_sha256_b64,
                        );
                        match now {
                            ExistingObject::Identical => return Ok((canonical, Placement::Reused)),
                            ExistingObject::Absent => return Err(err),
                            ExistingObject::Different => {}
                        }
                    }
                }
            }
            CanonicalStep::GoToSidecar => {}
        }
        let sidecar = sidecar_archive_key(table, partition, &e.gzip_sha256_hex);
        match classify_existing(
            cold.head(&sidecar).await?.as_ref(),
            e.gzip_bytes,
            &e.gzip_sha256_b64,
        ) {
            ExistingObject::Identical => Ok((sidecar, Placement::SidecarReused)),
            ExistingObject::Different => anyhow::bail!(
                "{sidecar} holds different bytes under its own digest; neither key overwritten"
            ),
            ExistingObject::Absent => {
                cold.put_if_absent_path(&e.path, &sidecar, &e.gzip_sha256_b64)
                    .await?;
                warn!(
                    code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                    table, partition, key = %sidecar,
                    "brutex export: the canonical archive key holds other bytes; this \
                     day's copy went to the content-addressed key (nothing overwritten)"
                );
                Ok((sidecar, Placement::Sidecar))
            }
        }
    }

    /// Every key the manifest lists is still in S3 with its length (and its
    /// SHA-256 where S3 stored one). Covers the archiver deleting an object
    /// it uploaded itself on its pre-drop mismatch arm.
    async fn recheck_listed(&self, manifest: &Manifest) -> Result<()> {
        for t in &manifest.tables {
            for p in &t.partitions {
                let meta = self.archiver.cold.head(&p.key).await?;
                let ok = meta.as_ref().is_some_and(|m| {
                    m.len == p.gzip_bytes
                        && m.checksum_sha256_b64
                            .as_deref()
                            .is_none_or(|b64| Some(b64.to_string()) == hex_to_base64(&p.sha256_hex))
                });
                anyhow::ensure!(ok, "{} changed or vanished before DONE", p.key);
            }
        }
        let c = &manifest.instrument_counts;
        let meta = self.archiver.cold.head(&c.key).await?;
        anyhow::ensure!(
            meta.is_some_and(|m| m.len == c.bytes),
            "{} vanished before DONE",
            c.key
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 13).expect("date")
    }

    #[test]
    fn test_export_order_by_equals_dedup_key_for_every_export_table() {
        assert_eq!(
            export_order_by("ticks"),
            Some(crate::tick_persistence::DEDUP_KEY_TICKS)
        );
        assert_eq!(
            export_order_by("tick_quotes"),
            Some(crate::tick_quotes_persistence::DEDUP_KEY_TICK_QUOTES)
        );
        assert_eq!(
            export_order_by("feed_aux_packets"),
            Some(crate::feed_aux_persistence::DEDUP_KEY_FEED_AUX_PACKETS)
        );
        assert_eq!(
            export_order_by("market_depth"),
            Some(crate::depth_persistence::DEDUP_KEY_MARKET_DEPTH)
        );
        assert_eq!(
            export_order_by("market_depth_book"),
            Some(crate::depth_persistence::DEDUP_KEY_MARKET_DEPTH_BOOK)
        );
        for t in crate::shadow_persistence::emitted_candle_table_names() {
            assert_eq!(
                export_order_by(t),
                Some(crate::shadow_persistence::DEDUP_KEY_CANDLES)
            );
        }
        for t in export_tables() {
            let key = export_order_by(t).expect("every export table has an order");
            assert!(
                key.contains("segment") && key.contains("feed"),
                "{t}: {key}"
            );
        }
    }

    #[test]
    fn test_build_export_sql_orders_by_dedup_key_when_enabled() {
        let sql = build_export_sql_ordered("ticks", "a", "b", export_order_by("ticks"));
        assert!(sql.ends_with(&format!(
            "ORDER BY {}",
            crate::tick_persistence::DEDUP_KEY_TICKS
        )));
        assert!(sql.starts_with(&super::super::build_export_sql("ticks", "a", "b")));
    }

    #[test]
    fn test_build_export_sql_unchanged_for_tables_outside_the_export_set() {
        for t in ["order_audit", "partition_archive_audit", "rest_spot_1m"] {
            assert_eq!(export_order_by(t), None);
            assert_eq!(
                build_export_sql_ordered(t, "a", "b", export_order_by(t)),
                super::super::build_export_sql(t, "a", "b")
            );
        }
    }

    #[test]
    fn test_gzip_of_same_csv_is_byte_identical_across_chunk_splits() {
        let csv: String = (0..20_000)
            .map(|i| {
                format!(
                    "2026-11-13T09:{:02}:00.000000Z,{i},NSE_FNO,\"a,\"\"b\",dhan\n",
                    i % 60
                )
            })
            .collect();
        let whole = gzip_bytes(csv.as_bytes()).expect("gzip");
        for split in [1usize, 7, 8191, 65_536] {
            let mut enc = GzEncoder::new(Vec::new(), Compression::default());
            for chunk in csv.as_bytes().chunks(split) {
                enc.write_all(chunk).expect("write");
            }
            let out = enc.finish().expect("finish");
            assert_eq!(sha256_pair(&out), sha256_pair(&whole), "split {split}");
        }
    }

    #[test]
    fn test_gzip_header_has_zero_mtime_and_unknown_os() {
        let out = gzip_bytes(b"x\n").expect("gzip");
        assert_eq!(&out[..10], &[0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff]);
    }

    #[test]
    fn test_partitions_of_day_takes_the_day_and_its_hours_only() {
        let names = [
            "2026-11-13T09",
            "2026-11-13",
            "2026-11-12T15",
            "2026-11-13T15",
            "2026-11-130",
            "2026-11-14",
            "2026-11-13X09",
        ];
        assert_eq!(
            partitions_of_day(names, day()),
            vec!["2026-11-13", "2026-11-13T09", "2026-11-13T15"]
        );
    }

    #[test]
    fn test_key_naming_is_under_the_archive_prefix() {
        assert_eq!(done_key(day()), "questdb-partitions/_daily/2026-11-13/DONE");
        assert_eq!(
            manifest_key(day(), 3),
            "questdb-partitions/_daily/2026-11-13/manifest-3.json"
        );
        assert_eq!(
            counts_key(day(), 1),
            "questdb-partitions/_daily/2026-11-13/instrument-counts-1.csv.gz"
        );
        assert_eq!(
            amend_key(day(), "ticks", "2026-11-13T09", "0123456789abcdef"),
            "questdb-partitions/_daily/2026-11-13/amend-ticks-2026-11-13T09-0123456789ab.json"
        );
        // Never under a table's own folder, so it can never be taken for a partition.
        assert!(!done_key(day()).starts_with(&canonical_archive_key("ticks", "")));
    }

    #[test]
    fn test_next_attempt_number_from_listing() {
        assert_eq!(next_attempt(&[], day()), 1);
        let keys = vec![
            (manifest_key(day(), 1), 10),
            (manifest_key(day(), 4), 10),
            (counts_key(day(), 9), 10),
            (
                "questdb-partitions/_daily/2026-11-13/manifest-x.json".to_string(),
                1,
            ),
            (manifest_key(day().pred_opt().expect("d"), 7), 10),
        ];
        assert_eq!(
            next_attempt(&keys, day()),
            10,
            "a counts file burns its attempt too"
        );
        // Regression (2026-11-14 review): a counts file written before a failed
        // manifest PUT must not be reused by the next attempt.
        let orphan = vec![(counts_key(day(), 1), 10)];
        assert_eq!(next_attempt(&orphan, day()), 2);
    }

    #[test]
    fn test_canonical_step_never_overwrites() {
        assert_eq!(
            canonical_step(ExistingObject::Identical),
            CanonicalStep::Reuse
        );
        assert_eq!(
            canonical_step(ExistingObject::Absent),
            CanonicalStep::Upload
        );
        assert_eq!(
            canonical_step(ExistingObject::Different),
            CanonicalStep::GoToSidecar
        );
    }

    #[test]
    fn test_tally_keys_on_security_id_segment_feed() {
        let mut t = InstrumentTally::default();
        t.update(b"\"ts\",\"security_id\",\"segment\",\"ltp\",\"feed\"\n");
        t.update(
            b"1,13,\"IDX_I\",1.5,\"dhan\"\n2,13,\"NSE_EQ\",2,\"dhan\"\n3,13,\"IDX_I\",3,\"dhan\"",
        );
        t.finish();
        assert_eq!(t.rows(), 3);
        let sep = KEY_SEP;
        assert_eq!(t.counts().get(&format!("13{sep}IDX_I{sep}dhan")), Some(&2));
        assert_eq!(t.counts().get(&format!("13{sep}NSE_EQ{sep}dhan")), Some(&1));
        let mut by = BTreeMap::new();
        by.insert("ticks".to_string(), t.counts().clone());
        assert_eq!(
            render_counts_csv(&by),
            "table,security_id,segment,feed,rows\nticks,13,IDX_I,dhan,2\nticks,13,NSE_EQ,dhan,1\n"
        );
    }

    proptest::proptest! {
        #[test]
        fn proptest_instrument_tally_sum_equals_record_count(
            rows in proptest::collection::vec(
                (0u32..50, 0usize..3, "[a-z,\"\n ]{0,8}"), 0..60),
            split in 1usize..40,
        ) {
            let segs = ["IDX_I", "NSE_EQ", "NSE_FNO"];
            let mut csv = String::from("ts,security_id,segment,note,feed\n");
            for (i, (sid, seg, note)) in rows.iter().enumerate() {
                csv.push_str(&format!(
                    "{i},{sid},{},{},dhan\n", segs[*seg], csv_field(note)
                ));
            }
            let mut t = InstrumentTally::default();
            for chunk in csv.as_bytes().chunks(split) {
                t.update(chunk);
            }
            t.finish();
            let mut counter = super::super::CsvRecordCounter::default();
            counter.update(csv.as_bytes());
            proptest::prop_assert_eq!(t.rows(), rows.len() as u64);
            proptest::prop_assert_eq!(t.rows(), counter.records() - 1);
            proptest::prop_assert_eq!(t.counts().values().sum::<u64>(), rows.len() as u64);
        }
    }

    #[test]
    fn test_manifest_json_golden() {
        let m = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            day: "2026-11-13".into(),
            attempt: 2,
            export_started_ist: "s".into(),
            export_finished_ist: "f".into(),
            complete: false,
            incomplete_reasons: vec!["spill_pending:ticks".into()],
            tables: vec![ManifestTable {
                table: "ticks".into(),
                order_by: "ts".into(),
                columns: vec!["ts".into(), "feed".into()],
                open_from_ist: Some("2026-11-13T16:00:00+05:30".into()),
                open_partitions_left_out: vec!["2026-11-13T16".into()],
                rows: 3,
                rows_may_overlap: false,
                partitions: vec![ManifestPartition {
                    partition: "2026-11-13T09".into(),
                    key: "k".into(),
                    placement: Placement::SidecarReused,
                    rows: 3,
                    gzip_bytes: 9,
                    sha256_hex: "ab".into(),
                    csv_bytes: None,
                    instrument_counts_known: false,
                    ordered_by_dedup_key: true,
                }],
            }],
            instrument_counts: ObjectRef {
                key: "c".into(),
                bytes: 1,
                sha256_hex: "cd".into(),
            },
        };
        let json = serde_json::to_string(&m).expect("json");
        assert_eq!(
            json,
            "{\"schema_version\":1,\"day\":\"2026-11-13\",\"attempt\":2,\
             \"export_started_ist\":\"s\",\"export_finished_ist\":\"f\",\"complete\":false,\
             \"incomplete_reasons\":[\"spill_pending:ticks\"],\"tables\":[{\"table\":\"ticks\",\
             \"order_by\":\"ts\",\"columns\":[\"ts\",\"feed\"],\
             \"open_from_ist\":\"2026-11-13T16:00:00+05:30\",\
             \"open_partitions_left_out\":[\"2026-11-13T16\"],\"rows\":3,\"rows_may_overlap\":false,\"partitions\":[{\"partition\":\"2026-11-13T09\",\
             \"key\":\"k\",\"placement\":\"sidecar_reused\",\"rows\":3,\"gzip_bytes\":9,\
             \"sha256_hex\":\"ab\",\"csv_bytes\":null,\"instrument_counts_known\":false,\"ordered_by_dedup_key\":true}]}],\
             \"instrument_counts\":{\"key\":\"c\",\"bytes\":1,\"sha256_hex\":\"cd\"}}"
        );
    }

    #[test]
    fn test_done_names_manifest_and_its_sha() {
        let body = b"{\"schema_version\":1}";
        let (_, hex) = sha256_pair(body);
        let done = DoneRecord {
            manifest_key: manifest_key(day(), 2),
            manifest_sha256_hex: hex.clone(),
            written_at_ist: "t".into(),
        };
        let json = serde_json::to_string(&done).expect("json");
        assert_eq!(
            json,
            format!(
                "{{\"manifest_key\":\"questdb-partitions/_daily/2026-11-13/manifest-2.json\",\
                 \"manifest_sha256_hex\":\"{hex}\",\"written_at_ist\":\"t\"}}"
            )
        );
        assert_eq!(hex.len(), 64);
    }

    /// 2026-11-14 review: every distinct archived copy of a partition is
    /// kept (an archived bulk and a later copy of rows that re-created the
    /// partition are both data); a repeated row of the same copy keeps the
    /// last.
    #[test]
    fn test_parse_archived_rows_keeps_every_copy_of_a_partition() {
        let body = "{\"dataset\":[[\"market_depth\",\"2026-11-13T09\",\"k1\",5,10,\"aa\"],\
                    [\"market_depth\",\"2026-11-13T09\",\"k2\",6,11,\"bb\"],\
                    [\"market_depth\",\"2026-11-13T09\",\"k2\",7,11,\"bb\"],\
                    [\"ticks\",\"2026-11-13T10\",\"k3\",7,12,\"cc\"]]}";
        let rows = parse_archived_rows(body).expect("rows");
        assert_eq!(rows.len(), 3, "{rows:?}");
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["k1", "k2", "k3"]);
        assert_eq!(rows[1].rows, 7, "the last row of one copy wins");
        assert!(parse_archived_rows("{\"dataset\":[[1]]}").is_none());
    }

    fn entry(partition: &str, key: &str, rows: u64) -> ManifestPartition {
        ManifestPartition {
            partition: partition.into(),
            key: key.into(),
            placement: Placement::Uploaded,
            rows,
            gzip_bytes: 1,
            sha256_hex: "aa".into(),
            csv_bytes: Some(1),
            instrument_counts_known: true,
            ordered_by_dedup_key: true,
        }
    }

    fn archived(partition: &str, key: &str, rows: u64) -> ArchivedRow {
        ArchivedRow {
            table: "ticks".into(),
            partition: partition.into(),
            key: key.into(),
            rows,
            gzip_bytes: 9,
            sha256_hex: "bb".into(),
        }
    }

    fn table_result(placed: Vec<ManifestPartition>, vanished: Vec<&str>) -> TableResult {
        TableResult {
            entry: ManifestTable {
                table: "ticks".into(),
                order_by: "k".into(),
                columns: Vec::new(),
                open_from_ist: None,
                open_partitions_left_out: Vec::new(),
                rows: 0,
                rows_may_overlap: false,
                partitions: placed,
            },
            counts: HashMap::new(),
            vanished: vanished.into_iter().map(ToString::to_string).collect(),
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt")
            .block_on(f)
    }

    /// Regression (2026-11-14 review, HIGH): an hour archived and dropped,
    /// then re-created by late rows, lists BOTH objects; the export's own
    /// object is not listed twice.
    #[test]
    fn test_merge_lists_the_archived_bulk_beside_the_re_created_rows() {
        let mut t = table_result(vec![entry("2026-11-13T10", "side", 40)], vec![]);
        let archived = vec![
            archived("2026-11-13T10", "canonical", 2_000_000),
            archived("2026-11-13T10", "side", 40),
        ];
        let mut reasons = Vec::new();
        block_on(merge_archived(&mut t, &archived, &mut reasons, |_| async {
            Ok(true)
        }))
        .expect("merge");
        let keys: Vec<&str> = t.entry.partitions.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["canonical", "side"]);
        // 2026-11-14 third review: an archiver copy is not known to be sorted.
        let ordered: Vec<bool> = t
            .entry
            .partitions
            .iter()
            .map(|p| p.ordered_by_dedup_key)
            .collect();
        assert_eq!(ordered, [false, true]);
        assert_eq!(t.entry.rows, 2_000_040);
        assert!(t.entry.rows_may_overlap);
        assert!(reasons.is_empty());
    }

    /// Regression (2026-11-14 review, CRITICAL): a partition dropped before
    /// the export could lock it is listed from its audit row; with no intact
    /// copy the day is refused, never sealed without the hour.
    #[test]
    fn test_merge_requires_a_copy_for_every_vanished_partition() {
        let mut t = table_result(vec![], vec!["2026-11-13T10"]);
        let mut reasons = Vec::new();
        let err = block_on(merge_archived(&mut t, &[], &mut reasons, |_| async {
            Ok(true)
        }))
        .expect_err("no copy yet");
        assert!(format!("{err:#}").contains("2026-11-13T10"), "{err:#}");
        let mut t = table_result(vec![], vec!["2026-11-13T10"]);
        let copy = vec![archived("2026-11-13T10", "canonical", 5)];
        let err = block_on(merge_archived(&mut t, &copy, &mut reasons, |_| async {
            Ok(false)
        }))
        .expect_err("copy not intact");
        assert!(format!("{err:#}").contains("no intact"), "{err:#}");
        assert_eq!(reasons, ["archived_copy_unconfirmed:ticks/2026-11-13T10"]);
        let mut t = table_result(vec![], vec!["2026-11-13T10"]);
        block_on(merge_archived(&mut t, &copy, &mut Vec::new(), |_| async {
            Ok(true)
        }))
        .expect("covered");
        assert_eq!(t.entry.rows, 5);
        assert!(!t.entry.rows_may_overlap);
    }

    /// Records every key registered, scoped with `metrics::with_local_recorder`.
    #[derive(Default)]
    struct KeyRecorder {
        keys: std::sync::Mutex<Vec<String>>,
    }

    impl metrics::Recorder for KeyRecorder {
        fn describe_counter(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn register_counter(
            &self,
            key: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            let labels: Vec<String> = key
                .labels()
                .map(|l| format!("{}={}", l.key(), l.value()))
                .collect();
            if let Ok(mut keys) = self.keys.lock() {
                keys.push(format!("{}{{{}}}", key.name(), labels.join(",")));
            }
            metrics::Counter::noop()
        }
        fn register_gauge(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
            if let Ok(mut keys) = self.keys.lock() {
                keys.push(key.name().to_string());
            }
            metrics::Gauge::noop()
        }
        fn register_histogram(
            &self,
            _: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    #[test]
    fn test_brutex_export_metrics_registered_at_zero() {
        let recorder = KeyRecorder::default();
        metrics::with_local_recorder(&recorder, register_brutex_export_metrics);
        let keys = recorder.keys.lock().map(|k| k.clone()).unwrap_or_default();
        for want in [
            "tv_brutex_export_runs_total{outcome=done}",
            "tv_brutex_export_runs_total{outcome=stopped}",
            "tv_brutex_export_runs_total{outcome=gave_up}",
            "tv_brutex_export_runs_total{outcome=incomplete}",
            "tv_brutex_export_objects_total{placement=uploaded}",
            "tv_brutex_export_objects_total{placement=archived_before_export}",
            "tv_partition_archive_export_unordered_fallback_total{}",
            "tv_partition_archive_export_unordered_in_session_total{}",
            "tv_partition_archive_amend_records_total{}",
            "tv_brutex_export_duration_secs",
            "tv_brutex_export_last_done_day",
        ] {
            assert!(keys.iter().any(|k| k == want), "{want} missing: {keys:?}");
        }
        let before = objects_placed_total();
        note_object_placed();
        assert!(objects_placed_total() > before);
    }

    proptest::proptest! {
        #[test]
        fn proptest_export_never_runs_inside_the_session_window_on_a_session_day(
            secs in 0u32..86_400,
            end in 0u32..86_400,
            trading in proptest::bool::ANY,
            muhurat in proptest::bool::ANY,
        ) {
            let info = SessionDayInfo { trading, muhurat_end_secs: muhurat.then_some(end) };
            let open = export_window_open(info, secs);
            if trading && (9 * 3600..EXPORT_OPEN_SECS_IST).contains(&secs) {
                proptest::prop_assert!(!open, "trading day {secs}");
            }
            if trading && (EXPORT_FORBIDDEN_FROM_SECS_IST..EXPORT_OPEN_SECS_IST).contains(&secs) {
                proptest::prop_assert!(!open);
            }
            if muhurat && secs >= EXPORT_FORBIDDEN_FROM_SECS_IST && secs < end + MUHURAT_END_GRACE_SECS {
                proptest::prop_assert!(!open, "muhurat {secs} end {end}");
            }
            if !trading && !muhurat {
                proptest::prop_assert!(open);
            }
        }
    }

    #[test]
    fn test_export_open_secs_per_kind_of_day() {
        let trading = SessionDayInfo {
            trading: true,
            muhurat_end_secs: None,
        };
        let closed = SessionDayInfo {
            trading: false,
            muhurat_end_secs: None,
        };
        let muhurat = SessionDayInfo {
            trading: false,
            muhurat_end_secs: Some(19 * 3600 + 15 * 60),
        };
        assert_eq!(export_open_secs(trading), Some(EXPORT_OPEN_SECS_IST));
        assert_eq!(export_open_secs(closed), None);
        assert_eq!(export_open_secs(muhurat), Some(19 * 3600 + 15 * 60 + 360));
        let end = 19 * 3600 + 15 * 60;
        assert!(!export_window_open(muhurat, end + 359));
        assert!(export_window_open(muhurat, end + 360));
        assert!(
            export_window_open(muhurat, 3600),
            "the night before 08:30 is free"
        );
        let both = SessionDayInfo {
            trading: true,
            muhurat_end_secs: Some(10 * 3600),
        };
        assert_eq!(export_open_secs(both), Some(EXPORT_OPEN_SECS_IST));
        let sat = NaiveDate::from_ymd_opt(2031, 1, 4).expect("date");
        assert_eq!(
            session_info(sat),
            tickvault_common::market_hours::installed_calendar_session_day(sat).unwrap_or(closed)
        );
        let (_, secs) = ist_now();
        assert!(secs < 86_400);
    }

    /// Regression (2026-11-14 review, HIGH): the archiver's no-sort window
    /// ends at the session's INGEST end (15:40), not at the export's 15:45
    /// opening, so the daily archive leg (open at 15:40:02) sorts against
    /// the export's object and reuses it instead of writing a second copy.
    #[test]
    fn test_archiver_sort_forbidden_until_archiver_sort_end_secs() {
        let trading = SessionDayInfo {
            trading: true,
            muhurat_end_secs: None,
        };
        let end = tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST;
        assert_eq!(archiver_sort_end_secs(trading), Some(end));
        assert!(end < EXPORT_OPEN_SECS_IST);
        assert!(!archiver_sort_forbidden(
            trading,
            EXPORT_FORBIDDEN_FROM_SECS_IST - 1
        ));
        assert!(archiver_sort_forbidden(
            trading,
            EXPORT_FORBIDDEN_FROM_SECS_IST
        ));
        assert!(archiver_sort_forbidden(trading, end - 1));
        assert!(!archiver_sort_forbidden(trading, end), "15:40:00 sorts");
        assert!(
            !archiver_sort_forbidden(trading, end + 2),
            "the 15:40:02 leg sorts"
        );
        let muhurat = SessionDayInfo {
            trading: false,
            muhurat_end_secs: Some(19 * 3600 + 15 * 60),
        };
        assert!(archiver_sort_forbidden(muhurat, 19 * 3600));
        assert!(!archiver_sort_forbidden(muhurat, 19 * 3600 + 15 * 60));
        let closed = SessionDayInfo {
            trading: false,
            muhurat_end_secs: None,
        };
        assert_eq!(archiver_sort_end_secs(closed), None);
        assert!(!archiver_sort_forbidden(closed, 11 * 3600));
    }

    /// Regression (2026-11-14 review, MEDIUM): the archiver's in-session
    /// guard is pinned on a real date, so dropping its sense (sorting in
    /// session) fails a test. Finds a date of the coming weeks the calendar
    /// (or the Monday-to-Friday fallback) calls a trading day.
    #[test]
    fn test_in_trading_day_window_at_bites_on_a_session_day() {
        let start = NaiveDate::from_ymd_opt(2031, 3, 3).expect("date");
        let trading_day = (0..28)
            .filter_map(|d| start.checked_add_days(chrono::Days::new(d)))
            .find(|d| session_info(*d).trading && session_info(*d).muhurat_end_secs.is_none())
            .expect("a trading day in four weeks");
        assert!(
            in_trading_day_window_at(trading_day, 11 * 3600),
            "no sort at 11:00"
        );
        assert!(
            !in_trading_day_window_at(trading_day, 3 * 3600),
            "sorts at 03:00"
        );
        assert!(
            !in_trading_day_window_at(trading_day, 16 * 3600),
            "sorts at 16:00"
        );
        let closed_day = (0..28)
            .filter_map(|d| start.checked_add_days(chrono::Days::new(d)))
            .find(|d| export_open_secs(session_info(*d)).is_none())
            .expect("a closed day in four weeks");
        assert!(!in_trading_day_window_at(closed_day, 11 * 3600));
    }

    /// Regression (2026-11-14 review, MEDIUM): a run before 08:30 on a
    /// session day knows how long it has; nothing bounds a closed day or a
    /// time after the session's window opened.
    #[test]
    fn test_secs_until_forbidden_bounds_a_run_before_0830() {
        let trading = SessionDayInfo {
            trading: true,
            muhurat_end_secs: None,
        };
        assert_eq!(secs_until_forbidden(trading, 8 * 3600), Some(1_800));
        assert_eq!(
            secs_until_forbidden(trading, 8 * 3600 + 29 * 60 + 50),
            Some(10)
        );
        assert_eq!(
            secs_until_forbidden(trading, EXPORT_FORBIDDEN_FROM_SECS_IST),
            None
        );
        assert_eq!(secs_until_forbidden(trading, 17 * 3600), None);
        let closed = SessionDayInfo {
            trading: false,
            muhurat_end_secs: None,
        };
        assert_eq!(secs_until_forbidden(closed, 8 * 3600), None);
        assert_eq!(EXPORT_PARTITION_WORST_SECS, 1_800);
        assert!(
            secs_until_forbidden(trading, 8 * 3600)
                .is_some_and(|s| u64::from(s) <= EXPORT_PARTITION_WORST_SECS)
        );
    }

    /// Regression (2026-11-14 review, HIGH): the export needs the
    /// after-close floor plus the partition's size free; an unread volume is
    /// refused.
    #[test]
    fn test_export_space_ok_needs_floor_plus_partition() {
        let floor = |total: u64| total / 10;
        assert!(export_space_ok(Some((15, 100)), 5, &floor));
        assert!(!export_space_ok(Some((14, 100)), 5, &floor));
        assert!(!export_space_ok(None, 0, &floor));
        assert!(export_space_ok(Some((10, 100)), 0, &floor));
        assert!(!export_space_ok(
            Some((u64::MAX - 1, 100)),
            u64::MAX,
            &floor
        ));
    }

    /// Regression (2026-11-14 third review, MEDIUM): the export's disk floor
    /// includes the disk-pressure line, so it never holds the quiesce lock
    /// while the pressure archiver needs the exclusive side.
    #[test]
    fn test_pressure_free_floor_bytes_is_the_room_below_high_water() {
        let gib = 1_u64 << 30;
        assert_eq!(pressure_free_floor_bytes(100 * gib, 75), 25 * gib);
        assert_eq!(pressure_free_floor_bytes(100, 100), 0);
        assert_eq!(pressure_free_floor_bytes(100, 200), 0);
        assert_eq!(pressure_free_floor_bytes(u64::MAX, 0), u64::MAX);
        // 30 GB free on a 150 GB volume is inside pressure: no partition.
        let floor = |total: u64| pressure_free_floor_bytes(total, 75).max(16 * gib);
        assert!(!export_space_ok(Some((30 * gib, 150 * gib)), 1, &floor));
        assert!(export_space_ok(Some((40 * gib, 150 * gib)), gib, &floor));
    }

    /// Regression (2026-11-14 third review, MEDIUM): a failed HEAD is not
    /// "no object" for the archiver's order choice.
    #[test]
    fn test_canonical_maybe_exists_reads_a_failed_head_as_maybe() {
        assert!(!canonical_maybe_exists::<u8>(&Ok(None)));
        assert!(canonical_maybe_exists(&Ok(Some(1_u8))));
        assert!(canonical_maybe_exists::<u8>(&Err(anyhow::anyhow!("503"))));
    }

    /// Regression (2026-11-14 third review, MEDIUM x2): the session day's
    /// arrival-time hours that may still take rows are cut at the open
    /// hour; once the clock passed the day nothing is cut.
    #[test]
    fn test_arrival_open_from_and_left_out_as_open_cut_at_the_open_hour() {
        let d = day();
        let at = |h: u32, m: u32| d.and_hms_opt(h, m, 0).expect("t");
        assert_eq!(
            arrival_open_from(d, d, 16 * 3600 + 20 * 60),
            Some(at(16, 0))
        );
        // Inside the settle slack the hour that just ended is still open.
        assert_eq!(arrival_open_from(d, d, 16 * 3600 + 60), Some(at(15, 0)));
        assert_eq!(
            arrival_open_from(d, d, 16 * 3600 + ARRIVAL_SETTLE_SECS),
            Some(at(16, 0))
        );
        assert_eq!(arrival_open_from(d, d, 30), Some(at(0, 0)));
        assert_eq!(arrival_open_from(d, d, 86_399), Some(at(23, 0)));
        let next = d.succ_opt().expect("d");
        assert_eq!(arrival_open_from(d, next, 3_600), None);
        assert_eq!(arrival_open_from(next, d, 3_600), next.and_hms_opt(0, 0, 0));
        let cut = arrival_open_from(d, d, 16 * 3600 + 20 * 60);
        assert!(left_out_as_open("tick_quotes", "2026-11-13T16", cut));
        assert!(left_out_as_open("feed_aux_packets", "2026-11-13T17", cut));
        assert!(!left_out_as_open("tick_quotes", "2026-11-13T15", cut));
        assert!(!left_out_as_open("ticks", "2026-11-13T16", cut));
        assert!(!left_out_as_open("market_depth_book", "2026-11-13T16", cut));
        assert!(!left_out_as_open("candles_1m", "2026-11-13", cut));
        assert!(!left_out_as_open("tick_quotes", "2026-11-13T16", None));
    }

    /// The scheduler's close gate: the end of the hour holding the last
    /// session second, plus the settle slack.
    #[test]
    fn test_arrival_close_secs_is_the_close_hour_end() {
        let trading = SessionDayInfo {
            trading: true,
            muhurat_end_secs: None,
        };
        assert_eq!(
            arrival_close_secs(trading),
            Some(16 * 3600 + ARRIVAL_SETTLE_SECS)
        );
        let muhurat = SessionDayInfo {
            trading: false,
            muhurat_end_secs: Some(19 * 3600 + 15 * 60),
        };
        assert_eq!(
            arrival_close_secs(muhurat),
            Some(20 * 3600 + ARRIVAL_SETTLE_SECS)
        );
        let on_the_hour = SessionDayInfo {
            trading: false,
            muhurat_end_secs: Some(19 * 3600),
        };
        assert_eq!(
            arrival_close_secs(on_the_hour),
            Some(19 * 3600 + ARRIVAL_SETTLE_SECS)
        );
        let closed = SessionDayInfo {
            trading: false,
            muhurat_end_secs: None,
        };
        assert_eq!(arrival_close_secs(closed), None);
        // At the gate every session hour has ended: nothing session-held is cut.
        let gate = arrival_close_secs(trading).expect("gate");
        let cut = arrival_open_from(day(), day(), gate);
        assert!(!left_out_as_open("tick_quotes", "2026-11-13T15", cut));
    }

    /// Regression (2026-11-14 review, MEDIUM x3): the archiver sorts only when
    /// an object already sits at the canonical key (only then can ordered
    /// bytes match it) and never inside the trading-day window; it does not
    /// read the export switch at all.
    #[test]
    fn test_archiver_orders_only_against_an_existing_object_outside_the_session() {
        let key = export_order_by("ticks");
        assert!(key.is_some());
        assert_eq!(archiver_export_order("ticks", true, false), key);
        assert_eq!(archiver_export_order("ticks", false, false), None);
        assert_eq!(archiver_export_order("ticks", true, true), None);
        assert_eq!(archiver_export_order("order_audit", true, false), None);
    }

    #[test]
    fn test_hex_to_base64_round_trips_with_the_archive_digest() {
        let (b64, hex) = sha256_pair(b"abc");
        assert_eq!(hex_to_base64(&hex), Some(b64));
        assert_eq!(hex_to_base64("abc"), None);
        assert_eq!(hex_to_base64("zz"), None);
    }

    /// The module never deletes, drops or overwrites (owner Quote 60).
    #[test]
    fn test_daily_export_module_never_deletes_or_drops() {
        let src = include_str!("daily_export.rs");
        let prod = src
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("prod part");
        for banned in [
            "delete_s3_object",
            ".delete(",
            "DeleteObject",
            "drop_partition",
            "DROP ",
            "append_audit",
            "append_verified_audit",
            "record_failure",
            "put_object()",
        ] {
            assert!(
                !prod.contains(banned),
                "the export must never use `{banned}`"
            );
        }
    }
}
