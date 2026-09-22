//! The one-shot fresh-start schema reset — `2026-09-19-fresh-start`.
//!
//! Authority: `websocket-connection-scope-lock.md`, "The fresh-scratch
//! mechanism — ONE TIME, THIS TIME ALONE" (operator, 2026-09-19:
//! *"for this time alone only it shoudl be the fresh newer approach"*).
//!
//! # What it does
//!
//! A `schema_reset_log` table holds one row per reset id. At boot the app
//! looks for [`FRESH_START_RESET_ID`]:
//!
//! | log state | in-session? | verdict |
//! |---|---|---|
//! | id present | — | [`ResetDecision::AlreadyDone`] — nothing happens, forever |
//! | unreadable / unwritable | — | [`ResetDecision::RefuseUnreadable`] — boots on whatever schema exists, loudly; NEVER wipes on a guess |
//! | id absent, none of the reset tables exist | yes | [`ResetDecision::NothingToWipe`] — a bare-nuked volume; the id is recorded so no later boot wipes today |
//! | id absent, reset tables exist (or unknown) | yes | [`ResetDecision::RefuseInSession`] — never wipes a live session; the next out-of-session boot runs it |
//! | id absent | no | [`ResetDecision::Run`] — drop the allowlist, write the id, re-read it |
//!
//! The drops are followed by the boot's ordinary ensure path in the SAME boot:
//! `ensure_shadow_candle_tables` recreates the nine candle tables,
//! `run_live_table_ddl_at_boot` recreates `ticks` / `market_depth` and the
//! four direct `top_volume_<tf>` tables, and `ensure_named_views` recreates
//! every console view. The reset
//! itself creates nothing but its own log.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS schema_reset_log (
//!     reset_id SYMBOL, ts TIMESTAMP
//! ) timestamp(ts);
//! ```
//!
//! Two fixed columns, no partitioning, no DEDUP key: it holds one row per
//! reset id ever run (one today), and the read-back before each re-insert is
//! what keeps a lost insert reply from doubling it. It has no generic
//! `ADD COLUMN IF NOT EXISTS` self-heal because its shape is the reset's
//! contract - a new column would be a new reset, with its own dated quote.
//!
//! # Why it cannot fire twice
//!
//! There is exactly ONE id and it is a compile-time constant. Minting a second
//! one needs its own dated operator quote in the scope lock first. The id is
//! written LAST and then RE-READ; if the re-read does not find it the boot
//! says so loudly, because a wipe whose id is missing would repeat.
//!
//! A DROP QuestDB refuses is retried [`RESET_DROP_RETRY_ROUNDS`] more times
//! before it is given up on, and the id write is tried
//! [`RESET_ID_WRITE_ATTEMPTS`] times, reading back before each re-insert so a
//! write whose reply was lost is never doubled.
//!
//! The id is written after the drop pass even when an individual DROP was
//! refused. A refused DROP is named in a coded error for the operator; NOT
//! writing the id would re-run the whole wipe on the next out-of-session boot
//! and destroy the tables that DID drop and have since captured a day of
//! data. "Never fires twice" is the stronger constraint.
//!
//! # Why it cannot reach a SEBI table
//!
//! [`RESET_TABLES`] and [`RESET_VIEWS`] are literal lists, checked against
//! [`SEBI_NEVER_RESET`] by a `const` assertion — a SEBI name added to either
//! list fails the BUILD, not the boot.
//!
//! # Honest limits (Rule 11)
//!
//! - **The data captured before this reset runs is DROPPED.** That is the
//!   point of a fresh start. If a build without this module captured a session
//!   first, that session's `ticks` / `market_depth` / candle / `top_volume`
//!   rows go with it. None of them is a SEBI table.
//! - **A refused in-session boot DEFERS the wipe.** The next out-of-session
//!   boot RENAMES any table the in-session boot wrote into to
//!   `<name>_pre_reset_<yyyymmdd>` (never drops it; see [`reset_action`]).
//!   Renamed tables sit outside retention and need a manual drop. The deploy band
//!   (no deploys 09:00–15:45 IST) makes the first boot of a new build an
//!   out-of-session one, so this needs a mid-session crash of a build that has
//!   never booted before.
//! - **An unreadable log is retried, but only within a bound.** The boot tries
//!   the log [`RESET_LOG_READ_ATTEMPTS`] times,
//!   [`RESET_LOG_READ_BACKOFF_SECS`] apart. If it still does not answer, the
//!   session boots on the OLD schema, and every write that day lands in the old
//!   column layout. The next out-of-session boot renames that day aside.
//!   The four refusal arms log `STORAGE-GAP-03`, so this is greppable and can be
//!   triaged. No alarm pages on it.
//! - **A bare nuke of the QuestDB volume deletes the log with everything else.**
//!   The next boot then "resets" an empty volume — a harmless no-op that
//!   re-writes the id. If that boot is IN-session it checks which reset tables
//!   exist; with none it records the id rather than deferring, so the next
//!   out-of-session boot does not drop the day this one captures. An
//!   unreadable answer to that check keeps the deferral.
//! - **Rows from before the reset can come back in the same boot.** The tick
//!   and depth spill drains and the staged live-feed WAL run AFTER the drops
//!   and write whatever they hold into the fresh tables. The seal spill is
//!   version-gated and does not.
//! - **A refused recreate after the drop leaves a table without its DEDUP
//!   key.** If the ensure DDL that follows the reset is refused on every
//!   attempt, the first ILP write auto-creates the table key-less, and the
//!   reset never runs again. Before this reset those tables normally already
//!   existed, so a refused ensure cost nothing.
//! - **Time bound.** Retries of the log read and of refused DROPs stop
//!   starting after [`RESET_RETRY_BUDGET_SECS`]. If every statement hangs its
//!   full HTTP timeout the call is bounded by [`RESET_WORST_CASE_SECS`]; a
//!   QuestDB that answers with a refusal costs seconds.
//! - **Not verified against a live QuestDB.** Port 9000 is unreachable from the
//!   build container. The HTTP exchange is pinned by a mock server below; the
//!   first boot after deploy is the measurement.

use std::time::Duration;

use reqwest::Client;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    IST_UTC_OFFSET_SECONDS, IST_UTC_OFFSET_SECONDS_I64, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tracing::{error, info, warn};

/// The ONE reset id. A compile-time constant by the scope lock's own rule.
pub const FRESH_START_RESET_ID: &str = "2026-09-19-fresh-start";

/// The table that remembers which resets have run.
///
/// Non-partitioned, so QuestDB keeps it OFF the write-ahead log and an
/// `INSERT` is visible to the very next `SELECT` — which is what the
/// verify-after-write step depends on.
pub const SCHEMA_RESET_LOG_TABLE: &str = "schema_reset_log";

/// Views dropped first. NONE is recreated: since 2026-09-22 (SECOND — NO VIEWS
/// ANYWHERE) the app creates no view. `candles_10m` was a VIEW over `candles_1m`
/// until then and is a real TABLE since, so it is in BOTH lists (see
/// [`reportable_refusals`]).
pub const RESET_VIEWS: &[&str] = &[
    "candles_10m",
    "candles_named",
    "ticks_named",
    "market_depth_named",
    // The four per-cadence top-volume faces were VIEWS until 2026-09-22 and are
    // DIRECT TABLES since. The same four names are therefore in BOTH lists: on
    // a volume written by an older build they are views (this pass drops them),
    // on a newer one they are tables (the table pass does). See
    // [`reportable_refusals`] for why the half that does not apply is never
    // reported as a failure.
    "top_volume_1s",
    "top_volume_3s",
    "top_volume_5s",
    "top_volume_1m",
    // The four faces of `top_volume` under its pre-2026-09-12 name. Dropped so a
    // legacy table (below) never refuses its DROP over a dependent view.
    "top_volume_rank_1s",
    "top_volume_rank_3s",
    "top_volume_rank_5s",
    "top_volume_rank_1m",
];

/// The ONLY tables the reset can drop — the scope lock's allowlist, literal.
pub const RESET_TABLES: &[&str] = &[
    "candles_1s",
    "candles_3s",
    "candles_5s",
    "candles_1m",
    "candles_3m",
    "candles_5m",
    "candles_15m",
    "candles_30m",
    "candles_60m",
    // The tenth fold frame (2026-09-22 SECOND). A VIEW on an older volume,
    // so also in [`RESET_VIEWS`]. The same logical object the reset already
    // dropped as a view, now dropped as the table it became — not a widening.
    "candles_10m",
    // The four direct per-cadence tables (2026-09-22). Also in [`RESET_VIEWS`]
    // — on an older volume they are views.
    "top_volume_1s",
    "top_volume_3s",
    "top_volume_5s",
    "top_volume_1m",
    // The retired single table and its pre-2026-09-12 name. No boot writes
    // either any more (2026-09-22), and nothing renames one into the other;
    // they are dropped so a fresh start leaves no old-schema ranking rows
    // behind. The same logical table, not a widening of the allowlist.
    "top_volume",
    "top_volume_rank",
    "ticks",
    "market_depth",
];

/// Five-year SEBI retention tables. Unreachable by construction.
pub const SEBI_NEVER_RESET: &[&str] = &[
    "instrument_lifecycle",
    "instrument_lifecycle_audit",
    "index_constituency",
    "order_audit",
    "order_update_events",
    "position_update_events",
    "ws_event_audit",
];

/// Byte-wise `str` equality usable in a `const` context.
const fn const_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn lists_are_disjoint(left: &[&str], right: &[&str]) -> bool {
    let mut i = 0;
    while i < left.len() {
        let mut j = 0;
        while j < right.len() {
            if const_str_eq(left[i], right[j]) {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

// A SEBI table in either reset list fails the BUILD, not the boot.
const _: () = assert!(lists_are_disjoint(RESET_TABLES, SEBI_NEVER_RESET));
const _: () = assert!(lists_are_disjoint(RESET_VIEWS, SEBI_NEVER_RESET));
// The log itself must never be in its own drop list — dropping it would make
// every later boot re-run the wipe.
const _: () = assert!(lists_are_disjoint(RESET_TABLES, &[SCHEMA_RESET_LOG_TABLE]));
const _: () = assert!(lists_are_disjoint(RESET_VIEWS, &[SCHEMA_RESET_LOG_TABLE]));

/// Refuse the wipe from five minutes before the persist window opens…
pub const RESET_BLACKOUT_START_SECS_OF_DAY_IST: u32 = TICK_PERSIST_START_SECS_OF_DAY_IST - 300;
/// …until the deploy band closes (15:45 IST), whatever the day. Wall clock
/// only — a stale holiday calendar must never let the wipe run mid-session.
pub const RESET_BLACKOUT_END_SECS_OF_DAY_IST: u32 = TICK_PERSIST_END_SECS_OF_DAY_IST + 300;

const _: () = assert!(RESET_BLACKOUT_START_SECS_OF_DAY_IST < TICK_PERSIST_START_SECS_OF_DAY_IST);
const _: () = assert!(RESET_BLACKOUT_END_SECS_OF_DAY_IST > TICK_PERSIST_END_SECS_OF_DAY_IST);

/// HTTP bound on every statement the reset issues.
const RESET_HTTP_TIMEOUT_SECS: u64 = 30;

/// How many times the BOOT tries to read the reset log before giving up.
///
/// Added 2026-09-22 (hostile finding B3). A QuestDB still replaying its own
/// write-ahead log answers a probe but refuses DDL for a while after start.
/// One refused read used to mean `RefuseUnreadable` for the WHOLE session, and
/// the next out-of-session boot then dropped that entire day. Six attempts
/// five seconds apart cover a 25-second replay window, the same budget
/// `run_live_table_ddl_at_boot` uses. Only an UNREADABLE log is retried.
/// A log that answers is final.
pub const RESET_LOG_READ_ATTEMPTS: u32 = 6;

/// Pause between reset-log read attempts.
pub const RESET_LOG_READ_BACKOFF_SECS: u64 = 5;

const _: () = assert!(RESET_LOG_READ_ATTEMPTS >= 1);
// The retries cannot push a boot that started before the blackout far
// enough to cross the 5-minute margin ahead of the persist window.
const _: () = assert!((RESET_LOG_READ_ATTEMPTS as u64 - 1) * RESET_LOG_READ_BACKOFF_SECS < 300);

/// Extra rounds for DROPs QuestDB refused on the first pass (added 2026-09-22,
/// review finding A03). A refused DROP left alone keeps the OLD schema for the
/// life of that table, because the id is written and the reset never returns.
pub const RESET_DROP_RETRY_ROUNDS: u32 = 2;

/// Attempts at writing the id and reading it back (added 2026-09-22). One lost
/// write used to mean the next out-of-session boot re-ran the whole wipe and
/// dropped the day captured in between, with nothing paging.
pub const RESET_ID_WRITE_ATTEMPTS: u32 = 3;

/// Wall-clock budget after which the reset stops STARTING retries of the log
/// read and of refused DROPs. First attempts and the id write are not cut.
///
/// Honest worst case, if every statement hangs its full
/// `RESET_HTTP_TIMEOUT_SECS`: the read phase can pass the budget by one
/// attempt (2 statements), the first drop pass is one statement per object,
/// and the id write is its count bound. Since 2026-09-22 (FOURTH, 44c) the
/// first-boot marker adds 3 statements and the table pass adds a census plus
/// one newest-row probe per table. [`RESET_WORST_CASE_SECS`] is that sum.
/// A QuestDB that REFUSES (a fast 400) costs seconds, not this.
pub const RESET_RETRY_BUDGET_SECS: u64 = 120;

/// Upper bound on one reset call when every statement hangs to its timeout.
pub const RESET_WORST_CASE_SECS: u64 = RESET_RETRY_BUDGET_SECS
    + 2 * RESET_HTTP_TIMEOUT_SECS
    + (RESET_VIEWS.len() + RESET_TABLES.len()) as u64 * RESET_HTTP_TIMEOUT_SECS
    + RESET_ID_WRITE_ATTEMPTS as u64 * (3 * RESET_HTTP_TIMEOUT_SECS + RESET_LOG_READ_BACKOFF_SECS)
    + 3 * RESET_HTTP_TIMEOUT_SECS
    + (4 + RESET_TABLES.len()) as u64 * RESET_HTTP_TIMEOUT_SECS;

const _: () = assert!(RESET_ID_WRITE_ATTEMPTS >= 1);
// Stays well inside an hour, so an unattended bad boot still finishes long
// before the 08:55 IST blackout that a 06:00 recovery might otherwise meet.
const _: () = assert!(RESET_WORST_CASE_SECS < 3600);

/// The boot's verdict on the one-shot reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetDecision {
    /// The id is already logged — nothing happens.
    AlreadyDone,
    /// The id is absent and the clock is outside the session — wipe.
    Run,
    /// The id is absent but the clock is inside the session — defer.
    RefuseInSession,
    /// The log could not be created or read — never wipe on a guess.
    RefuseUnreadable,
    /// In-session boot on a volume holding NONE of [`RESET_TABLES`] (a bare
    /// nuke): nothing to drop, so the id is recorded instead of deferring a
    /// wipe that would later destroy the day this boot captures.
    NothingToWipe,
}

impl ResetDecision {
    /// Stable label for logs and the counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyDone => "already_done",
            Self::Run => "run",
            Self::RefuseInSession => "refused_in_session",
            Self::RefuseUnreadable => "refused_unreadable",
            Self::NothingToWipe => "nothing_to_wipe",
        }
    }
}

/// Whether an IST second-of-day falls in the reset blackout.
#[must_use]
pub const fn in_reset_blackout(ist_secs_of_day: u32) -> bool {
    ist_secs_of_day >= RESET_BLACKOUT_START_SECS_OF_DAY_IST
        && ist_secs_of_day < RESET_BLACKOUT_END_SECS_OF_DAY_IST
}

/// Pure decision. `logged_count` is `None` when the log is unreadable.
///
/// Order is load-bearing: unreadable beats everything (never a guess), a
/// logged id beats the clock (a finished reset stays finished quietly), and
/// only then does the blackout decide.
#[must_use]
pub const fn decide(logged_count: Option<i64>, ist_secs_of_day: u32) -> ResetDecision {
    match logged_count {
        None => ResetDecision::RefuseUnreadable,
        Some(n) if n > 0 => ResetDecision::AlreadyDone,
        Some(_) if in_reset_blackout(ist_secs_of_day) => ResetDecision::RefuseInSession,
        Some(_) => ResetDecision::Run,
    }
}

/// Seconds since IST midnight for a UTC epoch second.
#[must_use]
pub fn ist_secs_of_day(utc_epoch_secs: i64) -> u32 {
    let ist = utc_epoch_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // rem_euclid keeps a pre-1970 clock in range instead of going negative.
    u32::try_from(ist.rem_euclid(86_400)).unwrap_or(0)
}

/// Extract `dataset[0][0]` from a QuestDB `/exec` count response.
///
/// Anything but a well-formed non-negative integer is `None` — the caller
/// treats that as UNREADABLE and refuses the wipe.
#[must_use]
pub fn parse_count(body: &str) -> Option<i64> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let n = value.get("dataset")?.get(0)?.get(0)?.as_i64()?;
    (n >= 0).then_some(n)
}

fn create_log_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {SCHEMA_RESET_LOG_TABLE} \
         (reset_id SYMBOL, ts TIMESTAMP) timestamp(ts);"
    )
}

fn count_log_sql() -> String {
    format!(
        "SELECT count() FROM {SCHEMA_RESET_LOG_TABLE} WHERE reset_id = '{FRESH_START_RESET_ID}';"
    )
}

fn insert_log_sql() -> String {
    format!(
        "INSERT INTO {SCHEMA_RESET_LOG_TABLE} (reset_id, ts) VALUES ('{FRESH_START_RESET_ID}', now());"
    )
}

/// The drop statements, in order: views first (a base table is freed of
/// dependents before it goes), then tables. The Run pass issues the view half
/// verbatim; a table gets its `DROP TABLE` only when [`reset_action`] says so
/// (2026-09-22 FOURTH, 44c), else it is renamed or skipped.
#[must_use]
pub fn drop_statements() -> Vec<(&'static str, String)> {
    let mut out = Vec::with_capacity(RESET_VIEWS.len() + RESET_TABLES.len());
    for view in RESET_VIEWS {
        out.push((*view, format!("DROP VIEW IF EXISTS {view};")));
    }
    for table in RESET_TABLES {
        out.push((*table, format!("DROP TABLE IF EXISTS {table};")));
    }
    out
}

/// The refusals worth REPORTING, out of every DROP still refused after the
/// retry rounds.
///
/// A name that is in BOTH [`RESET_VIEWS`] and [`RESET_TABLES`] (the four
/// `top_volume_<tf>` names and `candles_10m`, each a view on an older volume
/// and a table on a newer one) gets two statements, and only one of them can match what is on disk.
/// Whether QuestDB answers `DROP VIEW IF EXISTS` on a TABLE's name with a
/// no-op or an error is UNVERIFIED (no QuestDB is reachable from a dev
/// container). If it errors, that refusal is the wrong-kind half of a
/// successful drop, and reporting it would page "these objects could NOT be
/// dropped" about a table that is gone.
///
/// So a VIEW-drop refusal is suppressed when the same name is in
/// [`RESET_TABLES`] and its TABLE drop was NOT refused. Every other refusal is
/// reported. The honest cost: if a stubborn VIEW of that name survives while
/// `DROP TABLE IF EXISTS` on it is a no-op, the refusal is not named here —
/// and is then named by `ensure_top_volume_tables`, whose `CREATE TABLE`
/// cannot succeed over a live view and is a coded error of its own.
#[must_use]
pub fn reportable_refusals<'a>(refused: &[(&'a str, String)]) -> Vec<&'a str> {
    let table_refused = |name: &str| {
        refused
            .iter()
            .any(|(o, s)| *o == name && s.starts_with("DROP TABLE"))
    };
    refused
        .iter()
        .filter(|(object, sql)| {
            !(sql.starts_with("DROP VIEW")
                && RESET_TABLES.contains(object)
                && !table_refused(object))
        })
        .map(|(o, _)| *o)
        .collect()
}

// ---- 2026-09-22 (FOURTH), item 44c: rename rather than drop ---------------
//
// A reset refused mid-session let that day write into the OLD tables, and the
// next out-of-session boot dropped them. The table pass now asks each table
// how new its newest row is, against the instant the reset first found itself
// pending, and RENAMES aside any table holding a row from after that instant.

/// The reset log's second row kind: the first boot at which the reset was
/// pending. Written once (read before write), never swept — it lives in
/// [`SCHEMA_RESET_LOG_TABLE`], which is retention-exempt. Its `ts` is
/// QuestDB's `now()`, i.e. REAL UTC.
pub const FIRST_BOOT_MARKER_ID: &str = "2026-09-19-fresh-start.first-boot";

// The id count query is an exact `=` on FRESH_START_RESET_ID, so the marker
// row can never be read as "the reset already ran".
const _: () = assert!(!const_str_eq(FIRST_BOOT_MARKER_ID, FRESH_START_RESET_ID));

/// How far a table's newest `ts` can sit BEFORE the moment that row was
/// written: the widest candle bucket (60 min — a bar is stamped with its
/// OPEN) plus 5 min of late-tick lateness. Subtracted from the marker, so the
/// error is always toward RENAME. Every reset table stamps `ts` as naive IST
/// (the marker is converted into that domain by [`marker_cutoff_micros`]).
pub const NEWEST_ROW_LAG_MARGIN_SECS: i64 = 3_900;

/// Numbered suffixes tried after `<table>_pre_reset_<yyyymmdd>` is taken
/// (`_2` ..= `_9`). Past that the table is left untouched and the id is not
/// written — never a drop.
pub const RENAME_SUFFIX_ATTEMPTS: u32 = 9;

/// What a name in the reset lists is on disk right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// A table (or a name the census could not rule out — fail closed).
    Table,
    /// A name in [`RESET_VIEWS`].
    View,
    /// Positively absent from the table census.
    Missing,
}

/// The newest designated timestamp a table holds, in naive-IST micros.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewestRow {
    /// `count() = 0`.
    NoRows,
    /// `max(ts)` in naive-IST microseconds.
    At(i64),
    /// The probe was refused or did not parse.
    Unreadable,
}

/// The first-boot marker, already converted to the comparison cutoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerState {
    /// Rows at or after this naive-IST micros instant are protected.
    Present(i64),
    /// The log answered and holds no marker (its write failed).
    Absent,
    /// The marker could not be read.
    Unreadable,
}

/// What the table pass does to one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetAction {
    /// `DROP VIEW IF EXISTS` — the only statement a view name ever gets.
    DropView,
    /// `DROP TABLE IF EXISTS` — only for a table provably older than the marker.
    DropTable,
    /// `RENAME TABLE` to `<name>_pre_reset_<yyyymmdd>` — never a drop.
    Rename,
    /// Nothing to do.
    Skip,
}

/// Pure decision for one object. Fail-closed: anything not PROVABLY free of
/// rows written after the marker is renamed, never dropped.
#[must_use]
pub const fn reset_action(kind: ObjectKind, newest: NewestRow, marker: MarkerState) -> ResetAction {
    match kind {
        ObjectKind::View => ResetAction::DropView,
        ObjectKind::Missing => ResetAction::Skip,
        ObjectKind::Table => match (newest, marker) {
            // An empty table has nothing to lose, marker or not.
            (NewestRow::NoRows, _) => ResetAction::DropTable,
            (NewestRow::At(t), MarkerState::Present(m)) if t < m => ResetAction::DropTable,
            _ => ResetAction::Rename,
        },
    }
}

/// Refine a `Rename` verdict: a table whose OLDEST row is at or after the
/// marker cutoff holds ONLY post-marker capture — it was already dropped or
/// renamed by an earlier pass and recreated by the ensure DDL. Renaming it
/// again would empty the LIVE table on every retry boot (2026-09-22 hostile
/// review, HIGH), so it is skipped. Anything unproven stays `Rename`
/// (fail-closed): an unreadable or empty oldest probe, an absent or
/// unreadable marker, or an oldest row before the cutoff.
///
/// Honest residual: a table whose pre-marker partitions were removed by
/// the retention sweep (the reset failing for longer than the 15-day
/// market-data window, paging `STORAGE-GAP-03` every boot meanwhile) also
/// reads as post-marker and keeps its old schema.
#[must_use]
pub const fn refine_rename(oldest: NewestRow, marker: MarkerState) -> ResetAction {
    match (oldest, marker) {
        (NewestRow::At(t), MarkerState::Present(m)) if t >= m => ResetAction::Skip,
        _ => ResetAction::Rename,
    }
}

/// Convert the marker's REAL-UTC micros into the naive-IST domain the reset
/// tables stamp `ts` in, minus [`NEWEST_ROW_LAG_MARGIN_SECS`]. Saturating.
#[must_use]
pub const fn marker_cutoff_micros(marker_utc_micros: i64) -> i64 {
    marker_utc_micros
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64 * 1_000_000)
        .saturating_sub(NEWEST_ROW_LAG_MARGIN_SECS * 1_000_000)
}

/// The IST calendar date of a UTC epoch second, as `yyyymmdd`.
#[must_use]
pub fn ist_yyyymmdd(utc_epoch_secs: i64) -> u32 {
    use chrono::Datelike;
    let ist = utc_epoch_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64);
    chrono::DateTime::from_timestamp(ist, 0).map_or(19_700_101, |d| {
        let year = u32::try_from(d.year()).unwrap_or(1970);
        year * 10_000 + d.month() * 100 + d.day()
    })
}

/// The name a table is renamed to: `<table>_pre_reset_<yyyymmdd>`, then
/// `_2` ..= `_`[`RENAME_SUFFIX_ATTEMPTS`] while `taken` says a name exists.
/// `None` when every candidate is taken — the caller leaves the table alone.
#[must_use]
pub fn rename_target(
    table: &str,
    ist_yyyymmdd: u32,
    taken: impl Fn(&str) -> bool,
) -> Option<String> {
    let base = format!("{table}_pre_reset_{ist_yyyymmdd:08}");
    if !taken(&base) {
        return Some(base);
    }
    (2..=RENAME_SUFFIX_ATTEMPTS)
        .map(|n| format!("{base}_{n}"))
        .find(|candidate| !taken(candidate))
}

fn marker_read_sql() -> String {
    format!(
        "SELECT count(), cast(min(ts) AS LONG) FROM {SCHEMA_RESET_LOG_TABLE} \
         WHERE reset_id = '{FIRST_BOOT_MARKER_ID}';"
    )
}

fn marker_insert_sql() -> String {
    format!(
        "INSERT INTO {SCHEMA_RESET_LOG_TABLE} (reset_id, ts) VALUES ('{FIRST_BOOT_MARKER_ID}', now());"
    )
}

fn table_census_sql() -> &'static str {
    "SELECT table_name FROM tables();"
}

fn newest_row_sql(table: &str) -> String {
    format!("SELECT count(), cast(max(ts) AS LONG) FROM {table};")
}

fn oldest_row_sql(table: &str) -> String {
    format!("SELECT count(), cast(min(ts) AS LONG) FROM {table};")
}

fn rename_sql(from: &str, to: &str) -> String {
    format!("RENAME TABLE '{from}' TO '{to}';")
}

/// `dataset[0]` as `(count, optional long)`. `None` when either cell is
/// malformed — a null second cell is `Some((n, None))`.
#[must_use]
pub fn parse_count_and_long(body: &str) -> Option<(i64, Option<i64>)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let row = value.get("dataset")?.get(0)?;
    let n = row.get(0)?.as_i64().filter(|n| *n >= 0)?;
    let cell = row.get(1)?;
    if cell.is_null() {
        return Some((n, None));
    }
    Some((n, Some(cell.as_i64()?)))
}

/// Every `dataset[i][0]` string of a `tables()` answer. `None` when malformed.
#[must_use]
pub fn parse_table_names(body: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("dataset")?
        .as_array()?
        .iter()
        .map(|row| row.get(0)?.as_str().map(str::to_owned))
        .collect()
}

/// The marker row as `(count, min(ts))`; `None` when unreadable.
async fn read_first_boot_marker(client: &Client, base_url: &str) -> Option<(i64, Option<i64>)> {
    let body = exec(client, base_url, &marker_read_sql()).await?;
    parse_count_and_long(&body)
}

/// Read the marker; write it once if absent; read it back. Returns the raw
/// REAL-UTC micros state (not yet a cutoff).
async fn ensure_first_boot_marker(client: &Client, base_url: &str) -> MarkerState {
    match read_first_boot_marker(client, base_url).await {
        None => return MarkerState::Unreadable,
        Some((n, Some(micros))) if n > 0 => return MarkerState::Present(micros),
        Some((n, None)) if n > 0 => return MarkerState::Unreadable,
        Some(_) => {}
    }
    // Absent: record this boot. Read back regardless of the INSERT reply — an
    // INSERT whose answer was lost may still have landed.
    let _ = exec(client, base_url, &marker_insert_sql()).await;
    match read_first_boot_marker(client, base_url).await {
        Some((n, Some(micros))) if n > 0 => {
            info!(
                marker = FIRST_BOOT_MARKER_ID,
                "fresh-start reset: first boot with the reset pending recorded"
            );
            MarkerState::Present(micros)
        }
        Some((0, _)) => MarkerState::Absent,
        _ => MarkerState::Unreadable,
    }
}

async fn table_census(client: &Client, base_url: &str) -> Option<Vec<String>> {
    let body = exec(client, base_url, table_census_sql()).await?;
    parse_table_names(&body)
}

async fn newest_row(client: &Client, base_url: &str, table: &str) -> NewestRow {
    let Some(body) = exec(client, base_url, &newest_row_sql(table)).await else {
        return NewestRow::Unreadable;
    };
    match parse_count_and_long(&body) {
        Some((0, _)) => NewestRow::NoRows,
        Some((_, Some(ts))) => NewestRow::At(ts),
        _ => NewestRow::Unreadable,
    }
}

/// The OLDEST row of a table, probed only when [`reset_action`] chose
/// `Rename`. Same parse as [`newest_row`]; the variant carries `min(ts)`.
async fn oldest_row(client: &Client, base_url: &str, table: &str) -> NewestRow {
    let Some(body) = exec(client, base_url, &oldest_row_sql(table)).await else {
        return NewestRow::Unreadable;
    };
    match parse_count_and_long(&body) {
        Some((0, _)) => NewestRow::NoRows,
        Some((_, Some(ts))) => NewestRow::At(ts),
        _ => NewestRow::Unreadable,
    }
}

fn record_rename(table: &str, target: &str) {
    metrics::counter!("tv_fresh_start_reset_renamed_total").increment(1);
    // ERROR, not warn: a renamed table is outside retention and needs a
    // hand-drop, so it must reach the coded-error alarm (2026-09-22 review).
    error!(
        code = tickvault_common::error_code::ErrorCode::StorageGap03AuditWriteFailed.code_str(),
        source = "fresh_start_reset_rename",
        table,
        new_name = target,
        "fresh-start reset: table held rows newer than the first boot with the reset pending, \
         so it was RENAMED aside instead of dropped. It is outside retention — inspect it and \
         drop it by hand outside market hours."
    );
}

/// Run one statement; `Some(body)` on 2xx, `None` on anything else.
async fn exec(client: &Client, base_url: &str, sql: &str) -> Option<String> {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(resp) if resp.status().is_success() => resp.text().await.ok(),
        Ok(resp) => {
            warn!(status = %resp.status(), sql, "fresh-start reset: statement refused");
            None
        }
        Err(err) => {
            warn!(?err, sql, "fresh-start reset: statement transport failure");
            None
        }
    }
}

/// Read the log. `None` = unreadable (create refused, count refused, or an
/// answer that does not parse).
async fn read_logged_count(client: &Client, base_url: &str) -> Option<i64> {
    exec(client, base_url, &create_log_ddl()).await?;
    let body = exec(client, base_url, &count_log_sql()).await?;
    parse_count(&body)
}

/// SQL counting how many of [`RESET_TABLES`] exist right now.
fn reset_tables_present_sql() -> String {
    let names: Vec<String> = RESET_TABLES.iter().map(|t| format!("'{t}'")).collect();
    format!(
        "SELECT count() FROM tables() WHERE table_name IN ({});",
        names.join(", ")
    )
}

/// How many of [`RESET_TABLES`] exist. `None` when the answer is unreadable —
/// the caller then keeps its safe default and treats the volume as non-empty.
async fn reset_tables_present(client: &Client, base_url: &str) -> Option<i64> {
    let body = exec(client, base_url, &reset_tables_present_sql()).await?;
    parse_count(&body)
}

fn record(outcome: &'static str) {
    metrics::counter!("tv_fresh_start_reset_total", "outcome" => outcome).increment(1);
}

/// Run the one-shot reset against `base_url` (`http://host:port/exec`) with
/// the given wall clock. Returns the decision it acted on.
///
/// Separated from [`run_fresh_start_reset_at_boot`] so a mock QuestDB and a
/// pinned clock can drive every arm.
pub async fn run_fresh_start_reset_with(
    client: &Client,
    base_url: &str,
    utc_epoch_secs: i64,
) -> ResetDecision {
    run_fresh_start_reset_retrying(client, base_url, 1, Duration::ZERO, move || utc_epoch_secs)
        .await
}

/// Whether the retry budget still allows STARTING another retry.
fn budget_left(started: tokio::time::Instant) -> bool {
    started.elapsed() < Duration::from_secs(RESET_RETRY_BUDGET_SECS)
}

/// Write the id and read it back, up to [`RESET_ID_WRITE_ATTEMPTS`] times.
///
/// Bounded by COUNT, never by the budget: a missing id is the worst outcome
/// this module has (the next out-of-session boot re-runs the whole wipe), so
/// it always gets every attempt.
async fn write_and_verify_id(client: &Client, base_url: &str, backoff: Duration) -> (bool, u32) {
    for attempt in 1..=RESET_ID_WRITE_ATTEMPTS {
        // An INSERT whose answer was lost may still have landed, so read
        // before writing again rather than inserting blind a second time.
        if attempt > 1 && read_logged_count(client, base_url).await.unwrap_or(0) > 0 {
            return (true, attempt - 1);
        }
        let wrote = exec(client, base_url, &insert_log_sql()).await.is_some();
        if wrote && read_logged_count(client, base_url).await.unwrap_or(0) > 0 {
            return (true, attempt);
        }
        if attempt < RESET_ID_WRITE_ATTEMPTS {
            warn!(
                attempt,
                attempts = RESET_ID_WRITE_ATTEMPTS,
                "fresh-start reset: the reset id was not confirmed, retrying"
            );
            tokio::time::sleep(backoff).await;
        }
    }
    (false, RESET_ID_WRITE_ATTEMPTS)
}

/// [`run_fresh_start_reset_with`] with a bounded retry on an UNREADABLE log.
///
/// The clock is read AFTER the log answers, so the blackout decision uses the
/// time the drops would actually run, not the time the boot began.
///
/// Retries of the log read and of refused DROPs stop STARTING once
/// [`RESET_RETRY_BUDGET_SECS`] has elapsed; the id write is bounded by count
/// alone. Every first attempt always runs.
pub async fn run_fresh_start_reset_retrying(
    client: &Client,
    base_url: &str,
    attempts: u32,
    backoff: Duration,
    now_utc_epoch_secs: impl Fn() -> i64,
) -> ResetDecision {
    let started = tokio::time::Instant::now();
    let attempts = attempts.max(1);
    let mut logged = None;
    for attempt in 1..=attempts {
        logged = read_logged_count(client, base_url).await;
        if logged.is_some() {
            break;
        }
        if attempt < attempts {
            if !budget_left(started) {
                warn!(
                    attempt,
                    budget_secs = RESET_RETRY_BUDGET_SECS,
                    "fresh-start reset: retry budget spent before the reset log answered"
                );
                break;
            }
            warn!(
                attempt,
                attempts, "fresh-start reset: the reset log did not answer, retrying"
            );
            tokio::time::sleep(backoff).await;
        }
    }
    let now_utc = now_utc_epoch_secs();
    let ist = ist_secs_of_day(now_utc);
    let mut decision = decide(logged, ist);
    // Seeded so the first rename is not swallowed by the agent's
    // dropped-first-sample rule.
    metrics::counter!("tv_fresh_start_reset_renamed_total").increment(0);
    // The log answered and the id is absent: this boot may be the FIRST with
    // the reset pending. Record it, so a later Run can tell rows written since
    // (renamed) from rows written before (dropped). 2026-09-22 (FOURTH) 44c.
    let marker = if matches!(
        decision,
        ResetDecision::Run | ResetDecision::RefuseInSession
    ) {
        ensure_first_boot_marker(client, base_url).await
    } else {
        MarkerState::Unreadable
    };

    match decision {
        ResetDecision::AlreadyDone => {
            info!(
                reset_id = FRESH_START_RESET_ID,
                "fresh-start reset already recorded — nothing to do"
            );
        }
        ResetDecision::NothingToWipe => {}
        ResetDecision::RefuseUnreadable => {
            error!(
                code = tickvault_common::error_code::ErrorCode::StorageGap03AuditWriteFailed
                    .code_str(),
                source = "fresh_start_reset",
                reset_id = FRESH_START_RESET_ID,
                table = SCHEMA_RESET_LOG_TABLE,
                "fresh-start reset REFUSED: the reset log could not be created or read, so \
                 whether the one-shot already ran is unknown. Nothing was dropped — the app \
                 boots on the schema that exists. It retries on the next boot."
            );
        }
        ResetDecision::RefuseInSession => {
            // A bare-nuked volume booted in-session has NOTHING to wipe: the
            // ensure DDL below creates every table on the new schema. Deferring
            // anyway would let the next out-of-session boot drop the whole day
            // this boot captures, so record the id instead. Only a positive
            // zero counts — an unreadable answer keeps the deferral.
            if reset_tables_present(client, base_url).await == Some(0) {
                let (verified, _) = write_and_verify_id(client, base_url, backoff).await;
                if verified {
                    info!(
                        reset_id = FRESH_START_RESET_ID,
                        ist_secs_of_day = ist,
                        "fresh-start reset: in-session boot on a volume with none of the reset \
                         tables — nothing to wipe, id recorded so no later boot wipes today"
                    );
                    decision = ResetDecision::NothingToWipe;
                }
            }
            if decision == ResetDecision::RefuseInSession {
                error!(
                    code = tickvault_common::error_code::ErrorCode::StorageGap03AuditWriteFailed
                        .code_str(),
                    source = "fresh_start_reset",
                    reset_id = FRESH_START_RESET_ID,
                    ist_secs_of_day = ist,
                    "fresh-start reset DEFERRED: this boot is inside the market session window, \
                     and the one-shot wipe never runs on a live session. Nothing was dropped. The \
                     next boot outside 08:55–15:45 IST runs it, and RENAMES aside (never drops) any \
                     table this boot writes into."
                );
            }
        }
        ResetDecision::Run => {
            let cutoff = match marker {
                MarkerState::Present(utc_micros) => {
                    MarkerState::Present(marker_cutoff_micros(utc_micros))
                }
                other => other,
            };
            let run_date = ist_yyyymmdd(now_utc);
            let mut refused: Vec<(&'static str, String)> = Vec::new();
            let mut attempted = 0_usize;
            // Views first (a base table is freed of dependents before it goes):
            // a name in RESET_VIEWS only ever gets `DROP VIEW IF EXISTS`.
            for (view, sql) in drop_statements().into_iter().take(RESET_VIEWS.len()) {
                attempted += 1;
                if exec(client, base_url, &sql).await.is_none() {
                    refused.push((view, sql));
                }
            }
            // One census for kind and rename collisions. Unreadable = every
            // name is treated as a table (fail closed: probed, never assumed
            // absent).
            let census = table_census(client, base_url).await;
            let mut taken: std::collections::HashSet<String> =
                census.iter().flatten().cloned().collect();
            let mut renames: Vec<(&'static str, String)> = Vec::new();
            let mut rename_blocked: Vec<&'static str> = Vec::new();
            for table in RESET_TABLES {
                // Unreachable by the const assert; kept so a list edit can
                // never route a SEBI name into a probe, drop or rename.
                if SEBI_NEVER_RESET.contains(table) {
                    continue;
                }
                let kind = match &census {
                    Some(names) if !names.iter().any(|n| n == table) => ObjectKind::Missing,
                    _ => ObjectKind::Table,
                };
                let newest = if kind == ObjectKind::Table {
                    newest_row(client, base_url, table).await
                } else {
                    NewestRow::NoRows
                };
                match reset_action(kind, newest, cutoff) {
                    ResetAction::Skip | ResetAction::DropView => {}
                    ResetAction::DropTable => {
                        attempted += 1;
                        let sql = format!("DROP TABLE IF EXISTS {table};");
                        if exec(client, base_url, &sql).await.is_none() {
                            refused.push((*table, sql));
                        }
                    }
                    ResetAction::Rename => {
                        if matches!(
                            refine_rename(oldest_row(client, base_url, table).await, cutoff),
                            ResetAction::Skip
                        ) {
                            info!(
                                table,
                                "fresh-start reset: table holds only rows written after the \
                                 first boot — already reset by an earlier pass, left alone"
                            );
                            continue;
                        }
                        match rename_target(table, run_date, |n| taken.contains(n)) {
                            Some(target) => {
                                taken.insert(target.clone());
                                if exec(client, base_url, &rename_sql(table, &target))
                                    .await
                                    .is_some()
                                {
                                    record_rename(table, &target);
                                } else {
                                    renames.push((*table, target));
                                }
                            }
                            None => rename_blocked.push(table),
                        }
                    }
                }
            }
            // A QuestDB finishing its own WAL replay can refuse DDL briefly. A
            // refused DROP left alone keeps the OLD schema for as long as that
            // table lives, so retry it — in order, views still first. A
            // refused RENAME is retried the same way.
            let mut round = 0;
            while (!refused.is_empty() || !renames.is_empty())
                && round < RESET_DROP_RETRY_ROUNDS
                && budget_left(started)
            {
                round += 1;
                tokio::time::sleep(backoff).await;
                let mut still: Vec<(&'static str, String)> = Vec::new();
                for (object, sql) in refused {
                    if exec(client, base_url, &sql).await.is_none() {
                        still.push((object, sql));
                    }
                }
                refused = still;
                let mut still_renames: Vec<(&'static str, String)> = Vec::new();
                for (table, target) in renames {
                    if exec(client, base_url, &rename_sql(table, &target))
                        .await
                        .is_some()
                    {
                        record_rename(table, &target);
                    } else {
                        still_renames.push((table, target));
                    }
                }
                renames = still_renames;
            }
            let refused_count = refused.len();
            let refused: Vec<&'static str> = reportable_refusals(&refused);
            if !refused.is_empty() {
                error!(
                    code = tickvault_common::error_code::ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    source = "fresh_start_reset",
                    reset_id = FRESH_START_RESET_ID,
                    refused = ?refused,
                    retry_rounds = round,
                    "fresh-start reset: these objects could NOT be dropped and keep their old \
                     schema. The reset id is still written so the wipe never repeats — drop \
                     them by hand outside market hours, and the next boot recreates them."
                );
                record("drop_refused");
            }
            if !renames.is_empty() || !rename_blocked.is_empty() {
                // A table holding rows newer than the first boot is never
                // dropped. It stays untouched and the id is NOT written, so
                // the reset retries on the next out-of-session boot.
                let failed: Vec<&'static str> = renames.iter().map(|(t, _)| *t).collect();
                error!(
                    code = tickvault_common::error_code::ErrorCode::StorageGap03AuditWriteFailed
                        .code_str(),
                    source = "fresh_start_reset",
                    reset_id = FRESH_START_RESET_ID,
                    rename_refused = ?failed,
                    rename_target_taken = ?rename_blocked,
                    retry_rounds = round,
                    "fresh-start reset: these tables hold rows newer than the first boot with \
                     the reset pending and could NOT be renamed aside. They were left untouched \
                     and the reset id was NOT written, so the next out-of-session boot retries."
                );
                record("rename_refused");
                record(decision.as_str());
                return decision;
            }
            let (verified, id_attempts) = write_and_verify_id(client, base_url, backoff).await;
            if verified {
                info!(
                    reset_id = FRESH_START_RESET_ID,
                    dropped = attempted - refused_count,
                    id_attempts,
                    "fresh-start reset COMPLETE — id written and verified; the ensure DDL \
                     that follows recreates every dropped or renamed object"
                );
                record("completed");
            } else {
                error!(
                    code = tickvault_common::error_code::ErrorCode::StorageGap03AuditWriteFailed
                        .code_str(),
                    source = "fresh_start_reset",
                    reset_id = FRESH_START_RESET_ID,
                    id_attempts,
                    "fresh-start reset: the tables were dropped but the reset id could NOT be \
                     written and read back. The NEXT out-of-session boot will run the wipe \
                     AGAIN. Insert it by hand before then: \
                     INSERT INTO schema_reset_log (reset_id, ts) VALUES ('2026-09-19-fresh-start', now());"
                );
                record("id_unverified");
            }
        }
    }
    record(decision.as_str());
    decision
}

/// Boot entry point. Awaited BEFORE any table DDL in
/// `candle_ddl_boot::run_candle_ddl_at_boot`, which runs before the feed stack
/// is spawned — so no writer is live while the drops run.
// TEST-EXEMPT: thin wrapper over run_fresh_start_reset_with (mock-server tested below) plus a client build and the wall clock.
pub async fn run_fresh_start_reset_at_boot(questdb: &QuestDbConfig) -> ResetDecision {
    let client = match Client::builder()
        .timeout(Duration::from_secs(RESET_HTTP_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            error!(
                source = "fresh_start_reset",
                ?err,
                code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 fresh-start reset client build failed — reset REFUSED this \
                 boot, nothing dropped"
            );
            record(ResetDecision::RefuseUnreadable.as_str());
            return ResetDecision::RefuseUnreadable;
        }
    };
    let base_url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    run_fresh_start_reset_retrying(
        &client,
        &base_url,
        RESET_LOG_READ_ATTEMPTS,
        Duration::from_secs(RESET_LOG_READ_BACKOFF_SECS),
        || chrono::Utc::now().timestamp(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // 2026-09-21 00:00:00 UTC = 05:30 IST — outside the blackout.
    const OFF_HOURS_UTC: i64 = 1_789_948_800;
    // Same day + 5h = 10:30 IST — inside the blackout.
    const IN_SESSION_UTC: i64 = OFF_HOURS_UTC + 5 * 3600;

    #[test]
    fn the_id_is_the_scope_locks_one_constant() {
        assert_eq!(FRESH_START_RESET_ID, "2026-09-19-fresh-start");
    }

    #[test]
    fn the_allowlist_is_exactly_the_scope_lock_set() {
        assert_eq!(RESET_TABLES.len(), 18);
        let candles = RESET_TABLES
            .iter()
            .filter(|t| t.starts_with("candles_"))
            .count();
        assert_eq!(candles, 10, "ten fold tables, candles_10m included");
        // A view on an older volume, a table since 2026-09-22 (SECOND).
        assert!(RESET_VIEWS.contains(&"candles_10m"));
        assert!(RESET_TABLES.contains(&"candles_10m"));
        for t in [
            "top_volume_1s",
            "top_volume_3s",
            "top_volume_5s",
            "top_volume_1m",
            "top_volume",
            "ticks",
            "market_depth",
            "top_volume_rank",
        ] {
            assert!(RESET_TABLES.contains(&t), "{t} missing");
        }
        // Emitted candle set must equal the reset's candle set, so a future
        // tenth fold frame fails here instead of surviving a reset unnoticed.
        let mut emitted = crate::shadow_persistence::emitted_candle_table_names();
        emitted.sort_unstable();
        let mut reset: Vec<&str> = RESET_TABLES
            .iter()
            .copied()
            .filter(|t| t.starts_with("candles_"))
            .collect();
        reset.sort_unstable();
        assert_eq!(emitted, reset);
    }

    /// Every top-volume name the persistence module knows — the four live
    /// tables and both retired names — is dropped by the reset, pinned against
    /// the module's own constants so a fifth cadence cannot survive a reset
    /// unnoticed. The legacy VIEWS go before the legacy table, so its DROP is
    /// never refused over a dependent view.
    #[test]
    fn the_reset_drops_every_top_volume_name() {
        use crate::top_volume_rank_persistence::{
            LEGACY_TOP_VOLUME_RANK_TABLE, LEGACY_TOP_VOLUME_TABLE, SnapshotCadence,
        };
        assert!(RESET_TABLES.contains(&LEGACY_TOP_VOLUME_TABLE));
        assert!(RESET_TABLES.contains(&LEGACY_TOP_VOLUME_RANK_TABLE));
        for c in SnapshotCadence::ALL {
            assert!(
                RESET_TABLES.contains(&c.table_name()),
                "{} table",
                c.table_name()
            );
            assert!(
                RESET_VIEWS.contains(&c.table_name()),
                "{} view",
                c.table_name()
            );
        }
        let stmts = drop_statements();
        let legacy_table = stmts
            .iter()
            .position(|(o, _)| *o == LEGACY_TOP_VOLUME_RANK_TABLE)
            .unwrap();
        for tf in ["1s", "3s", "5s", "1m"] {
            let view = format!("{LEGACY_TOP_VOLUME_RANK_TABLE}_{tf}");
            let at = stmts.iter().position(|(o, _)| *o == view).unwrap();
            assert!(at < legacy_table, "{view} must drop before its table");
        }
    }

    /// The only names in BOTH lists are the four per-cadence names and
    /// `candles_10m` — any other
    /// overlap would make [`reportable_refusals`] suppress a real refusal.
    #[test]
    fn only_the_cadence_names_and_candles_10m_are_both_a_view_and_a_table() {
        let both: Vec<&str> = RESET_VIEWS
            .iter()
            .copied()
            .filter(|v| RESET_TABLES.contains(v))
            .collect();
        assert_eq!(
            both,
            [
                "candles_10m",
                "top_volume_1s",
                "top_volume_3s",
                "top_volume_5s",
                "top_volume_1m"
            ]
        );
    }

    fn view(name: &'static str) -> (&'static str, String) {
        (name, format!("DROP VIEW IF EXISTS {name};"))
    }
    fn table(name: &'static str) -> (&'static str, String) {
        (name, format!("DROP TABLE IF EXISTS {name};"))
    }

    /// Every combination of "view drop refused / table drop refused" for a
    /// dual-listed name, plus single-listed names of both kinds.
    #[test]
    fn reportable_refusals_every_permutation() {
        // Dual-listed name: the four outcomes.
        assert!(reportable_refusals(&[]).is_empty(), "neither refused");
        assert!(
            reportable_refusals(&[view("top_volume_1s")]).is_empty(),
            "view half refused, table dropped — the wrong-kind half, suppressed"
        );
        assert_eq!(
            reportable_refusals(&[table("top_volume_1s")]),
            ["top_volume_1s"],
            "a refused TABLE drop is always reported"
        );
        assert_eq!(
            reportable_refusals(&[view("top_volume_1s"), table("top_volume_1s")]),
            ["top_volume_1s", "top_volume_1s"],
            "both halves refused — nothing dropped it, both reported"
        );
        // A view-only name is never suppressed.
        assert_eq!(reportable_refusals(&[view("ticks_named")]), ["ticks_named"]);
        // A table-only name is never suppressed.
        assert_eq!(reportable_refusals(&[table("ticks")]), ["ticks"]);
        // One name's table refusal never un-suppresses ANOTHER name's view half.
        assert_eq!(
            reportable_refusals(&[view("top_volume_1s"), table("top_volume_3s")]),
            ["top_volume_3s"]
        );
        // Order is preserved.
        assert_eq!(
            reportable_refusals(&[table("ticks"), view("ticks_named"), view("top_volume_5s")]),
            ["ticks", "ticks_named"]
        );
    }

    #[test]
    fn no_sebi_table_is_reachable() {
        for s in SEBI_NEVER_RESET {
            assert!(!RESET_TABLES.contains(s) && !RESET_VIEWS.contains(s), "{s}");
            assert!(drop_statements().iter().all(|(o, _)| o != s), "{s}");
        }
        assert!(lists_are_disjoint(RESET_TABLES, SEBI_NEVER_RESET));
        assert!(!lists_are_disjoint(&["order_audit"], SEBI_NEVER_RESET));
        assert!(!const_str_eq("order_audit", "order_audi"));
    }

    #[test]
    fn decide_covers_every_arm_in_priority_order() {
        let off = ist_secs_of_day(OFF_HOURS_UTC);
        let on = ist_secs_of_day(IN_SESSION_UTC);
        assert_eq!(decide(None, off), ResetDecision::RefuseUnreadable);
        assert_eq!(decide(None, on), ResetDecision::RefuseUnreadable);
        assert_eq!(decide(Some(1), on), ResetDecision::AlreadyDone);
        assert_eq!(decide(Some(3), off), ResetDecision::AlreadyDone);
        assert_eq!(decide(Some(0), on), ResetDecision::RefuseInSession);
        assert_eq!(decide(Some(0), off), ResetDecision::Run);
    }

    #[test]
    fn test_ist_secs_of_day_and_blackout_edges() {
        assert_eq!(RESET_BLACKOUT_START_SECS_OF_DAY_IST, 8 * 3600 + 55 * 60);
        assert_eq!(RESET_BLACKOUT_END_SECS_OF_DAY_IST, 15 * 3600 + 45 * 60);
        assert!(!in_reset_blackout(RESET_BLACKOUT_START_SECS_OF_DAY_IST - 1));
        assert!(in_reset_blackout(RESET_BLACKOUT_START_SECS_OF_DAY_IST));
        assert!(in_reset_blackout(RESET_BLACKOUT_END_SECS_OF_DAY_IST - 1));
        assert!(!in_reset_blackout(RESET_BLACKOUT_END_SECS_OF_DAY_IST));
        assert_eq!(ist_secs_of_day(OFF_HOURS_UTC), 5 * 3600 + 30 * 60);
        assert_eq!(ist_secs_of_day(IN_SESSION_UTC), 10 * 3600 + 30 * 60);
        assert!(ist_secs_of_day(-1) < 86_400);
    }

    #[test]
    fn parse_count_refuses_anything_but_a_count() {
        assert_eq!(parse_count(r#"{"dataset":[[0]],"count":1}"#), Some(0));
        assert_eq!(parse_count(r#"{"dataset":[[2]],"count":1}"#), Some(2));
        assert_eq!(parse_count(r#"{"dataset":[],"count":0}"#), None);
        assert_eq!(parse_count(r#"{"dataset":[["x"]]}"#), None);
        assert_eq!(parse_count(r#"{"dataset":[[-1]]}"#), None);
        assert_eq!(parse_count("not json"), None);
        assert_eq!(parse_count(r#"{"error":"table does not exist"}"#), None);
    }

    #[test]
    fn drop_statements_drop_views_before_tables() {
        let stmts = drop_statements();
        let first_table = stmts
            .iter()
            .position(|(_, s)| s.starts_with("DROP TABLE"))
            .unwrap();
        assert!(
            stmts[..first_table]
                .iter()
                .all(|(_, s)| s.starts_with("DROP VIEW IF EXISTS"))
        );
        assert!(
            stmts[first_table..]
                .iter()
                .all(|(_, s)| s.starts_with("DROP TABLE IF EXISTS"))
        );
        assert!(
            stmts
                .iter()
                .all(|(_, s)| !s.contains(SCHEMA_RESET_LOG_TABLE))
        );
    }

    // ---- mock QuestDB ----------------------------------------------------

    const OK: &str = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}";
    const REFUSED: &str = "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 16\r\n\r\n{\"error\":\"nope\"}";

    fn count_reply(n: i64) -> String {
        let body = format!("{{\"dataset\":[[{n}]],\"count\":1}}");
        format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// How the mock answers the id INSERT.
    #[derive(Clone, Copy)]
    enum Insert {
        Ok,
        Refuse,
        /// The row lands but the reply is lost — the case a blind re-insert
        /// would double up on.
        LandButRefuse,
    }

    /// The newest-row probe's answer for one table.
    #[derive(Clone, Copy)]
    enum Newest {
        /// `(count, max(ts) as naive-IST micros)`.
        Rows(i64, Option<i64>),
        Refuse,
    }

    /// The `SELECT table_name FROM tables()` answer.
    #[derive(Clone, Copy)]
    enum Census {
        Refuse,
        /// Every reset table, every SEBI table, plus these extras.
        AllPlus(&'static [&'static str]),
        /// Exactly these names.
        Only(&'static [&'static str]),
    }

    /// QuestDB's `now()` as the mock's marker INSERT records it.
    const QDB_NOW_MICROS: i64 = IN_SESSION_UTC * 1_000_000;

    fn no_rows(_: &str) -> Newest {
        Newest::Rows(0, None)
    }

    /// Everything the stand-in QuestDB can be told to do.
    #[derive(Clone, Copy)]
    struct Cfg {
        initial_logged: i64,
        /// Refuse this many CREATEs first (a WAL replay answering probes but
        /// not DDL); `usize::MAX` = never answer.
        refuse_creates: usize,
        insert: Insert,
        /// Refuse the DROP of this object this many times.
        refuse_drop: Option<(&'static str, usize)>,
        /// Answer to the "which reset tables exist" count; `None` = refuse.
        tables_present: Option<i64>,
        /// First-boot marker already recorded, as REAL-UTC micros.
        marker: Option<i64>,
        marker_insert_ok: bool,
        marker_read_ok: bool,
        census: Census,
        newest: fn(&str) -> Newest,
        /// The `min(ts)` probe asked only on a `Rename` verdict. Defaults to
        /// refused, which keeps the verdict `Rename` (fail-closed).
        oldest: fn(&str) -> Newest,
        /// Refuse the RENAME of this table this many times.
        refuse_rename: Option<(&'static str, usize)>,
    }

    impl Default for Cfg {
        fn default() -> Self {
            Self {
                initial_logged: 0,
                refuse_creates: 0,
                insert: Insert::Ok,
                refuse_drop: None,
                // A populated volume unless a test says otherwise.
                tables_present: Some(RESET_TABLES.len() as i64),
                marker: None,
                marker_insert_ok: true,
                marker_read_ok: true,
                census: Census::AllPlus(&[]),
                newest: no_rows,
                oldest: |_| Newest::Refuse,
                refuse_rename: None,
            }
        }
    }

    fn count_long_reply(n: i64, v: Option<i64>) -> String {
        let cell = v.map_or_else(|| "null".to_owned(), |v| v.to_string());
        let body = format!("{{\"dataset\":[[{n},{cell}]],\"count\":1}}");
        format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn census_reply(census: Census) -> String {
        let names: Vec<&str> = match census {
            Census::Refuse => return REFUSED.to_owned(),
            Census::Only(names) => names.to_vec(),
            Census::AllPlus(extra) => RESET_TABLES
                .iter()
                .chain(SEBI_NEVER_RESET)
                .chain(extra)
                .copied()
                .collect(),
        };
        let rows: Vec<String> = names.iter().map(|n| format!("[\"{n}\"]")).collect();
        let body = format!(
            "{{\"dataset\":[{}],\"count\":{}}}",
            rows.join(","),
            rows.len()
        );
        format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// A stateful QuestDB stand-in: every request line is recorded.
    struct Mock {
        port: u16,
        seen: Arc<Mutex<Vec<String>>>,
    }

    async fn spawn_mock_with(cfg: Cfg) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let logged = Arc::new(Mutex::new(cfg.initial_logged));
        let creates = Arc::new(Mutex::new(0usize));
        let drops_refused = Arc::new(Mutex::new(0usize));
        let renames_refused = Arc::new(Mutex::new(0usize));
        let marker = Arc::new(Mutex::new(cfg.marker));
        let seen_c = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                let (seen, logged, creates, drops_refused, renames_refused, marker) = (
                    Arc::clone(&seen_c),
                    Arc::clone(&logged),
                    Arc::clone(&creates),
                    Arc::clone(&drops_refused),
                    Arc::clone(&renames_refused),
                    Arc::clone(&marker),
                );
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let q = raw
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .replace('+', " ")
                        .replace("%20", " ")
                        .replace("%28", "(")
                        .replace("%29", ")")
                        .replace("%2C", ",")
                        .replace("%27", "'")
                        .replace("%3B", ";");
                    seen.lock().unwrap().push(q.clone());
                    let reply = if q.contains("CREATE") {
                        let mut c = creates.lock().unwrap();
                        *c += 1;
                        if *c <= cfg.refuse_creates {
                            REFUSED.to_owned()
                        } else {
                            OK.to_owned()
                        }
                    } else if q.contains("first-boot") {
                        if q.contains("INSERT") {
                            if cfg.marker_insert_ok {
                                let mut m = marker.lock().unwrap();
                                if m.is_none() {
                                    *m = Some(QDB_NOW_MICROS);
                                }
                                OK.to_owned()
                            } else {
                                REFUSED.to_owned()
                            }
                        } else if cfg.marker_read_ok {
                            let m = *marker.lock().unwrap();
                            count_long_reply(i64::from(m.is_some()), m)
                        } else {
                            REFUSED.to_owned()
                        }
                    } else if q.contains("tables()") && !q.contains("count") {
                        census_reply(cfg.census)
                    } else if q.contains("table_name") {
                        cfg.tables_present
                            .map_or_else(|| REFUSED.to_owned(), count_reply)
                    } else if q.contains("min(ts)") {
                        let table = q
                            .split("FROM ")
                            .nth(1)
                            .and_then(|r| r.split(';').next())
                            .unwrap_or_default();
                        match (cfg.oldest)(table) {
                            Newest::Rows(n, v) => count_long_reply(n, v),
                            Newest::Refuse => REFUSED.to_owned(),
                        }
                    } else if q.contains("max(ts)") {
                        let table = q
                            .split("FROM ")
                            .nth(1)
                            .and_then(|r| r.split(';').next())
                            .unwrap_or_default();
                        match (cfg.newest)(table) {
                            Newest::Rows(n, v) => count_long_reply(n, v),
                            Newest::Refuse => REFUSED.to_owned(),
                        }
                    } else if q.contains("RENAME") {
                        match cfg.refuse_rename {
                            Some((t, times)) if q.contains(&format!("TABLE '{t}' TO")) => {
                                let mut r = renames_refused.lock().unwrap();
                                if *r < times {
                                    *r += 1;
                                    REFUSED.to_owned()
                                } else {
                                    OK.to_owned()
                                }
                            }
                            _ => OK.to_owned(),
                        }
                    } else if q.contains("count") {
                        count_reply(*logged.lock().unwrap())
                    } else if q.contains("INSERT") {
                        match cfg.insert {
                            Insert::Ok => {
                                *logged.lock().unwrap() += 1;
                                OK.to_owned()
                            }
                            Insert::Refuse => REFUSED.to_owned(),
                            Insert::LandButRefuse => {
                                *logged.lock().unwrap() += 1;
                                REFUSED.to_owned()
                            }
                        }
                    } else if let Some((t, times)) = cfg.refuse_drop
                        && q.contains(&format!("EXISTS {t};"))
                    {
                        let mut r = drops_refused.lock().unwrap();
                        if *r < times {
                            *r += 1;
                            REFUSED.to_owned()
                        } else {
                            OK.to_owned()
                        }
                    } else {
                        OK.to_owned()
                    };
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        Mock { port, seen }
    }

    async fn spawn_mock(
        initial_logged: i64,
        create_ok: bool,
        insert_ok: bool,
        refuse_drop_of: Option<&'static str>,
    ) -> Mock {
        spawn_mock_with(Cfg {
            initial_logged,
            refuse_creates: if create_ok { 0 } else { usize::MAX },
            insert: if insert_ok {
                Insert::Ok
            } else {
                Insert::Refuse
            },
            refuse_drop: refuse_drop_of.map(|t| (t, usize::MAX)),
            ..Cfg::default()
        })
        .await
    }

    fn client() -> Client {
        Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
    }

    fn url(m: &Mock) -> String {
        format!("http://127.0.0.1:{}/exec", m.port)
    }

    fn seen_matching(m: &Mock, needle: &str) -> usize {
        m.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains(needle))
            .count()
    }

    fn drops_seen(m: &Mock) -> usize {
        seen_matching(m, "DROP")
    }

    /// The reset-id INSERT, never the first-boot marker's.
    fn is_id_insert(q: &str) -> bool {
        q.contains("INSERT") && q.contains(&format!("'{FRESH_START_RESET_ID}'"))
    }

    fn id_inserts(m: &Mock) -> usize {
        m.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|q| is_id_insert(q))
            .count()
    }

    /// B3 (2026-09-22): a log that answers on the third try RUNS the reset,
    /// instead of refusing the whole session on the first refusal.
    #[tokio::test]
    async fn test_run_fresh_start_reset_retrying_waits_out_a_slow_starting_questdb() {
        let m = spawn_mock_with(Cfg {
            refuse_creates: 2,
            ..Cfg::default()
        })
        .await;
        let d = run_fresh_start_reset_retrying(
            &client(),
            &url(&m),
            RESET_LOG_READ_ATTEMPTS,
            Duration::ZERO,
            || OFF_HOURS_UTC,
        )
        .await;
        assert_eq!(d, ResetDecision::Run);
        assert_eq!(drops_seen(&m), drop_statements().len());
        assert_eq!(id_inserts(&m), 1);
    }

    /// The retry is BOUNDED: a log that never answers still refuses, drops
    /// nothing, and makes exactly `attempts` CREATE calls.
    #[tokio::test]
    async fn a_never_answering_log_gives_up_after_the_bound() {
        let m = spawn_mock_with(Cfg {
            refuse_creates: usize::MAX,
            ..Cfg::default()
        })
        .await;
        let d = run_fresh_start_reset_retrying(&client(), &url(&m), 3, Duration::ZERO, || {
            OFF_HOURS_UTC
        })
        .await;
        assert_eq!(d, ResetDecision::RefuseUnreadable);
        assert_eq!(drops_seen(&m), 0);
        assert_eq!(seen_matching(&m, "CREATE"), 3);
        assert_eq!(id_inserts(&m), 0);
    }

    /// The blackout is judged on the clock AFTER the log answers. The clock
    /// here reads OFF-hours until the second CREATE, then IN-session — so a
    /// runner that read it at the START would see off-hours and drop, and
    /// this test would fail.
    #[tokio::test]
    async fn the_blackout_is_judged_on_the_clock_after_the_retries() {
        let m = spawn_mock_with(Cfg {
            refuse_creates: 1,
            ..Cfg::default()
        })
        .await;
        let seen = Arc::clone(&m.seen);
        let d = run_fresh_start_reset_retrying(&client(), &url(&m), 3, Duration::ZERO, || {
            let creates = seen
                .lock()
                .unwrap()
                .iter()
                .filter(|q| q.contains("CREATE"))
                .count();
            if creates >= 2 {
                IN_SESSION_UTC
            } else {
                OFF_HOURS_UTC
            }
        })
        .await;
        assert_eq!(d, ResetDecision::RefuseInSession);
        assert_eq!(drops_seen(&m), 0);
    }

    #[tokio::test]
    async fn test_run_fresh_start_reset_with_fresh_volume_off_hours_wipes_writes_the_id_and_verifies()
     {
        let m = spawn_mock(0, true, true, None).await;
        let d = run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await;
        assert_eq!(d, ResetDecision::Run);
        assert_eq!(drops_seen(&m), drop_statements().len());
        let seen = m.seen.lock().unwrap().clone();
        let last_drop = seen.iter().rposition(|q| q.contains("DROP")).unwrap();
        let insert = seen.iter().position(|q| is_id_insert(q)).unwrap();
        assert!(insert > last_drop, "the id is written LAST");
        assert!(
            seen[insert].contains("(reset_id, ts)"),
            "the id INSERT names its columns"
        );
        assert!(
            seen[insert + 1..].iter().any(|q| q.contains("count")),
            "the id is re-read after it is written"
        );
    }

    #[tokio::test]
    async fn a_second_boot_does_nothing() {
        let m = spawn_mock(0, true, true, None).await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::Run
        );
        let before = drops_seen(&m);
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::AlreadyDone
        );
        assert_eq!(drops_seen(&m), before, "no DROP on the second boot");
    }

    #[tokio::test]
    async fn an_in_session_boot_on_a_populated_volume_drops_nothing() {
        let m = spawn_mock(0, true, true, None).await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), IN_SESSION_UTC).await,
            ResetDecision::RefuseInSession
        );
        assert_eq!(drops_seen(&m), 0);
        assert_eq!(id_inserts(&m), 0);
        assert_eq!(seen_matching(&m, "table_name"), 1, "the volume was checked");
    }

    /// MEDIUM-3 (2026-09-22): a bare-nuked volume booted in-session has
    /// nothing to wipe. It records the id instead of deferring — otherwise
    /// the next out-of-session boot drops the whole day this boot captures.
    #[tokio::test]
    async fn an_in_session_boot_on_a_bare_nuked_volume_records_the_id() {
        let m = spawn_mock_with(Cfg {
            tables_present: Some(0),
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), IN_SESSION_UTC).await,
            ResetDecision::NothingToWipe
        );
        assert_eq!(drops_seen(&m), 0);
        assert_eq!(id_inserts(&m), 1);
        // The next OFF-hours boot does not wipe the day.
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::AlreadyDone
        );
        assert_eq!(drops_seen(&m), 0);
    }

    /// An unreadable "which tables exist" answer is NEVER read as empty.
    #[tokio::test]
    async fn an_unreadable_table_census_keeps_the_deferral() {
        let m = spawn_mock_with(Cfg {
            tables_present: None,
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), IN_SESSION_UTC).await,
            ResetDecision::RefuseInSession
        );
        assert_eq!(id_inserts(&m), 0);
    }

    #[tokio::test]
    async fn an_unreadable_log_drops_nothing() {
        let m = spawn_mock(0, false, true, None).await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::RefuseUnreadable
        );
        assert_eq!(drops_seen(&m), 0);
    }

    #[tokio::test]
    async fn a_dead_questdb_drops_nothing() {
        // Bind then drop, so the port refuses connections.
        let port = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let d = run_fresh_start_reset_with(
            &client(),
            &format!("http://127.0.0.1:{port}/exec"),
            OFF_HOURS_UTC,
        )
        .await;
        assert_eq!(d, ResetDecision::RefuseUnreadable);
    }

    /// A DROP that keeps failing is retried the full bound, then named, and
    /// the id is STILL written so the wipe never repeats.
    #[tokio::test]
    async fn a_refused_drop_is_retried_then_the_id_still_written() {
        let m = spawn_mock(0, true, true, Some("market_depth")).await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::Run
        );
        assert_eq!(
            seen_matching(&m, "EXISTS market_depth;"),
            1 + RESET_DROP_RETRY_ROUNDS as usize,
            "the refused DROP is retried, and ONLY it"
        );
        assert_eq!(
            drops_seen(&m),
            drop_statements().len() + RESET_DROP_RETRY_ROUNDS as usize
        );
        assert_eq!(id_inserts(&m), 1);
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::AlreadyDone
        );
    }

    /// A DROP refused once (QuestDB still replaying) succeeds on the retry.
    #[tokio::test]
    async fn a_briefly_refused_drop_succeeds_on_the_retry() {
        let m = spawn_mock_with(Cfg {
            refuse_drop: Some(("ticks", 1)),
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::Run
        );
        assert_eq!(seen_matching(&m, "EXISTS ticks;"), 2);
        assert_eq!(drops_seen(&m), drop_statements().len() + 1);
    }

    #[tokio::test]
    async fn an_unwritable_id_is_retried_then_reported_and_the_next_boot_would_repeat() {
        let m = spawn_mock(0, true, false, None).await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::Run
        );
        assert_eq!(
            id_inserts(&m),
            RESET_ID_WRITE_ATTEMPTS as usize,
            "every id-write attempt was spent"
        );
        // Honest consequence, pinned: without the id the next boot runs again.
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::Run
        );
    }

    /// An INSERT that landed but whose answer was lost is found by the
    /// re-read — never inserted a second time.
    #[tokio::test]
    async fn a_lost_insert_reply_is_not_inserted_twice() {
        let m = spawn_mock_with(Cfg {
            insert: Insert::LandButRefuse,
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::Run
        );
        assert_eq!(id_inserts(&m), 1);
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::AlreadyDone
        );
    }

    #[test]
    fn the_census_counts_exactly_the_reset_tables() {
        let sql = reset_tables_present_sql();
        for t in RESET_TABLES {
            assert!(
                sql.contains(&format!("'{t}'")),
                "{t} missing from the census"
            );
        }
        assert!(sql.contains("'candles_10m'"), "candles_10m is a table now");
        assert!(!sql.contains(SCHEMA_RESET_LOG_TABLE));
    }

    #[test]
    fn the_worst_case_bound_is_the_documented_sum() {
        // 120 + 60 + 30 statements × 30 + 3 × (90 + 5) + 90, plus (44c) the
        // marker's 3 statements, the census and 18 newest-row probes × 30.
        assert_eq!(RESET_WORST_CASE_SECS, 2_115);
    }

    // ---- 2026-09-22 (FOURTH) 44c: rename rather than drop -----------------

    /// 17:30 IST on the same day as `IN_SESSION_UTC` — outside the blackout.
    const EVENING_UTC: i64 = OFF_HOURS_UTC + 12 * 3600;
    const RUN_DATE: &str = "20260921";

    /// A REAL-UTC second as the naive-IST micros the reset tables stamp.
    const fn ist_naive_micros(utc_secs: i64) -> i64 {
        (utc_secs + IST_UTC_OFFSET_SECONDS_I64) * 1_000_000
    }

    /// Every (kind × newest × marker) permutation, expected value written out
    /// rather than recomputed.
    #[test]
    fn reset_action_full_permutation_grid() {
        use MarkerState as M;
        use NewestRow as N;
        use ResetAction as A;
        let m = 1_000_000_i64;
        let newest = [
            N::NoRows,
            N::At(m - 1),
            N::At(m),
            N::At(m + 1),
            N::Unreadable,
        ];
        let markers = [M::Present(m), M::Absent, M::Unreadable];
        for n in newest {
            for mk in markers {
                assert_eq!(
                    reset_action(ObjectKind::View, n, mk),
                    A::DropView,
                    "{n:?} {mk:?}"
                );
                assert_eq!(
                    reset_action(ObjectKind::Missing, n, mk),
                    A::Skip,
                    "{n:?} {mk:?}"
                );
            }
        }
        let table: [(NewestRow, MarkerState, ResetAction); 15] = [
            (N::NoRows, M::Present(m), A::DropTable),
            (N::NoRows, M::Absent, A::DropTable),
            (N::NoRows, M::Unreadable, A::DropTable),
            (N::At(m - 1), M::Present(m), A::DropTable),
            (N::At(m - 1), M::Absent, A::Rename),
            (N::At(m - 1), M::Unreadable, A::Rename),
            (N::At(m), M::Present(m), A::Rename),
            (N::At(m), M::Absent, A::Rename),
            (N::At(m), M::Unreadable, A::Rename),
            (N::At(m + 1), M::Present(m), A::Rename),
            (N::At(m + 1), M::Absent, A::Rename),
            (N::At(m + 1), M::Unreadable, A::Rename),
            (N::Unreadable, M::Present(m), A::Rename),
            (N::Unreadable, M::Absent, A::Rename),
            (N::Unreadable, M::Unreadable, A::Rename),
        ];
        for (n, mk, want) in table {
            assert_eq!(reset_action(ObjectKind::Table, n, mk), want, "{n:?} {mk:?}");
        }
    }

    proptest::proptest! {
        /// Around the boundary: a table is dropped iff its newest row is
        /// STRICTLY before the cutoff, and only when the marker is present.
        #[test]
        fn a_table_is_dropped_only_strictly_before_the_marker(
            m in proptest::prelude::any::<i64>(),
            d in -5_i64..=5,
        ) {
            let t = m.saturating_add(d);
            let got = reset_action(ObjectKind::Table, NewestRow::At(t), MarkerState::Present(m));
            let want = if t < m { ResetAction::DropTable } else { ResetAction::Rename };
            proptest::prop_assert_eq!(got, want);
            for mk in [MarkerState::Absent, MarkerState::Unreadable] {
                proptest::prop_assert_eq!(
                    reset_action(ObjectKind::Table, NewestRow::At(t), mk),
                    ResetAction::Rename
                );
            }
        }

        /// The cutoff never moves LATER than the marker in naive IST: a row
        /// written after the marker (ts >= its naive-IST instant minus the
        /// bucket lag) is always renamed.
        #[test]
        fn a_row_inside_the_lag_margin_is_renamed(
            marker_secs in 0_i64..4_102_444_800,
            back in 0_i64..=NEWEST_ROW_LAG_MARGIN_SECS,
        ) {
            let cutoff = marker_cutoff_micros(marker_secs * 1_000_000);
            let row = ist_naive_micros(marker_secs) - back * 1_000_000;
            proptest::prop_assert_eq!(
                reset_action(ObjectKind::Table, NewestRow::At(row), MarkerState::Present(cutoff)),
                ResetAction::Rename
            );
        }
    }

    #[test]
    fn the_marker_cutoff_is_naive_ist_minus_the_margin_and_saturates() {
        assert_eq!(marker_cutoff_micros(0), (19_800 - 3_900) * 1_000_000);
        assert_eq!(marker_cutoff_micros(i64::MAX), i64::MAX - 3_900 * 1_000_000);
        assert_eq!(
            marker_cutoff_micros(i64::MIN),
            i64::MIN + (19_800 - 3_900) * 1_000_000
        );
        assert_ne!(FIRST_BOOT_MARKER_ID, FRESH_START_RESET_ID);
        assert!(!FIRST_BOOT_MARKER_ID.contains('\''));
    }

    #[test]
    fn ist_yyyymmdd_rolls_at_ist_midnight_not_utc() {
        assert_eq!(ist_yyyymmdd(OFF_HOURS_UTC), 20_260_921);
        // 18:29:59 UTC = 23:59:59 IST, same date; one second later rolls.
        assert_eq!(
            ist_yyyymmdd(OFF_HOURS_UTC + 18 * 3600 + 29 * 60 + 59),
            20_260_921
        );
        assert_eq!(
            ist_yyyymmdd(OFF_HOURS_UTC + 18 * 3600 + 30 * 60),
            20_260_922
        );
        assert_eq!(ist_yyyymmdd(i64::MAX), 19_700_101);
    }

    fn is_questdb_identifier(s: &str) -> bool {
        s.len() <= 127
            && s.starts_with(|c: char| c.is_ascii_lowercase())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }

    #[test]
    fn rename_target_is_deterministic_and_a_valid_identifier() {
        for table in RESET_TABLES {
            let a = rename_target(table, 20_260_921, |_| false).unwrap();
            let b = rename_target(table, 20_260_921, |_| false).unwrap();
            assert_eq!(a, b);
            assert_eq!(a, format!("{table}_pre_reset_20260921"));
            assert!(is_questdb_identifier(&a), "{a}");
            assert!(!RESET_TABLES.contains(&a.as_str()) && !RESET_VIEWS.contains(&a.as_str()));
            assert!(!SEBI_NEVER_RESET.contains(&a.as_str()));
        }
        // Zero-padded, so a date always renders as eight digits.
        assert_eq!(
            rename_target("ticks", 1_010_101, |_| false).unwrap(),
            "ticks_pre_reset_01010101"
        );
        // Taken -> numbered suffix, in order.
        let base = "ticks_pre_reset_20260921";
        assert_eq!(
            rename_target("ticks", 20_260_921, |n| n == base).unwrap(),
            format!("{base}_2")
        );
        let t = rename_target("ticks", 20_260_921, |n| {
            n == base || n == format!("{base}_2")
        });
        assert_eq!(t.unwrap(), format!("{base}_3"));
        // Every candidate taken -> None, never a drop.
        assert_eq!(rename_target("ticks", 20_260_921, |_| true), None);
    }

    #[test]
    fn parse_count_and_long_accepts_a_count_and_an_optional_long() {
        assert_eq!(
            parse_count_and_long(r#"{"dataset":[[2,1700000000000000]]}"#),
            Some((2, Some(1_700_000_000_000_000)))
        );
        assert_eq!(
            parse_count_and_long(r#"{"dataset":[[0,null]]}"#),
            Some((0, None))
        );
        assert_eq!(parse_count_and_long(r#"{"dataset":[[1,"x"]]}"#), None);
        assert_eq!(parse_count_and_long(r#"{"dataset":[[-1,null]]}"#), None);
        assert_eq!(parse_count_and_long(r#"{"dataset":[[1]]}"#), None);
        assert_eq!(parse_count_and_long("nope"), None);
    }

    #[test]
    fn parse_table_names_reads_one_name_per_row_or_refuses() {
        assert_eq!(
            parse_table_names(r#"{"dataset":[["ticks"],["candles_1m"]]}"#),
            Some(vec!["ticks".to_owned(), "candles_1m".to_owned()])
        );
        assert_eq!(parse_table_names(r#"{"dataset":[]}"#), Some(vec![]));
        assert_eq!(parse_table_names(r#"{"dataset":[[1]]}"#), None);
        assert_eq!(parse_table_names(r#"{"error":"x"}"#), None);
    }

    fn renames_of(m: &Mock, table: &str) -> Vec<String> {
        m.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains(&format!("RENAME TABLE '{table}' TO")))
            .cloned()
            .collect()
    }

    /// `ticks` holds a row from 11:30 IST; `candles_1m` only one from 08:30.
    fn day_rows(table: &str) -> Newest {
        match table {
            "ticks" => Newest::Rows(5, Some(ist_naive_micros(IN_SESSION_UTC + 3600))),
            "candles_1m" => Newest::Rows(3, Some(ist_naive_micros(IN_SESSION_UTC - 2 * 3600))),
            _ => Newest::Rows(0, None),
        }
    }

    /// THE regression (2026-09-22 FOURTH): an in-session boot defers the wipe
    /// and writes into the old tables; the evening boot must RENAME a table
    /// holding that day's rows, not drop it.
    #[tokio::test]
    async fn a_deferred_reset_renames_what_the_session_wrote() {
        let m = spawn_mock_with(Cfg {
            newest: day_rows,
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), IN_SESSION_UTC).await,
            ResetDecision::RefuseInSession
        );
        assert_eq!(
            seen_matching(&m, "first-boot"),
            3,
            "read, insert, read back"
        );
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
            ResetDecision::Run
        );
        assert_eq!(
            renames_of(&m, "ticks").len(),
            1,
            "the session's table is renamed aside"
        );
        assert!(renames_of(&m, "ticks")[0].contains(&format!("'ticks_pre_reset_{RUN_DATE}'")));
        assert_eq!(seen_matching(&m, "DROP TABLE IF EXISTS ticks;"), 0);
        assert_eq!(
            seen_matching(&m, "DROP TABLE IF EXISTS candles_1m;"),
            1,
            "a table holding only pre-marker rows is still dropped"
        );
        assert_eq!(id_inserts(&m), 1);
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
            ResetDecision::AlreadyDone
        );
    }

    /// HIGH (2026-09-22 hostile review): a retry boot must NOT rename a table
    /// that holds only post-marker rows — that is the live table the DDL
    /// recreated after an earlier pass, and renaming it empties it every boot.
    #[tokio::test]
    async fn a_retry_boot_leaves_a_table_holding_only_post_marker_rows_alone() {
        fn oldest_is_newest(table: &str) -> Newest {
            day_rows(table)
        }
        let m = spawn_mock_with(Cfg {
            newest: day_rows,
            oldest: oldest_is_newest,
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), IN_SESSION_UTC).await,
            ResetDecision::RefuseInSession
        );
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
            ResetDecision::Run
        );
        assert!(
            renames_of(&m, "ticks").is_empty(),
            "a table whose OLDEST row is post-marker is already fresh — never renamed"
        );
        assert_eq!(seen_matching(&m, "DROP TABLE IF EXISTS ticks;"), 0);
        assert_eq!(seen_matching(&m, "DROP TABLE IF EXISTS candles_1m;"), 1);
        assert_eq!(id_inserts(&m), 1);
    }

    #[test]
    fn refine_rename_skips_only_a_provably_post_marker_table() {
        let m = 1_000_000_i64;
        let cases = [
            (NewestRow::At(m), MarkerState::Present(m), ResetAction::Skip),
            (
                NewestRow::At(m + 1),
                MarkerState::Present(m),
                ResetAction::Skip,
            ),
            (
                NewestRow::At(m - 1),
                MarkerState::Present(m),
                ResetAction::Rename,
            ),
            (NewestRow::At(m), MarkerState::Absent, ResetAction::Rename),
            (
                NewestRow::At(m),
                MarkerState::Unreadable,
                ResetAction::Rename,
            ),
            (
                NewestRow::Unreadable,
                MarkerState::Present(m),
                ResetAction::Rename,
            ),
            (
                NewestRow::NoRows,
                MarkerState::Present(m),
                ResetAction::Rename,
            ),
        ];
        for (oldest, marker, want) in cases {
            assert_eq!(refine_rename(oldest, marker), want, "{oldest:?} {marker:?}");
        }
    }

    /// The normal first boot (marker written THIS boot): rows from a
    /// previous session are older than the cutoff and are dropped.
    #[tokio::test]
    async fn a_first_boot_run_still_drops_old_rows() {
        fn ancient(_: &str) -> Newest {
            Newest::Rows(9, Some(0))
        }
        let m = spawn_mock_with(Cfg {
            newest: ancient,
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
            ResetDecision::Run
        );
        assert_eq!(seen_matching(&m, "RENAME"), 0);
        assert_eq!(drops_seen(&m), drop_statements().len());
        assert_eq!(id_inserts(&m), 1);
    }

    /// A refused RENAME leaves the table alone AND leaves the id unwritten,
    /// so the next boot retries; once it succeeds the id is written.
    #[tokio::test]
    async fn a_failed_rename_does_not_mark_the_reset_done() {
        let m = spawn_mock_with(Cfg {
            marker: Some(QDB_NOW_MICROS),
            newest: day_rows,
            refuse_rename: Some(("ticks", 1 + RESET_DROP_RETRY_ROUNDS as usize)),
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
            ResetDecision::Run
        );
        assert_eq!(
            renames_of(&m, "ticks").len(),
            1 + RESET_DROP_RETRY_ROUNDS as usize
        );
        assert_eq!(
            seen_matching(&m, "DROP TABLE IF EXISTS ticks;"),
            0,
            "never a drop"
        );
        assert_eq!(id_inserts(&m), 0, "the reset is NOT marked done");
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
            ResetDecision::Run,
            "the next boot retries"
        );
        assert_eq!(id_inserts(&m), 1);
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
            ResetDecision::AlreadyDone
        );
    }

    fn old_rows_everywhere(_: &str) -> Newest {
        Newest::Rows(1, Some(0))
    }

    /// Marker write refused (Absent) or marker unreadable: FAIL CLOSED —
    /// every table holding rows is renamed, not one is dropped.
    #[tokio::test]
    async fn an_absent_or_unreadable_marker_renames_every_non_empty_table() {
        for (insert_ok, read_ok) in [(false, true), (true, false)] {
            let m = spawn_mock_with(Cfg {
                marker_insert_ok: insert_ok,
                marker_read_ok: read_ok,
                newest: old_rows_everywhere,
                ..Cfg::default()
            })
            .await;
            assert_eq!(
                run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
                ResetDecision::Run
            );
            assert_eq!(seen_matching(&m, "DROP TABLE"), 0, "{insert_ok} {read_ok}");
            assert_eq!(seen_matching(&m, "RENAME TABLE"), RESET_TABLES.len());
            assert_eq!(seen_matching(&m, "DROP VIEW IF EXISTS"), RESET_VIEWS.len());
        }
    }

    /// A newest-row probe that is refused is never read as "empty".
    #[tokio::test]
    async fn an_unreadable_newest_row_probe_renames() {
        fn refuse(_: &str) -> Newest {
            Newest::Refuse
        }
        let m = spawn_mock_with(Cfg {
            marker: Some(QDB_NOW_MICROS),
            newest: refuse,
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
            ResetDecision::Run
        );
        assert_eq!(seen_matching(&m, "DROP TABLE"), 0);
        assert_eq!(seen_matching(&m, "RENAME TABLE"), RESET_TABLES.len());
    }

    const TICKS_TAKEN: &[&str] = &["ticks_pre_reset_20260921"];
    const TICKS_ALL_TAKEN: &[&str] = &[
        "ticks_pre_reset_20260921",
        "ticks_pre_reset_20260921_2",
        "ticks_pre_reset_20260921_3",
        "ticks_pre_reset_20260921_4",
        "ticks_pre_reset_20260921_5",
        "ticks_pre_reset_20260921_6",
        "ticks_pre_reset_20260921_7",
        "ticks_pre_reset_20260921_8",
        "ticks_pre_reset_20260921_9",
    ];

    #[tokio::test]
    async fn a_taken_rename_target_gets_a_suffix_and_all_taken_skips() {
        let m = spawn_mock_with(Cfg {
            marker: Some(QDB_NOW_MICROS),
            newest: day_rows,
            census: Census::AllPlus(TICKS_TAKEN),
            ..Cfg::default()
        })
        .await;
        run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await;
        assert!(renames_of(&m, "ticks")[0].contains("'ticks_pre_reset_20260921_2'"));
        assert_eq!(id_inserts(&m), 1);

        let m = spawn_mock_with(Cfg {
            marker: Some(QDB_NOW_MICROS),
            newest: day_rows,
            census: Census::AllPlus(TICKS_ALL_TAKEN),
            ..Cfg::default()
        })
        .await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await,
            ResetDecision::Run
        );
        assert!(renames_of(&m, "ticks").is_empty());
        assert_eq!(
            seen_matching(&m, "DROP TABLE IF EXISTS ticks;"),
            0,
            "never a drop"
        );
        assert_eq!(id_inserts(&m), 0, "skipped, so the reset retries");
    }

    /// SEBI tables sit in the census holding rows; no statement the reset
    /// sends ever names one — the decision is never reached for them.
    #[tokio::test]
    async fn no_statement_ever_names_a_sebi_table() {
        let m = spawn_mock_with(Cfg {
            newest: old_rows_everywhere,
            marker_insert_ok: false,
            ..Cfg::default()
        })
        .await;
        run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await;
        let seen = m.seen.lock().unwrap().clone();
        for sebi in SEBI_NEVER_RESET {
            assert!(
                seen.iter()
                    .all(|q| !q.contains(&format!(" {sebi};")) && !q.contains(&format!("'{sebi}'"))),
                "{sebi} reached a statement"
            );
        }
    }

    /// A name only in RESET_VIEWS gets `DROP VIEW IF EXISTS` and nothing else.
    #[tokio::test]
    async fn a_view_name_only_ever_gets_drop_view_if_exists() {
        let m = spawn_mock_with(Cfg {
            newest: old_rows_everywhere,
            ..Cfg::default()
        })
        .await;
        run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await;
        for view in RESET_VIEWS.iter().filter(|v| !RESET_TABLES.contains(v)) {
            assert_eq!(
                seen_matching(&m, &format!("DROP VIEW IF EXISTS {view};")),
                1
            );
            assert_eq!(
                seen_matching(&m, &format!("DROP TABLE IF EXISTS {view};")),
                0
            );
            assert_eq!(seen_matching(&m, &format!("TABLE '{view}'")), 0);
            assert_eq!(seen_matching(&m, &format!("FROM {view};")), 0);
        }
    }

    const ONLY_TICKS: &[&str] = &["ticks"];

    #[tokio::test]
    async fn a_missing_table_is_skipped_and_an_unreadable_census_probes_everything() {
        let m = spawn_mock_with(Cfg {
            census: Census::Only(ONLY_TICKS),
            ..Cfg::default()
        })
        .await;
        run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await;
        assert_eq!(seen_matching(&m, "max(ts)"), 1);
        assert_eq!(seen_matching(&m, "DROP TABLE"), 1);
        assert_eq!(seen_matching(&m, "DROP TABLE IF EXISTS market_depth;"), 0);
        assert_eq!(id_inserts(&m), 1);

        let m = spawn_mock_with(Cfg {
            census: Census::Refuse,
            ..Cfg::default()
        })
        .await;
        run_fresh_start_reset_with(&client(), &url(&m), EVENING_UTC).await;
        assert_eq!(seen_matching(&m, "max(ts)"), RESET_TABLES.len());
    }
}
