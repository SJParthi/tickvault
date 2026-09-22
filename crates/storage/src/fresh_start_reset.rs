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
//! | id absent | yes | [`ResetDecision::RefuseInSession`] — never wipes a live session; the next out-of-session boot runs it |
//! | id absent | no | [`ResetDecision::Run`] — drop the allowlist, write the id, re-read it |
//!
//! The drops are followed by the boot's ordinary ensure path in the SAME boot:
//! `ensure_shadow_candle_tables` recreates the nine candle tables,
//! `run_live_table_ddl_at_boot` recreates `ticks` / `market_depth` /
//! `top_volume`, and `ensure_named_views` recreates every view. The reset
//! itself creates nothing but its own log.
//!
//! # Why it cannot fire twice
//!
//! There is exactly ONE id and it is a compile-time constant. Minting a second
//! one needs its own dated operator quote in the scope lock first. The id is
//! written LAST and then RE-READ; if the re-read does not find it the boot
//! says so loudly, because a wipe whose id is missing would repeat.
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
//!   re-writes the id.
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
    format!("INSERT INTO {SCHEMA_RESET_LOG_TABLE} VALUES ('{FRESH_START_RESET_ID}', now());")
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

/// [`run_fresh_start_reset_with`] with a bounded retry on an UNREADABLE log.
///
/// The clock is read AFTER the log answers, so the blackout decision uses the
/// time the drops would actually run, not the time the boot began.
pub async fn run_fresh_start_reset_retrying(
    client: &Client,
    base_url: &str,
    attempts: u32,
    backoff: Duration,
    now_utc_epoch_secs: impl Fn() -> i64,
) -> ResetDecision {
    let attempts = attempts.max(1);
    let mut logged = None;
    for attempt in 1..=attempts {
        logged = read_logged_count(client, base_url).await;
        if logged.is_some() {
            break;
        }
        if attempt < attempts {
            warn!(
                attempt,
                attempts, "fresh-start reset: the reset log did not answer, retrying"
            );
            tokio::time::sleep(backoff).await;
        }
    }
    let ist = ist_secs_of_day(now_utc_epoch_secs());
    let decision = decide(logged, ist);
    record(decision.as_str());

    match decision {
        ResetDecision::AlreadyDone => {
            info!(
                reset_id = FRESH_START_RESET_ID,
                "fresh-start reset already recorded — nothing to do"
            );
        }
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
        ResetDecision::Run => {
            let mut refused: Vec<&'static str> = Vec::new();
            for (object, sql) in drop_statements() {
                if exec(client, base_url, &sql).await.is_none() {
                    refused.push(object);
                }
            }
            if !refused.is_empty() {
                error!(
                    code = tickvault_common::error_code::ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    source = "fresh_start_reset",
                    reset_id = FRESH_START_RESET_ID,
                    refused = ?refused,
                    "fresh-start reset: these objects could NOT be dropped and keep their old \
                     schema. The reset id is still written so the wipe never repeats — drop \
                     them by hand outside market hours, and the next boot recreates them."
                );
                record("drop_refused");
            }
            let wrote = exec(client, base_url, &insert_log_sql()).await.is_some();
            let verified = wrote && read_logged_count(client, base_url).await.unwrap_or(0) > 0;
            if verified {
                info!(
                    reset_id = FRESH_START_RESET_ID,
                    dropped = drop_statements().len() - refused.len(),
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
                    wrote,
                    "fresh-start reset: the tables were dropped but the reset id could NOT be \
                     written and read back. The NEXT out-of-session boot will run the wipe \
                     AGAIN. Insert it by hand before then: \
                     INSERT INTO schema_reset_log VALUES ('2026-09-19-fresh-start', now());"
                );
                record("id_unverified");
            }
        }
    }
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
    fn blackout_edges() {
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

    /// A stateful QuestDB stand-in: `logged` is the id's current row count,
    /// an INSERT bumps it, every request line is recorded.
    struct Mock {
        port: u16,
        seen: Arc<Mutex<Vec<String>>>,
    }

    async fn spawn_mock(
        initial_logged: i64,
        create_ok: bool,
        insert_ok: bool,
        refuse_drop_of: Option<&'static str>,
    ) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let logged = Arc::new(Mutex::new(initial_logged));
        let seen_c = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                let seen = Arc::clone(&seen_c);
                let logged = Arc::clone(&logged);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let line = raw.lines().next().unwrap_or_default().to_owned();
                    let q = line.replace('+', " ").replace("%20", " ");
                    seen.lock().unwrap().push(q.clone());
                    let reply = if q.contains("CREATE") {
                        if create_ok {
                            OK.to_owned()
                        } else {
                            REFUSED.to_owned()
                        }
                    } else if q.contains("count") {
                        count_reply(*logged.lock().unwrap())
                    } else if q.contains("INSERT") {
                        if insert_ok {
                            *logged.lock().unwrap() += 1;
                            OK.to_owned()
                        } else {
                            REFUSED.to_owned()
                        }
                    } else if refuse_drop_of.is_some_and(|t| q.contains(&format!("EXISTS {t}"))) {
                        REFUSED.to_owned()
                    } else {
                        OK.to_owned()
                    };
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        Mock { port, seen }
    }

    /// A QuestDB that refuses the first `refuse_creates` CREATEs (a WAL
    /// replay that answers probes but not DDL), then behaves normally on a
    /// fresh volume.
    async fn spawn_slow_start_mock(refuse_creates: usize) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let creates = Arc::new(Mutex::new(0usize));
        let logged = Arc::new(Mutex::new(0i64));
        let seen_c = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                let (seen, creates, logged) = (
                    Arc::clone(&seen_c),
                    Arc::clone(&creates),
                    Arc::clone(&logged),
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
                        .replace("%20", " ");
                    seen.lock().unwrap().push(q.clone());
                    let reply = if q.contains("CREATE") {
                        let mut c = creates.lock().unwrap();
                        *c += 1;
                        if *c <= refuse_creates {
                            REFUSED.to_owned()
                        } else {
                            OK.to_owned()
                        }
                    } else if q.contains("count") {
                        count_reply(*logged.lock().unwrap())
                    } else if q.contains("INSERT") {
                        *logged.lock().unwrap() += 1;
                        OK.to_owned()
                    } else {
                        OK.to_owned()
                    };
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        Mock { port, seen }
    }

    /// B3 (2026-09-22): a log that answers on the third try RUNS the reset,
    /// instead of refusing the whole session on the first refusal.
    #[tokio::test]
    async fn a_slow_starting_questdb_is_retried_not_given_up_on() {
        let m = spawn_slow_start_mock(2).await;
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
    }

    /// The retry is BOUNDED: a log that never answers still refuses, drops
    /// nothing, and makes exactly `attempts` CREATE calls.
    #[tokio::test]
    async fn a_never_answering_log_gives_up_after_the_bound() {
        let m = spawn_slow_start_mock(usize::MAX).await;
        let d = run_fresh_start_reset_retrying(&client(), &url(&m), 3, Duration::ZERO, || {
            OFF_HOURS_UTC
        })
        .await;
        assert_eq!(d, ResetDecision::RefuseUnreadable);
        assert_eq!(drops_seen(&m), 0);
        let creates = m
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains("CREATE"))
            .count();
        assert_eq!(creates, 3);
    }

    /// The blackout is judged on the clock AFTER the log answers, so a boot
    /// that began outside the window but reached the decision inside it
    /// drops nothing.
    #[tokio::test]
    async fn the_blackout_is_judged_on_the_clock_after_the_retries() {
        let m = spawn_slow_start_mock(1).await;
        let calls = std::sync::atomic::AtomicU32::new(0);
        let d = run_fresh_start_reset_retrying(&client(), &url(&m), 3, Duration::ZERO, || {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            IN_SESSION_UTC
        })
        .await;
        assert_eq!(d, ResetDecision::RefuseInSession);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(drops_seen(&m), 0);
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

    fn drops_seen(m: &Mock) -> usize {
        m.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains("DROP"))
            .count()
    }

    #[tokio::test]
    async fn a_fresh_volume_off_hours_wipes_writes_the_id_and_verifies() {
        let m = spawn_mock(0, true, true, None).await;
        let d = run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await;
        assert_eq!(d, ResetDecision::Run);
        assert_eq!(drops_seen(&m), drop_statements().len());
        let seen = m.seen.lock().unwrap().clone();
        let last_drop = seen.iter().rposition(|q| q.contains("DROP")).unwrap();
        let insert = seen.iter().position(|q| q.contains("INSERT")).unwrap();
        assert!(insert > last_drop, "the id is written LAST");
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
    async fn an_in_session_boot_drops_nothing() {
        let m = spawn_mock(0, true, true, None).await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), IN_SESSION_UTC).await,
            ResetDecision::RefuseInSession
        );
        assert_eq!(drops_seen(&m), 0);
        assert!(m.seen.lock().unwrap().iter().all(|q| !q.contains("INSERT")));
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

    #[tokio::test]
    async fn a_refused_drop_still_writes_the_id_so_the_wipe_never_repeats() {
        let m = spawn_mock(0, true, true, Some("market_depth")).await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::Run
        );
        assert!(m.seen.lock().unwrap().iter().any(|q| q.contains("INSERT")));
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::AlreadyDone
        );
    }

    #[tokio::test]
    async fn an_unwritable_id_is_reported_and_the_next_boot_would_repeat() {
        let m = spawn_mock(0, true, false, None).await;
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::Run
        );
        // Honest consequence, pinned: without the id the next boot runs again.
        assert_eq!(
            run_fresh_start_reset_with(&client(), &url(&m), OFF_HOURS_UTC).await,
            ResetDecision::Run
        );
    }
}
