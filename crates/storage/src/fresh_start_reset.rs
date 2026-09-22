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
//! `run_live_table_ddl_at_boot` recreates `ticks` / `market_depth` /
//! `top_volume`, and `ensure_named_views` recreates every view. The reset
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
//!   boot then drops whatever the in-session boot wrote. The deploy band
//!   (no deploys 09:00–15:45 IST) makes the first boot of a new build an
//!   out-of-session one, so this needs a mid-session crash of a build that has
//!   never booted before.
//! - **An unreadable log is retried, but only within a bound.** The boot tries
//!   the log [`RESET_LOG_READ_ATTEMPTS`] times,
//!   [`RESET_LOG_READ_BACKOFF_SECS`] apart. If it still does not answer, the
//!   session boots on the OLD schema, and every write that day lands in the old
//!   column layout. The next out-of-session boot then drops that whole day.
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
    IST_UTC_OFFSET_SECONDS, TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
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

/// Views dropped first. They are recreated by `ensure_named_views` later in
/// the same boot. `candles_10m` is a VIEW over `candles_1m`, never a table.
pub const RESET_VIEWS: &[&str] = &[
    "candles_10m",
    "candles_named",
    "ticks_named",
    "market_depth_named",
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
    "top_volume",
    // `top_volume` under its pre-2026-09-12 name (added 2026-09-22, hostile
    // finding B5). Without it the reset drops `top_volume` and the SAME boot's
    // `ensure_top_volume_rank_table` then RENAMES a surviving legacy table into
    // its place, bringing every old-schema row back under the new name. It is
    // the same logical table, not a widening of the allowlist.
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
/// and the id write is its count bound. [`RESET_WORST_CASE_SECS`] is that sum.
/// A QuestDB that REFUSES (a fast 400) costs seconds, not this.
pub const RESET_RETRY_BUDGET_SECS: u64 = 120;

/// Upper bound on one reset call when every statement hangs to its timeout.
pub const RESET_WORST_CASE_SECS: u64 = RESET_RETRY_BUDGET_SECS
    + 2 * RESET_HTTP_TIMEOUT_SECS
    + (RESET_VIEWS.len() + RESET_TABLES.len()) as u64 * RESET_HTTP_TIMEOUT_SECS
    + RESET_ID_WRITE_ATTEMPTS as u64 * (3 * RESET_HTTP_TIMEOUT_SECS + RESET_LOG_READ_BACKOFF_SECS)
    + 3 * RESET_HTTP_TIMEOUT_SECS;

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

/// Every statement the drop pass issues, in order: views first (a base table
/// is freed of dependents before it goes), then tables.
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
    let ist = ist_secs_of_day(now_utc_epoch_secs());
    let mut decision = decide(logged, ist);

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
                     next boot outside 08:55–15:45 IST runs it, and will drop what this boot writes."
                );
            }
        }
        ResetDecision::Run => {
            let mut refused: Vec<(&'static str, String)> = Vec::new();
            for (object, sql) in drop_statements() {
                if exec(client, base_url, &sql).await.is_none() {
                    refused.push((object, sql));
                }
            }
            // A QuestDB finishing its own WAL replay can refuse DDL briefly. A
            // refused DROP left alone keeps the OLD schema for as long as that
            // table lives, so retry it — in order, views still first.
            let mut round = 0;
            while !refused.is_empty() && round < RESET_DROP_RETRY_ROUNDS && budget_left(started) {
                round += 1;
                tokio::time::sleep(backoff).await;
                let mut still: Vec<(&'static str, String)> = Vec::new();
                for (object, sql) in refused {
                    if exec(client, base_url, &sql).await.is_none() {
                        still.push((object, sql));
                    }
                }
                refused = still;
            }
            let refused: Vec<&'static str> = refused.into_iter().map(|(o, _)| o).collect();
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
            let (verified, id_attempts) = write_and_verify_id(client, base_url, backoff).await;
            if verified {
                info!(
                    reset_id = FRESH_START_RESET_ID,
                    dropped = drop_statements().len() - refused.len(),
                    id_attempts,
                    "fresh-start reset COMPLETE — id written and verified; the ensure DDL \
                     that follows recreates every dropped object"
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
        assert_eq!(RESET_TABLES.len(), 13);
        let candles = RESET_TABLES
            .iter()
            .filter(|t| t.starts_with("candles_"))
            .count();
        assert_eq!(candles, 9, "nine fold tables; candles_10m is a view");
        assert!(RESET_VIEWS.contains(&"candles_10m"));
        assert!(!RESET_TABLES.contains(&"candles_10m"));
        for t in ["top_volume", "ticks", "market_depth", "top_volume_rank"] {
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

    /// B5 (2026-09-22): `ensure_top_volume_rank_table` renames a surviving
    /// legacy table INTO `top_volume` later in the same boot. If the reset
    /// dropped `top_volume` but not its legacy name, that rename would bring
    /// every pre-reset row back. Pinned against the persistence module's own
    /// constant, not a copied literal, so a second rename cannot drift past it.
    #[test]
    fn the_reset_drops_top_volume_under_both_of_its_names() {
        use crate::top_volume_rank_persistence::{
            LEGACY_TOP_VOLUME_RANK_TABLE, TOP_VOLUME_RANK_TABLE,
        };
        assert!(RESET_TABLES.contains(&TOP_VOLUME_RANK_TABLE));
        assert!(RESET_TABLES.contains(&LEGACY_TOP_VOLUME_RANK_TABLE));
        // The legacy views go first, so the legacy DROP is never refused over
        // a dependent view.
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
            }
        }
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
        let seen_c = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                let (seen, logged, creates, drops_refused) = (
                    Arc::clone(&seen_c),
                    Arc::clone(&logged),
                    Arc::clone(&creates),
                    Arc::clone(&drops_refused),
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
                    } else if q.contains("table_name") {
                        cfg.tables_present
                            .map_or_else(|| REFUSED.to_owned(), count_reply)
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
        assert_eq!(seen_matching(&m, "INSERT"), 1);
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
        assert_eq!(seen_matching(&m, "INSERT"), 0);
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
        let insert = seen.iter().position(|q| q.contains("INSERT")).unwrap();
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
        assert_eq!(seen_matching(&m, "INSERT"), 0);
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
        assert_eq!(seen_matching(&m, "INSERT"), 1);
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
        assert_eq!(seen_matching(&m, "INSERT"), 0);
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
        assert_eq!(seen_matching(&m, "INSERT"), 1);
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
            seen_matching(&m, "INSERT"),
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
        assert_eq!(seen_matching(&m, "INSERT"), 1);
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
        assert!(!sql.contains("candles_10m"), "a view is not a table");
        assert!(!sql.contains(SCHEMA_RESET_LOG_TABLE));
    }

    #[test]
    fn the_worst_case_bound_is_the_documented_sum() {
        // 120 + 60 + 25 statements × 30 + 3 × (90 + 5) + 90.
        assert_eq!(RESET_WORST_CASE_SECS, 1_305);
    }
}
