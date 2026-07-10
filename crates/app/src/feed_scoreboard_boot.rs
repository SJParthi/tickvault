//! Dual-feed daily scoreboard orchestrator (SCOREBOARD-01 — operator
//! directive 2026-07-10: run Dhan + Groww live for a month, *"all tracked,
//! captured, visualized, logged, monitored, 100% automated"* + *"ensure and
//! CAPTURE that the issue really arose from the broker side"*).
//!
//! Two entry points, spawned from `main.rs` on BOTH boot paths — the slow
//! process-global prefix AND the FAST crash-recovery arm (hostile review
//! 2026-07-10: the fast arm is the DOMINANT mid-market restart channel and
//! `return`s before the slow-path spawn) — gated on `[scoreboard] enabled`:
//!
//! 1. [`reconcile_process_death_episodes`] — ONCE per boot (first query
//!    delayed a few minutes so this boot's `connected` audit rows land,
//!    then POLLING per-key until every pairing candidate's own post-boot
//!    up row is visible): a dying process writes NO disconnect row, so the
//!    BOOTING process is its own correlation evidence. For each
//!    `(feed, ws_type, connection_index)` whose last PRE-boot
//!    `ws_event_audit` row was an "up" kind, whose first POST-boot up-kind
//!    row exists, and whose DEATH WINDOW `[prior_ts, connect_ts]` overlaps
//!    the session (round-2 gate — the normal-day prior row is the ~08:34
//!    PRE-market connect), synthesize ONE `process_death` episode at the
//!    deterministic post-boot up ts (DEDUP-idempotent across repeated boots),
//!    blame ALWAYS `ours` with the deploy-vs-crash sub-reason
//!    (`build_info::BUILD_GIT_SHA` vs the SSM
//!    `/tickvault/<env>/deploy/binary-git-sha` control-plane param,
//!    fail-soft to `process_restart`).
//! 2. [`run_feed_scoreboard`] — the 15:45 IST daily aggregation
//!    (`tick_conservation_boot` idiom: pure decide fn with the RunCatchUp
//!    late-boot variant + the `TICKVAULT_SCOREBOARD_NOW` operator override
//!    (+ `TICKVAULT_SCOREBOARD_DATE=YYYY-MM-DD` past-day backfill) + the
//!    trading-day gate). Classifies today's disconnect episodes from
//!    `ws_event_audit` (+ the same-day errors.jsonl correlation scan),
//!    UPSERTs `feed_episode_audit` and tallies them IN MEMORY (the
//!    read-back merges only the long-visible boot-reconciled rows — never
//!    its own just-flushed rows), aggregates per-feed coverage from the
//!    `ticks` partition, writes the two `feed_scoreboard_daily` rows, and
//!    returns the summary the Telegram scorecard is built from.
//!
//! Everything is cold-path + fail-soft: a missing source records the `-1`
//! sentinel and an honest `partial`/`degraded` outcome — never fabricated
//! zeros (audit Rule 11), never a blocked boot, never a touched hot path.
//! O(N) legs (the day's audit rows, the ≤375-entry minute sets, the ≤48h
//! errors.jsonl scan) are flagged O(N), cold, once per day — never claimed
//! O(1).
//!
//! Runbooks: `.claude/rules/project/dual-feed-scoreboard-error-codes.md`
//! (triage) + `docs/runbooks/dual-feed-scoreboard.md` (month-end verdict).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed_blame::{
    EPISODE_KIND_DISCONNECT, EPISODE_KIND_NEVER_STREAMED_RESTART,
    EPISODE_KIND_OFF_HOURS_DISCONNECT, EPISODE_KIND_PROCESS_DEATH, EPISODE_KIND_STALL_RESTART,
    EpisodeEvidence, classify_episode,
};
use tickvault_storage::feed_episode_audit_persistence::{
    FeedEpisodeAuditRow, FeedEpisodeAuditWriter, ensure_feed_episode_audit_table,
};
use tickvault_storage::feed_scoreboard_persistence::{
    CoverageSource, FeedScoreboardDailyRow, FeedScoreboardWriter, LAG_FLOOR_MS_DHAN,
    LAG_FLOOR_MS_GROWW, SCOREBOARD_SESSION_MINUTES, SCOREBOARD_UNAVAILABLE_SENTINEL,
    ScoreboardOutcome, ensure_feed_scoreboard_tables,
};

use crate::tick_conservation_boot::parse_questdb_count;

/// IST seconds-of-day of the DETERMINISTIC daily-row timestamp (15:45:00).
/// This stamps `feed_scoreboard_daily.ts` regardless of when the run
/// actually fired (catch-up / `TICKVAULT_SCOREBOARD_NOW` backfill), so
/// re-runs UPSERT the same row (DEDUP idempotency by construction).
pub const SCOREBOARD_ROW_TS_SECS_OF_DAY_IST: i64 = 15 * 3600 + 45 * 60; // 56_700

/// NSE regular session bounds in IST seconds-of-day ([09:15, 15:30)).
pub const SESSION_START_SECS_OF_DAY_IST: i64 = 9 * 3600 + 15 * 60; // 33_300
pub const SESSION_END_SECS_OF_DAY_IST: i64 = 15 * 3600 + 30 * 60; // 55_800

/// Market-hours window used for the "boot occurred in-session" gate of the
/// process-death reconciler ([09:00, 15:30) IST — the ws_event_audit
/// `market_hours` convention).
pub const MARKET_HOURS_START_SECS_OF_DAY_IST: u32 = 9 * 3600;
pub const MARKET_HOURS_END_SECS_OF_DAY_IST: u32 = 15 * 3600 + 30 * 60;

/// WS-GAP-09 corroboration window around a disconnect episode (±120s).
pub const WS_GAP9_OVERLAP_WINDOW_SECS: i64 = 120;

/// PROC-01 / RESOURCE-01..03 corroboration window (±300s).
pub const RESOURCE_OVERLAP_WINDOW_SECS: i64 = 300;

/// How long after boot the process-death reconciler waits before its FIRST
/// query so this boot's own `connected` audit rows have landed in QuestDB
/// (the async audit writer + ILP flush).
pub const PROCESS_DEATH_RECONCILE_DELAY_SECS: u64 = 180;

/// Poll cadence + attempt bound for the reconciler (hostile review
/// 2026-07-10): a fast crash-recovery boot inside a Dhan 429 window waits
/// out the persisted WS-GAP-08 cooldown of up to 300s BEFORE
/// `create_websocket_pool`, so this boot's first `connected` row can land
/// well after the 180s first query — exactly the restart-storm case the
/// process-death detection exists for. The reconciler therefore POLLS
/// (DEDUP-idempotent synthesis makes repeats safe by construction) until a
/// post-boot connect row appears or the attempt budget is spent:
/// 180s + 9 × 60s ≈ 12.2 min total window, comfortably past the 300s
/// cooldown + connection stagger + audit-flush latency (≥ 420s worst case).
pub const PROCESS_DEATH_RECONCILE_POLL_INTERVAL_SECS: u64 = 60;
pub const PROCESS_DEATH_RECONCILE_MAX_ATTEMPTS: u32 = 10;

/// Bounded flush retry for the boot reconciler's synthesized rows (round-2
/// hostile review 2026-07-10): a QuestDB blip during the reconciler's flush
/// window loses this boot's death episodes FOREVER (only the boot
/// reconciler pairs prior-up → first-connect; no re-run re-creates them),
/// so the flush retries in place a few times before giving up loudly.
pub const RECONCILE_FLUSH_RETRY_ATTEMPTS: u32 = 3;
pub const RECONCILE_FLUSH_RETRY_DELAY_SECS: u64 = 60;

/// HTTP timeout for every QuestDB `/exec` read in this module.
const SCOREBOARD_HTTP_TIMEOUT_SECS: u64 = 10;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Decision for WHEN the daily scoreboard should fire. Mirrors
/// `tick_conservation_boot::ConservationStart` (the RunCatchUp variant is
/// the audit-fix-#2 idiom: a late trading-day boot runs once immediately —
/// day-1 / backfill friendly — instead of skipping the day).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreboardStart {
    /// Not a trading day and not forced → do not run.
    SkipNonTradingDay,
    /// Past the trigger on a trading-day boot → run once, immediately.
    RunCatchUp,
    /// Operator forced an on-demand run (`TICKVAULT_SCOREBOARD_NOW`).
    RunNow,
    /// Sleep this many seconds, then run at the trigger.
    SleepThenRun(u64),
}

/// Pure decision: when should the daily scoreboard fire. `trigger` is the
/// config-driven IST seconds-of-day (`[scoreboard] trigger_secs_of_day_ist`,
/// default 15:45:00).
#[must_use]
pub fn decide_scoreboard_start(
    now_secs_of_day_ist: u32,
    is_trading_day: bool,
    force_now: bool,
    trigger_secs_of_day_ist: u32,
) -> ScoreboardStart {
    if force_now {
        return ScoreboardStart::RunNow;
    }
    if !is_trading_day {
        return ScoreboardStart::SkipNonTradingDay;
    }
    if now_secs_of_day_ist >= trigger_secs_of_day_ist {
        return ScoreboardStart::RunCatchUp;
    }
    ScoreboardStart::SleepThenRun(u64::from(trigger_secs_of_day_ist - now_secs_of_day_ist))
}

/// `true` when `secs_of_day` is inside the market-hours window the
/// reconciler gates on ([09:00, 15:30) IST). Pure.
#[must_use]
pub fn is_in_market_hours_secs(secs_of_day_ist: u32) -> bool {
    (MARKET_HOURS_START_SECS_OF_DAY_IST..MARKET_HOURS_END_SECS_OF_DAY_IST)
        .contains(&secs_of_day_ist)
}

/// Parse + validate the `TICKVAULT_SCOREBOARD_DATE=YYYY-MM-DD` past-day
/// backfill override (design contract §5: `TICKVAULT_SCOREBOARD_NOW` "+
/// optional date arg for past-day backfill"). Returns `(ist_day_number,
/// label)`.
///
/// STRICT fail-closed validation (the `instrument_snapshot::
/// is_valid_trading_date` discipline): exactly `YYYY-MM-DD`, digits only,
/// a real calendar date, not before the 1970 epoch — anything else yields
/// `None` and the caller REFUSES the run loudly rather than silently
/// aggregating the wrong day.
#[must_use]
pub fn parse_scoreboard_date_override(raw: &str) -> Option<(u64, String)> {
    let raw = raw.trim();
    if raw.len() != 10 {
        return None;
    }
    let ok_shape = raw.chars().enumerate().all(|(i, c)| {
        if i == 4 || i == 7 {
            c == '-'
        } else {
            c.is_ascii_digit()
        }
    });
    if !ok_shape {
        return None;
    }
    let date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?;
    let days = date.signed_duration_since(epoch).num_days();
    u64::try_from(days).ok().map(|d| (d, raw.to_string()))
}

/// Semantic validation of a `TICKVAULT_SCOREBOARD_DATE` backfill target
/// (round-2 hostile review 2026-07-10): a well-SHAPED but non-trading or
/// future date used to run the full aggregation against empty sources —
/// every count legitimately 0 — and write two fabricated all-zero rows
/// stamped `outcome='complete'` (the audit-Rule-11 false-OK class),
/// polluting the month-end aggregates with no distinguishing signal. The
/// gate applies to the TARGET date (the run day's trading-day check is
/// correctly bypassed by `force_now` for weekend backfills of past trading
/// days). Pure; the caller supplies the calendar verdict.
///
/// # Errors
/// A human-readable reason when the target must be REFUSED (the caller
/// pages `DualFeedScorecardAborted`, matching the malformed-date arm).
pub fn validate_scoreboard_backfill_date(
    target_ist_day: u64,
    today_ist_day: u64,
    target_is_trading_day: bool,
) -> Result<(), String> {
    if target_ist_day > today_ist_day {
        return Err("is in the future — nothing can have been recorded for it".to_string());
    }
    if !target_is_trading_day {
        return Err(
            "is not a trading day — an all-zero row would fabricate a measured day".to_string(),
        );
    }
    Ok(())
}

/// Validate `[scoreboard] trigger_secs_of_day_ist` at spawn (round-2
/// hostile review 2026-07-10): a typo'd value ≥ 86400 sleeps past the
/// 16:30 auto-stop every day — the task is cancelled at shutdown (silent
/// teardown by design), so NO row, NO Telegram and NO Aborted page ever
/// fires; a tiny value turns every 08:31 boot into an empty-morning
/// catch-up whose all-zero rows own the day. Legal range:
/// [session end (15:30) .. 23:59:59] IST. Out-of-range falls back to the
/// 15:45 default; returns `(effective_trigger, was_invalid)` so the caller
/// logs SCOREBOARD-01 loudly. Pure.
#[must_use]
pub fn sanitize_scoreboard_trigger(configured_secs_of_day_ist: u32) -> (u32, bool) {
    // APPROVED: SESSION_END_SECS_OF_DAY_IST = 55_800 fits u32 trivially.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = SESSION_END_SECS_OF_DAY_IST as u32;
    if (lo..86_400).contains(&configured_secs_of_day_ist) {
        (configured_secs_of_day_ist, false)
    } else {
        // APPROVED: the deterministic-row constant (15:45:00 = 56_700).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (SCOREBOARD_ROW_TS_SECS_OF_DAY_IST as u32, true)
        }
    }
}

// ---------------------------------------------------------------------------
// SQL builders (pure — every literal is a compile-time constant or an i64;
// no user input reaches the SQL, so there is no injection surface)
//
// REGRESSION LOCK (hostile review 2026-07-10, CRITICAL — empirically
// confirmed on the pinned QuestDB 9.3.5): a bare integer literal compared
// against a TIMESTAMP column is interpreted as epoch **MICROSECONDS**, not
// nanoseconds. A nanosecond literal (`ts >= 1_783_996_200_000_000_000`)
// silently matches ZERO rows — every window query would return empty and
// the scoreboard would stamp fabricated all-zero "complete" days (the exact
// audit-Rule-11 false-OK class). Every embedded window bound below MUST
// therefore come from `day_bounds_micros` (nanos ÷ 1_000). The in-memory
// side (parsers / correlation / episode rows) stays in nanos — only the SQL
// literals are micros. Pinned by the per-builder digit-magnitude tests
// (a 2026 date's micros bound has 16 digits; the broken nanos bound has 19).
// ---------------------------------------------------------------------------

const MICROS_PER_SEC: i64 = 1_000_000;

fn day_bounds_nanos(target_ist_day: u64) -> (i64, i64) {
    // APPROVED: IST day numbers are ~20K, far below i64::MAX — saturating
    // keeps adversarial inputs bounded.
    let start = i64::try_from(target_ist_day)
        .unwrap_or(0)
        .saturating_mul(86_400)
        .saturating_mul(NANOS_PER_SEC);
    (start, start.saturating_add(86_400 * NANOS_PER_SEC))
}

/// IST-day bounds in epoch MICROSECONDS — the ONLY representation legal in
/// an embedded QuestDB TIMESTAMP comparison literal (see the regression
/// lock above).
fn day_bounds_micros(target_ist_day: u64) -> (i64, i64) {
    // APPROVED: IST day numbers are ~20K, far below i64::MAX — saturating
    // keeps adversarial inputs bounded.
    let start = i64::try_from(target_ist_day)
        .unwrap_or(0)
        .saturating_mul(86_400)
        .saturating_mul(MICROS_PER_SEC);
    (start, start.saturating_add(86_400 * MICROS_PER_SEC))
}

/// The day's `ws_event_audit` rows in ts order. `cast(ts as long)` yields
/// IST-epoch MICROseconds (`ts` stores IST wall-clock — data-integrity.md);
/// the window literals are micros too (QuestDB TIMESTAMP-comparison
/// semantics — regression lock above).
#[must_use]
pub fn build_ws_events_day_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select cast(ts as long), feed, ws_type, connection_index, event_kind, \
         source, dhan_code, down_secs, market_hours \
         from ws_event_audit where ts >= {start} and ts < {end} order by ts"
    )
}

/// The day's classified episode rows — ALL detectors (the ws-read-failed
/// fallback path only; the race-free primary path tallies in memory and
/// merges [`build_boot_reconciled_episode_day_sql`]). Micros literals.
/// `ws_type` rides along so the aggregate can exclude non-market-data
/// channels from the headline tallies (hostile review round 2, 2026-07-10);
/// `connection_index` + `cast(ts as long)` (micros) ride along so
/// [`fold_episode_readback_rows`] can DEDUPE the read-back against this
/// boot's in-memory rows (round 3, 2026-07-10).
#[must_use]
pub fn build_episode_day_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select feed, episode_kind, blame, market_hours, ws_type, \
         connection_index, cast(ts as long) \
         from feed_episode_audit where ts >= {start} and ts < {end}"
    )
}

/// The day's `detector='boot_reconciled'` process-death rows ONLY. The
/// daily run merges these into its IN-MEMORY step-2 tallies instead of
/// reading back its own just-flushed rows (hostile review 2026-07-10:
/// ILP-HTTP ACK = committed-to-WAL, NOT visible-to-SELECT — a same-run
/// read-back can silently miss every episode just written).
///
/// ROUND-3 CORRECTION (2026-07-10): boot-reconciled rows are NOT "long
/// visible" on the RunCatchUp/RunNow paths — Task 2 awaits the reconciler
/// and runs SECONDS after its ILP-HTTP flush, inside the same WAL-apply
/// window. This read-back therefore covers only EARLIER boots' rows
/// (multi-crash days); THIS boot's just-synthesized rows are threaded
/// in-memory from `reconcile_process_death_episodes` and DEDUPED against
/// this read-back by `(feed, ws_type, connection_index, ts)` — see
/// [`fold_episode_readback_rows`] / [`boot_reconciled_skip_keys`].
/// Micros literals. `ws_type` rides along for the market-data headline
/// filter (one process death synthesizes one row per up-key — without the
/// filter a single crash rendered "App restarts detected: Dhan 2 | Groww 1"
/// via the order-update key); `connection_index` + `cast(ts as long)`
/// (micros) ride along for the dedupe key.
#[must_use]
pub fn build_boot_reconciled_episode_day_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select feed, episode_kind, blame, market_hours, ws_type, \
         connection_index, cast(ts as long) \
         from feed_episode_audit where ts >= {start} and ts < {end} \
         and detector = 'boot_reconciled'"
    )
}

/// The day's existing episode rows' PARTIALITY view — the keep-better
/// guard input (round 3, 2026-07-10): before the step-2 UPSERT, a re-run
/// reads the target day's existing `feed_episode_audit` rows so an
/// evidence-less re-classification (`run_partial=true`, e.g. a >48h
/// backfill after the errors.jsonl correlation horizon expired) can never
/// destructively overwrite an evidence-backed row (`run_partial=false`) —
/// DEDUP is last-write-wins on QuestDB, so without this read the re-run
/// silently replaced `ours/dual_instance` verdicts with
/// `broker/rate_limit_805` defaults. `blame` + `market_hours` ride along
/// so a suppressed key still folds the EXISTING row's verdict into the
/// in-memory tallies. Micros literals.
#[must_use]
pub fn build_existing_episode_partiality_day_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select feed, ws_type, connection_index, episode_kind, \
         cast(ts as long), run_partial, blame, market_hours \
         from feed_episode_audit where ts >= {start} and ts < {end}"
    )
}

/// Ticks a feed delivered today (feed-filtered + day-windowed). LOCAL
/// builder with MICROS literals — deliberately NOT the
/// `tick_conservation_boot::build_conservation_ticks_count_sql` reuse: that
/// shipped builder embeds NANOS literals (the same silent-zero bug class,
/// pre-existing on main — reported for a separate fix PR).
#[must_use]
pub fn build_scoreboard_ticks_count_sql(feed: &str, target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select count() from ticks where feed = '{feed}' \
         and ts >= {start} and ts < {end}"
    )
}

/// Distinct `(security_id, segment)` pairs a feed delivered today. The
/// distinct is segment-qualified per I-P1-11 (`security_id` alone is NOT
/// unique — Dhan reuses ids across segments). Micros literals.
#[must_use]
pub fn build_feed_instruments_count_sql(feed: &str, target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select count() from (select distinct security_id, segment from ticks \
         where feed = '{feed}' and ts >= {start} and ts < {end})"
    )
}

/// Distinct session minutes ([09:15, 15:30) IST) a feed delivered any tick
/// in. ≤375 rows; the minute values are opaque keys compared in Rust.
/// Micros literals.
#[must_use]
pub fn build_feed_session_minutes_sql(feed: &str, target_ist_day: u64) -> String {
    let (day_start, _) = day_bounds_micros(target_ist_day);
    let sess_start = day_start.saturating_add(SESSION_START_SECS_OF_DAY_IST * MICROS_PER_SEC);
    let sess_end = day_start.saturating_add(SESSION_END_SECS_OF_DAY_IST * MICROS_PER_SEC);
    format!(
        "select distinct date_trunc('minute', ts) from ticks \
         where feed = '{feed}' and ts >= {sess_start} and ts < {sess_end}"
    )
}

// ---------------------------------------------------------------------------
// /exec response parsers (pure, fail-to-None — the caller records sentinels)
// ---------------------------------------------------------------------------

/// One `ws_event_audit` row, as read back for classification.
#[derive(Debug, Clone, PartialEq)]
pub struct WsAuditEventLite {
    pub ts_ist_nanos: i64,
    pub feed: String,
    pub ws_type: String,
    pub connection_index: i64,
    pub event_kind: String,
    pub source: String,
    pub dhan_code: i64,
    pub down_secs: i64,
    pub market_hours: bool,
}

/// Extract the `/exec` dataset rows. `None` on any shape mismatch.
fn parse_dataset(body: &str) -> Option<Vec<serde_json::Value>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(v.get("dataset")?.as_array()?.clone())
}

/// Parse the [`build_ws_events_day_sql`] response. Pure. Rows with an
/// unexpected shape are SKIPPED (counted by the caller as best-effort) —
/// never a panic.
#[must_use]
pub fn parse_ws_events(body: &str) -> Option<Vec<WsAuditEventLite>> {
    let rows = parse_dataset(body)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let cols = match row.as_array() {
            Some(c) if c.len() >= 9 => c,
            _ => continue,
        };
        let Some(ts_micros) = cols[0].as_i64() else {
            continue;
        };
        out.push(WsAuditEventLite {
            ts_ist_nanos: ts_micros.saturating_mul(1_000),
            feed: cols[1].as_str().unwrap_or("").to_string(),
            ws_type: cols[2].as_str().unwrap_or("").to_string(),
            connection_index: cols[3].as_i64().unwrap_or(0),
            event_kind: cols[4].as_str().unwrap_or("").to_string(),
            source: cols[5].as_str().unwrap_or("").to_string(),
            dhan_code: cols[6].as_i64().unwrap_or(-1),
            down_secs: cols[7].as_i64().unwrap_or(0),
            market_hours: cols[8].as_bool().unwrap_or(false),
        });
    }
    Some(out)
}

/// Parse a distinct-minutes response into an opaque minute-key set. Pure.
#[must_use]
pub fn parse_minute_set(body: &str) -> Option<HashSet<String>> {
    let rows = parse_dataset(body)?;
    let mut out = HashSet::with_capacity(rows.len());
    for row in rows {
        if let Some(first) = row.as_array().and_then(|c| c.first())
            && let Some(s) = first.as_str()
        {
            out.insert(s.to_string());
        }
    }
    Some(out)
}

/// Feed-level minute overlap: `(a_only, b_only, both)`. Pure, O(minutes ≤ 375).
#[must_use]
pub fn compute_minute_overlap(a: &HashSet<String>, b: &HashSet<String>) -> (i64, i64, i64) {
    let both = a.intersection(b).count();
    let to_i64 = |v: usize| i64::try_from(v).unwrap_or(i64::MAX);
    (
        to_i64(a.len().saturating_sub(both)),
        to_i64(b.len().saturating_sub(both)),
        to_i64(both),
    )
}

/// Per-feed episode tally from the day's `feed_episode_audit` rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EpisodeTally {
    pub disconnects_market: i64,
    pub disconnects_off_hours: i64,
    pub stalls: i64,
    pub restarts: i64,
    /// Blame tallies over the headline episodes (off-hours rows excluded —
    /// they are expected idle-cleanup noise, contract §4).
    pub blame_broker: i64,
    pub blame_ours: i64,
    pub blame_indeterminate: i64,
}

/// Fold ONE episode into a per-feed tally. Pure — the SINGLE tally rule
/// shared by the SQL read-back aggregate AND the race-free in-memory path
/// (so the two can never diverge on what counts as headline/off-hours).
pub fn fold_episode_into_tally(t: &mut EpisodeTally, kind: &str, blame: &str, market_hours: bool) {
    let mut headline = true;
    match kind {
        EPISODE_KIND_DISCONNECT => {
            if market_hours {
                t.disconnects_market += 1;
            } else {
                // A 'disconnect' row stamped off-market (edge second) —
                // still counted off-hours, not in the headline.
                t.disconnects_off_hours += 1;
                headline = false;
            }
        }
        EPISODE_KIND_OFF_HOURS_DISCONNECT => {
            t.disconnects_off_hours += 1;
            headline = false;
        }
        EPISODE_KIND_STALL_RESTART | EPISODE_KIND_NEVER_STREAMED_RESTART => t.stalls += 1,
        // A process_death stamped OFF-market is the round-3 post-close
        // restart shape (`blame_reason = post_close_restart`): the connect
        // landed AFTER session close, so the death is indistinguishable
        // from the clean scheduled 16:30 stop (a SIGTERM teardown persists
        // no disconnect row) — the row exists forensically but must never
        // vote in the headline restarts/blame tallies (hostile review
        // round 3, 2026-07-10: every same-day post-close stop → manual
        // evening start synthesized a phantom in-market death and
        // re-wrote the day's completed scorecard).
        EPISODE_KIND_PROCESS_DEATH => {
            if market_hours {
                t.restarts += 1;
            } else {
                headline = false;
            }
        }
        // An UNKNOWN kind (a future/PR-2+ kind read back before this fold
        // learns it) counts in NO count column, so it must not vote in the
        // blame split either — otherwise blame sums exceed the sum of the
        // visible incident columns and the card's "Who caused today's
        // incidents" line is unreconcilable against its own counts
        // (hostile review round 2, 2026-07-10).
        _ => headline = false,
    }
    if headline {
        match blame {
            "broker" => t.blame_broker += 1,
            "ours" => t.blame_ours += 1,
            _ => t.blame_indeterminate += 1,
        }
    }
}

/// `true` for the MARKET-DATA WebSocket channels — the ONLY ws_types whose
/// episodes belong in the headline "which broker's FEED is worse" tallies
/// (allowlist: `main_feed` = the Dhan market-data conn, `groww_bridge` =
/// the Groww feed). The Dhan ORDER-UPDATE WS is a trading-channel socket:
/// it cycles clean-closes on idle days and produced 39+ in-market
/// `disconnected` rows in the verified 2026-07-06 dead-token incident while
/// the market-data feed was perfect — folding it into Dhan's drop count
/// compares structurally different populations against single-channel
/// Groww (hostile review round 2, 2026-07-10). Episode ROWS for every
/// ws_type are still persisted for forensics; only the headline tallies
/// filter.
#[must_use]
pub fn is_market_data_ws_type(ws_type: &str) -> bool {
    matches!(ws_type, "main_feed" | "groww_bridge")
}

/// Fold ONE persisted episode row into the per-feed HEADLINE tallies —
/// applying the [`is_market_data_ws_type`] filter. Pure; the single gate
/// shared by the in-memory step-2 path (the SQL read-back paths apply the
/// same filter inside [`fold_episode_readback_rows`]).
pub fn fold_market_data_episode(
    tallies: &mut BTreeMap<String, EpisodeTally>,
    row: &FeedEpisodeAuditRow,
) {
    if !is_market_data_ws_type(&row.ws_type) {
        return;
    }
    fold_episode_into_tally(
        tallies.entry(row.feed.clone()).or_default(),
        row.episode_kind,
        row.blame.as_str(),
        row.market_hours,
    );
}

/// Dedupe key for an episode read-back row vs an in-memory row:
/// `(feed, ws_type, connection_index, ts_micros)` — the row-identity subset
/// of the DEDUP UPSERT key that varies within one day's rows (the SQL side
/// exposes `ts` as `cast(ts as long)` micros; the in-memory side divides
/// its nanos by 1000).
pub type EpisodeRowKey = (String, String, i64, i64);

/// The dedupe keys of THIS boot's in-memory synthesized rows — the SELECT
/// read-back skips these so an already-WAL-applied (visible) copy of a row
/// the run also folds in-memory is never double-counted (round 3,
/// 2026-07-10). Pure.
#[must_use]
pub fn boot_reconciled_skip_keys(rows: &[FeedEpisodeAuditRow]) -> HashSet<EpisodeRowKey> {
    rows.iter()
        .map(|r| {
            (
                r.feed.clone(),
                r.ws_type.clone(),
                r.connection_index,
                r.ts_ist_nanos.div_euclid(1_000),
            )
        })
        .collect()
}

/// Fold a [`build_episode_day_sql`] / [`build_boot_reconciled_episode_day_sql`]
/// response into per-feed tallies, SKIPPING any row whose
/// `(feed, ws_type, connection_index, ts)` key is in `skip_keys` (this
/// boot's in-memory rows — folded separately, race-free). Pure. Returns the
/// number of rows folded; `None` = the body itself was unparsable.
/// Non-market-data ws_types (the order-update trading channel) are SKIPPED
/// from the headline tallies (see [`is_market_data_ws_type`]).
///
/// Rows from an OLDER 5-column body shape (no key columns) still fold —
/// they simply cannot be deduped (the SQL builders + their tests pin the
/// 7-column shape, so this arm is defensive only).
#[must_use]
pub fn fold_episode_readback_rows(
    out: &mut BTreeMap<String, EpisodeTally>,
    body: &str,
    skip_keys: &HashSet<EpisodeRowKey>,
) -> Option<usize> {
    let rows = parse_dataset(body)?;
    let mut folded = 0_usize;
    for row in rows {
        let cols = match row.as_array() {
            Some(c) if c.len() >= 5 => c,
            _ => continue,
        };
        let feed = cols[0].as_str().unwrap_or("").to_string();
        let kind = cols[1].as_str().unwrap_or("");
        let blame = cols[2].as_str().unwrap_or("");
        let market_hours = cols[3].as_bool().unwrap_or(false);
        let ws_type = cols[4].as_str().unwrap_or("");
        if cols.len() >= 7
            && let (Some(conn_idx), Some(ts_micros)) = (cols[5].as_i64(), cols[6].as_i64())
            && skip_keys.contains(&(feed.clone(), ws_type.to_string(), conn_idx, ts_micros))
        {
            // Already folded in-memory this run — never double-count.
            continue;
        }
        if !is_market_data_ws_type(ws_type) {
            continue;
        }
        fold_episode_into_tally(out.entry(feed).or_default(), kind, blame, market_hours);
        folded += 1;
    }
    Some(folded)
}

/// One existing episode row's keep-better view (round 3, 2026-07-10) —
/// what the step-2 UPSERT consults before overwriting.
#[derive(Debug, Clone, PartialEq)]
pub struct ExistingEpisodeView {
    /// The persisted `run_partial` flag (false = evidence-backed).
    pub run_partial: bool,
    /// The persisted blame verdict label (folded when keep-better keeps it).
    pub blame: String,
    /// The persisted market-hours flag (folded when keep-better keeps it).
    pub market_hours: bool,
}

/// Full-DEDUP-collision key for the keep-better map: within one day the
/// `trading_date_ist` component is constant, so
/// `(feed, ws_type, connection_index, episode_kind, ts_micros)` identifies
/// exactly the row a new UPSERT would replace.
pub type ExistingEpisodeKey = (String, String, i64, String, i64);

/// Parse the [`build_existing_episode_partiality_day_sql`] response into
/// the keep-better map. Pure; `None` = unparsable body (the caller logs +
/// proceeds without the guard, matching pre-round-3 behavior).
#[must_use]
pub fn parse_existing_episode_partiality(
    body: &str,
) -> Option<HashMap<ExistingEpisodeKey, ExistingEpisodeView>> {
    let rows = parse_dataset(body)?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let cols = match row.as_array() {
            Some(c) if c.len() >= 8 => c,
            _ => continue,
        };
        let (Some(conn_idx), Some(ts_micros)) = (cols[2].as_i64(), cols[4].as_i64()) else {
            continue;
        };
        out.insert(
            (
                cols[0].as_str().unwrap_or("").to_string(),
                cols[1].as_str().unwrap_or("").to_string(),
                conn_idx,
                cols[3].as_str().unwrap_or("").to_string(),
                ts_micros,
            ),
            ExistingEpisodeView {
                run_partial: cols[5].as_bool().unwrap_or(true),
                blame: cols[6].as_str().unwrap_or("").to_string(),
                market_hours: cols[7].as_bool().unwrap_or(false),
            },
        );
    }
    Some(out)
}

/// Keep-better rule (round 3, 2026-07-10): suppress the UPSERT when it
/// would DOWNGRADE an evidence-backed row (`run_partial=false`) with an
/// evidence-less re-classification (`run_partial=true`) — the >48h-stale
/// backfill class where the errors.jsonl correlation horizon expired and
/// an `ours/dual_instance` 805 would silently flip to
/// `broker/rate_limit_805`. Every other combination proceeds (same or
/// better evidence overwrites in place — the DEDUP re-run stays
/// VALUE-idempotent there). Pure.
#[must_use]
pub fn should_suppress_episode_overwrite(
    existing_run_partial: Option<bool>,
    new_run_partial: bool,
) -> bool {
    existing_run_partial == Some(false) && new_run_partial
}

/// Sum EVERY series of a counter across its label sets (the bare name AND
/// `name{labels}` lines) from a Prometheus exposition body. Pure. `None`
/// when the metric is entirely absent (never silently zero).
#[must_use]
pub fn parse_prom_counter_sum(body: &str, name: &str) -> Option<u64> {
    let mut found = false;
    let mut sum = 0_u64;
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(metric), Some(raw)) = (parts.next(), parts.next()) else {
            continue;
        };
        let matches_name =
            metric == name || (metric.starts_with(name) && metric[name.len()..].starts_with('{'));
        if !matches_name {
            continue;
        }
        let Ok(v) = raw.parse::<f64>() else { continue };
        if !v.is_finite() || v < 0.0 {
            continue;
        }
        found = true;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // APPROVED: counter values are non-negative finite; u64 truncation
        // of an exact integral float is the intended conversion.
        {
            sum = sum.saturating_add(v as u64);
        }
    }
    found.then_some(sum)
}

// ---------------------------------------------------------------------------
// errors.jsonl correlation scan (blame corroboration evidence)
// ---------------------------------------------------------------------------

/// One parsed errors.jsonl event of interest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonlCodeEvent {
    pub code: String,
    pub reason: Option<String>,
    pub ts_ist_nanos: i64,
}

/// Same-day corroboration evidence for the blame classifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorrelationEvidence {
    /// A RESILIENCE-01/03 (dual-instance) line landed this trading day.
    pub resilience_same_day: bool,
    /// WS-GAP-09 `bare_dhan_reset` line timestamps (IST nanos).
    pub ws_gap9_bare_ts_nanos: Vec<i64>,
    /// WS-GAP-09 `in_window_429_ride_out` line timestamps (IST nanos).
    pub ws_gap9_429_ts_nanos: Vec<i64>,
    /// PROC-01 / RESOURCE-01..03 line timestamps (IST nanos).
    pub resource_ts_nanos: Vec<i64>,
    /// `false` when the scan could not read the log directory (aged-out
    /// backfill day / missing dir) — episodes classified from partial
    /// evidence are stamped `run_partial` (runbook note; 805 defaults
    /// broker, RSTs default indeterminate).
    pub scan_complete: bool,
}

/// Parse one errors.jsonl line (`flatten_event(true)` hoists `code` /
/// `reason` to the top level; `timestamp` is RFC3339 UTC). Pure; `None`
/// for anything unparsable — never a panic.
#[must_use]
pub fn parse_errors_jsonl_line(line: &str) -> Option<JsonlCodeEvent> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let code = v.get("code")?.as_str()?.to_string();
    let reason = v
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let ts = v.get("timestamp")?.as_str()?;
    let parsed = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    let utc_nanos = parsed.timestamp_nanos_opt()?;
    Some(JsonlCodeEvent {
        code,
        reason,
        ts_ist_nanos: utc_nanos.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
    })
}

/// Fold parsed events into day-scoped [`CorrelationEvidence`]. Pure.
#[must_use]
pub fn collect_correlation_evidence(
    events: &[JsonlCodeEvent],
    target_ist_day: u64,
    scan_complete: bool,
) -> CorrelationEvidence {
    let (day_start, day_end) = day_bounds_nanos(target_ist_day);
    let mut out = CorrelationEvidence {
        scan_complete,
        ..CorrelationEvidence::default()
    };
    for ev in events {
        if ev.ts_ist_nanos < day_start || ev.ts_ist_nanos >= day_end {
            continue;
        }
        match ev.code.as_str() {
            "RESILIENCE-01" | "RESILIENCE-03" => out.resilience_same_day = true,
            "WS-GAP-09" => match ev.reason.as_deref() {
                Some("bare_dhan_reset") => out.ws_gap9_bare_ts_nanos.push(ev.ts_ist_nanos),
                Some("in_window_429_ride_out") => out.ws_gap9_429_ts_nanos.push(ev.ts_ist_nanos),
                _ => {}
            },
            "PROC-01" | "RESOURCE-01" | "RESOURCE-02" | "RESOURCE-03" => {
                out.resource_ts_nanos.push(ev.ts_ist_nanos);
            }
            _ => {}
        }
    }
    out
}

/// `true` when any timestamp in `ts_list` is within ±`window_secs` of
/// `episode_ts_nanos`. Pure, O(list).
#[must_use]
pub fn has_overlap(ts_list: &[i64], episode_ts_nanos: i64, window_secs: i64) -> bool {
    let window_nanos = window_secs.saturating_mul(NANOS_PER_SEC);
    ts_list
        .iter()
        .any(|&t| (t - episode_ts_nanos).abs() <= window_nanos)
}

/// The ONLY codes the correlation scan retains. Filtering at PARSE time
/// bounds the transient allocation: an error-storm day of the 2026-07-03
/// class carries ~1M coded ERROR lines in the retained 48h window —
/// buffering them all before filtering was a ~100MB+ transient spike on
/// exactly the degraded days the scoreboard analyzes (hostile review
/// round 2, 2026-07-10). Must stay in sync with the match arms of
/// [`collect_correlation_evidence`].
pub const CORRELATION_CODES: [&str; 7] = [
    "RESILIENCE-01",
    "RESILIENCE-03",
    "WS-GAP-09",
    "PROC-01",
    "RESOURCE-01",
    "RESOURCE-02",
    "RESOURCE-03",
];

/// Scan the errors.jsonl directory (blocking file I/O — callers wrap in
/// `spawn_blocking`). Missing dir / unreadable files degrade to
/// `scan_complete = false` evidence — fail-soft, never a panic. O(≤48h of
/// retained hourly files), cold path, once per run; only
/// [`CORRELATION_CODES`] events are retained in memory.
#[must_use]
pub fn scan_errors_jsonl_for_correlation(dir: &Path, target_ist_day: u64) -> CorrelationEvidence {
    use std::io::BufRead;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return collect_correlation_evidence(&[], target_ist_day, false);
    };
    let mut events: Vec<JsonlCodeEvent> = Vec::new();
    let mut complete = true;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("errors.jsonl") {
            continue;
        }
        // The bare `errors.jsonl` compat SYMLINK aliases the newest hourly
        // file in the same dir — reading it double-counts that hour
        // (harmless for the boolean/overlap outputs, wasted IO). Skip it.
        if name == "errors.jsonl" {
            continue;
        }
        match std::fs::File::open(&path) {
            Ok(f) => {
                for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Some(ev) = parse_errors_jsonl_line(&line)
                        && CORRELATION_CODES.contains(&ev.code.as_str())
                    {
                        events.push(ev);
                    }
                }
            }
            Err(_) => complete = false,
        }
    }
    collect_correlation_evidence(&events, target_ist_day, complete)
}

// ---------------------------------------------------------------------------
// Episode classification (ws_event_audit rows → feed_episode_audit rows)
// ---------------------------------------------------------------------------

/// Map a `ws_event_audit.event_kind` to a scoreboard episode kind. Pure.
/// Connect / reconnect / sleep kinds are lifecycle, not episodes → `None`.
#[must_use]
pub fn episode_kind_for_event(event_kind: &str) -> Option<&'static str> {
    match event_kind {
        "disconnected" => Some(EPISODE_KIND_DISCONNECT),
        "disconnected_off_hours" => Some(EPISODE_KIND_OFF_HOURS_DISCONNECT),
        _ => None,
    }
}

/// Classify one audit row into an episode row (or `None` for lifecycle
/// kinds). Pure — the ONE classification path for audit-sourced episodes.
#[must_use]
pub fn classify_ws_event_to_episode(
    ev: &WsAuditEventLite,
    corr: &CorrelationEvidence,
    trading_date_ist_nanos: i64,
) -> Option<FeedEpisodeAuditRow> {
    let episode_kind = episode_kind_for_event(&ev.event_kind)?;
    let evidence = EpisodeEvidence {
        episode_kind,
        feed: &ev.feed,
        source: &ev.source,
        dhan_code: ev.dhan_code,
        ws_gap9_bare_reset_overlap: has_overlap(
            &corr.ws_gap9_bare_ts_nanos,
            ev.ts_ist_nanos,
            WS_GAP9_OVERLAP_WINDOW_SECS,
        ),
        ws_gap9_429_overlap: has_overlap(
            &corr.ws_gap9_429_ts_nanos,
            ev.ts_ist_nanos,
            WS_GAP9_OVERLAP_WINDOW_SECS,
        ),
        resilience_peer_evidence: corr.resilience_same_day,
        resource_pressure_overlap: has_overlap(
            &corr.resource_ts_nanos,
            ev.ts_ist_nanos,
            RESOURCE_OVERLAP_WINDOW_SECS,
        ),
        stall_reason: "",
        build_sha_changed: None,
    };
    let (blame, blame_reason) = classify_episode(&evidence);
    Some(FeedEpisodeAuditRow {
        ts_ist_nanos: ev.ts_ist_nanos,
        trading_date_ist_nanos,
        feed: ev.feed.clone(),
        ws_type: ev.ws_type.clone(),
        connection_index: ev.connection_index,
        episode_kind,
        blame,
        blame_reason,
        source: ev.source.clone(),
        dhan_code: ev.dhan_code,
        detector: "ws_event_audit",
        down_secs: ev.down_secs,
        market_hours: ev.market_hours,
        evidence: format!("source={} dhan_code={}", ev.source, ev.dhan_code),
        run_partial: !corr.scan_complete,
    })
}

// ---------------------------------------------------------------------------
// Process-death synthesis (the dying process wrote no disconnect row)
// ---------------------------------------------------------------------------

/// One synthesized process-death episode (pre-classification).
#[derive(Debug, Clone, PartialEq)]
pub struct SynthesizedProcessDeath {
    /// Deterministic ts = this boot's first post-boot "up"-kind row ts.
    pub ts_ist_nanos: i64,
    pub feed: String,
    pub ws_type: String,
    pub connection_index: i64,
    /// Gap between the last pre-boot "up" row and the post-boot up row —
    /// an UPPER BOUND on the outage (audit rows are edge-triggered, so on a
    /// clean session the prior row can be hours old). Persisted only inside
    /// the evidence string; the row's `down_secs` column records 0 (=
    /// unknown) so downtime summations are never inflated by hours-old
    /// edge-triggered gaps (hostile review round 2, 2026-07-10).
    pub gap_upper_bound_secs: i64,
    /// The DEATH WINDOW `[prior_ts, connect_ts]` overlapped market hours
    /// AND the reconnect landed BEFORE session close. `false` when
    /// `post_close_restart` (the row is stamped off-market so it never
    /// votes in the headline restarts/blame tallies).
    pub market_hours: bool,
    /// This boot's connect landed AT/AFTER session close (15:30 IST) on
    /// the prior row's day (round 3, 2026-07-10): with an edge-triggered
    /// audit (a clean SIGTERM/scheduled-stop teardown persists NO
    /// disconnect row) the death is INDISTINGUISHABLE from the clean
    /// 16:30 auto-stop → same-day manual evening start. The row is still
    /// synthesized (forensic record, `blame_reason = post_close_restart`)
    /// but stamped `market_hours=false` so [`fold_episode_into_tally`]
    /// excludes it from the headline, and it never engages the
    /// restart-day partial floor.
    pub post_close_restart: bool,
}

/// `true` when this boot's connect ts is AT/AFTER the session close
/// (15:30 IST) of the PRIOR row's day — the post-close-restart ambiguity
/// window (see [`SynthesizedProcessDeath::post_close_restart`]). Pure.
#[must_use]
pub fn is_post_close_connect(prior_ts_ist_nanos: i64, connect_ts_ist_nanos: i64) -> bool {
    let day_start = prior_ts_ist_nanos
        .div_euclid(86_400 * NANOS_PER_SEC)
        .saturating_mul(86_400 * NANOS_PER_SEC);
    let mh_end =
        day_start.saturating_add(i64::from(MARKET_HOURS_END_SECS_OF_DAY_IST) * NANOS_PER_SEC);
    connect_ts_ist_nanos >= mh_end
}

/// `true` for a connection-"up" `ws_event_audit.event_kind`. The first
/// SUCCESSFUL connection of a boot whose INITIAL attempt was rejected (the
/// 429 handshake-reject restart-storm case) is emitted as `reconnected`,
/// never `connected` (`total_reconnections` increments on every failure,
/// and the Connected-vs-Reconnected split keys on `reconnection_count > 0`)
/// — so pairing/gating on `connected` alone silently dropped exactly the
/// restart-storm keys (hostile review round 2, 2026-07-10).
#[must_use]
pub fn is_up_kind(event_kind: &str) -> bool {
    matches!(event_kind, "connected" | "reconnected" | "sleep_resumed")
}

/// `true` when the DEATH WINDOW `[prior_ts, connect_ts]` overlaps the
/// market-hours window ([09:00, 15:30) IST) of the prior row's day. Pure.
///
/// This is the round-2 CRITICAL fix: on a normal day the app boots ~08:31
/// and the `connected` rows are stamped ~08:34 PRE-market; a clean session
/// writes no further "up" rows (edge-triggered audit), so an 11:00 crash
/// pairs against the 08:34 connect. Gating on the PRIOR row's own instant
/// excluded that flagship first-crash-of-the-day — the death happened
/// somewhere INSIDE `[prior_ts, connect_ts]`, so the window, not either
/// endpoint, decides in-session-ness.
fn death_window_overlaps_market_hours(prior_ts_ist_nanos: i64, connect_ts_ist_nanos: i64) -> bool {
    let day_start = prior_ts_ist_nanos
        .div_euclid(86_400 * NANOS_PER_SEC)
        .saturating_mul(86_400 * NANOS_PER_SEC);
    let mh_start =
        day_start.saturating_add(i64::from(MARKET_HOURS_START_SECS_OF_DAY_IST) * NANOS_PER_SEC);
    let mh_end =
        day_start.saturating_add(i64::from(MARKET_HOURS_END_SECS_OF_DAY_IST) * NANOS_PER_SEC);
    prior_ts_ist_nanos.max(mh_start) < connect_ts_ist_nanos.min(mh_end)
}

/// PER-KEY polling gate for the reconciler: `true` when every
/// `(feed, ws_type, connection_index)` whose LAST pre-boot row is an "up"
/// kind (a pairing candidate) also has a post-boot up-kind row — i.e. the
/// synthesis input is complete. Vacuously true when no key was up pre-boot
/// (fresh day / first boot). Pure.
///
/// Round-2 fix: the old gate broke on ANY feed's post-boot `connected`
/// (e.g. Groww connecting in seconds) while the Dhan main-feed key was
/// still inside the persisted 429 cooldown (up to 300s) + connect stagger
/// + audit-flush latency — keys whose row was not yet visible at break
/// time were silently dropped for the boot. A key that never reconnects
/// (feed disabled mid-day) burns the bounded attempt budget, then the
/// reconciler proceeds with whatever paired.
#[must_use]
pub fn post_boot_pairing_complete(rows: &[WsAuditEventLite], boot_ts_ist_nanos: i64) -> bool {
    let mut state: BTreeMap<(&str, &str, i64), (Option<&str>, bool)> = BTreeMap::new();
    for ev in rows {
        let key = (ev.feed.as_str(), ev.ws_type.as_str(), ev.connection_index);
        let entry = state.entry(key).or_insert((None, false));
        if ev.ts_ist_nanos < boot_ts_ist_nanos {
            entry.0 = Some(ev.event_kind.as_str());
        } else if is_up_kind(&ev.event_kind) {
            entry.1 = true;
        }
    }
    state
        .values()
        .all(|(last_pre, has_post)| !last_pre.is_some_and(is_up_kind) || *has_post)
}

/// Pure process-death detection over the day's audit rows.
///
/// For each `(feed, ws_type, connection_index)`: the last row BEFORE
/// `boot_ts_ist_nanos` must be an "up" kind (`connected` / `reconnected` /
/// `sleep_resumed` — the connection was live when the process died), and a
/// first post-boot "up"-kind row must exist (this boot's connect — the
/// deterministic episode ts; `reconnected` / `sleep_resumed` qualify too,
/// per [`is_up_kind`]; Groww's by-design double-`connected` per episode
/// collapses to the EARLIEST).
///
/// Gate (hostile review round 2, 2026-07-10 — CRITICAL): synthesis keys on
/// DEATH-WINDOW overlap — `[prior_ts, connect_ts]` must intersect
/// [09:00, 15:30) IST (see [`death_window_overlaps_market_hours`]) — NOT
/// on the prior row's own instant (the normal-day 08:34 pre-market connect)
/// and NOT on the boot instant. A pre-market crash (window entirely
/// < 09:00) stays excluded; the scheduled 16:30 auto-stop → next-day 08:30
/// start cycle stays excluded because the query is day-scoped (the prior
/// day's rows never appear, so the key has no pre-boot row at all).
///
/// Post-close-restart carve-out (round 3, 2026-07-10 — MEDIUM): when the
/// reconnect lands AT/AFTER session close ([`is_post_close_connect`]) the
/// death is INDISTINGUISHABLE from the clean scheduled stop → same-day
/// manual evening start (edge-triggered audit; SIGTERM persists nothing) —
/// the episode is still synthesized (forensic) but flagged
/// `post_close_restart` and stamped `market_hours=false` so it never
/// pollutes the headline restarts/blame vote or the restart-day partial
/// floor.
#[must_use]
pub fn synthesize_process_death_episodes(
    rows: &[WsAuditEventLite],
    boot_ts_ist_nanos: i64,
) -> Vec<SynthesizedProcessDeath> {
    // BTreeMap for deterministic output ordering.
    let mut by_key: BTreeMap<(String, String, i64), (Option<&WsAuditEventLite>, Option<i64>)> =
        BTreeMap::new();
    for ev in rows {
        let key = (ev.feed.clone(), ev.ws_type.clone(), ev.connection_index);
        let entry = by_key.entry(key).or_insert((None, None));
        if ev.ts_ist_nanos < boot_ts_ist_nanos {
            // Track the LAST pre-boot row (rows arrive ts-ordered, but do
            // not depend on it).
            match entry.0 {
                Some(prev) if prev.ts_ist_nanos >= ev.ts_ist_nanos => {}
                _ => entry.0 = Some(ev),
            }
        } else if is_up_kind(&ev.event_kind) {
            // The EARLIEST post-boot up-kind row (Groww double-connect
            // collapse: both sources qualify; earliest wins, deterministic).
            match entry.1 {
                Some(prev) if prev <= ev.ts_ist_nanos => {}
                _ => entry.1 = Some(ev.ts_ist_nanos),
            }
        }
    }
    let mut out = Vec::new();
    for ((feed, ws_type, connection_index), (prior, first_connect)) in by_key {
        let (Some(prior), Some(connect_ts)) = (prior, first_connect) else {
            continue;
        };
        if !is_up_kind(&prior.event_kind) {
            continue;
        }
        // Death-WINDOW gate: [prior_ts, connect_ts] must overlap the
        // session (the prior row's market_hours flag kept as belt and
        // braces against a mis-stamped ts column).
        let in_session = prior.market_hours
            || death_window_overlaps_market_hours(prior.ts_ist_nanos, connect_ts);
        if !in_session {
            continue;
        }
        // Round-3 carve-out: a reconnect at/after session close is
        // ambiguous with the clean scheduled stop → stamped off-market
        // (headline-excluded) with a distinct sub-reason downstream.
        let post_close_restart = is_post_close_connect(prior.ts_ist_nanos, connect_ts);
        out.push(SynthesizedProcessDeath {
            ts_ist_nanos: connect_ts,
            feed,
            ws_type,
            connection_index,
            gap_upper_bound_secs: connect_ts
                .saturating_sub(prior.ts_ist_nanos)
                .saturating_div(NANOS_PER_SEC),
            market_hours: in_session && !post_close_restart,
            post_close_restart,
        });
    }
    out
}

/// Deploy-vs-crash evidence for the classifier: `Some(true)` when both shas
/// are known, valid hex, and DIFFERENT (a deploy landed since the last
/// recorded binary); `Some(false)` when identical; `None` when either side
/// is unknown (fail-soft → `process_restart`, still ours). Pure.
#[must_use]
pub fn classify_build_sha_changed(build_sha: &str, deployed_sha: Option<&str>) -> Option<bool> {
    let valid = |s: &str| (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit());
    let build_ok = valid(build_sha);
    let deployed = deployed_sha.filter(|s| valid(s))?;
    if !build_ok {
        return None;
    }
    Some(build_sha != deployed)
}

/// Best-effort read of the last-deployed binary sha from the SSM
/// control-plane param (`/tickvault/<env>/deploy/binary-git-sha`,
/// deploy-provenance.md — NOT a market-data REST pull). Bounded + fail-soft
/// to `None` on any failure.
// TEST-EXEMPT: live-AWS SSM read (bounded, fail-soft); the pure consumer classify_build_sha_changed is unit-tested.
async fn fetch_deployed_binary_sha() -> Option<String> {
    let env = tickvault_core::auth::secret_manager::resolve_environment().ok()?;
    let path = format!("/tickvault/{env}/deploy/binary-git-sha");
    let client = tokio::time::timeout(
        Duration::from_secs(SCOREBOARD_HTTP_TIMEOUT_SECS),
        tickvault_core::auth::secret_manager::create_ssm_client_public(),
    )
    .await
    .ok()?;
    let resp = tokio::time::timeout(
        Duration::from_secs(SCOREBOARD_HTTP_TIMEOUT_SECS),
        client.get_parameter().name(&path).send(),
    )
    .await
    .ok()?
    .ok()?;
    resp.parameter().and_then(|p| p.value()).map(str::to_string)
}

// ---------------------------------------------------------------------------
// QuestDB /exec helper (cold path, bounded)
// ---------------------------------------------------------------------------

async fn exec_query(
    client: &reqwest::Client,
    questdb: &QuestDbConfig,
    sql: &str,
) -> Option<String> {
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    match client.get(&url).query(&[("query", sql)]).send().await {
        Ok(resp) if resp.status().is_success() => resp.text().await.ok(),
        Ok(resp) => {
            warn!(status = %resp.status(), sql, "feed_scoreboard: /exec non-2xx");
            None
        }
        Err(err) => {
            warn!(?err, sql, "feed_scoreboard: /exec request failed");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Boot-time process-death reconciler
// ---------------------------------------------------------------------------

/// Runs once per boot (after the [`PROCESS_DEATH_RECONCILE_DELAY_SECS`]
/// settle delay): synthesizes `process_death` episodes for connections that
/// were "up" IN SESSION when the previous process died. POLLS (every
/// [`PROCESS_DEATH_RECONCILE_POLL_INTERVAL_SECS`], up to
/// [`PROCESS_DEATH_RECONCILE_MAX_ATTEMPTS`] attempts) until this boot's own
/// post-boot `connected` row is visible — a fast crash-recovery boot can
/// wait out a 300s WS-GAP-08 429 cooldown BEFORE connecting, past the old
/// single 180s-delayed query. Synthesis is DEDUP-idempotent, so repeated
/// attempts are safe by construction. Returns the SYNTHESIZED EPISODE ROWS
/// themselves (round 3, 2026-07-10) — the immediate RunCatchUp/RunNow
/// aggregation folds them IN MEMORY instead of reading back rows flushed
/// seconds earlier (ILP-HTTP 200 = committed-to-WAL, NOT
/// visible-to-SELECT). The rows are returned even when every flush attempt
/// failed: the deaths HAPPENED regardless of persistence, so the tallies
/// and the restart-day floor still see them. Fail-soft everywhere; never
/// blocks boot.
// TEST-EXEMPT: orchestration over the unit-tested pure parts (synthesize_process_death_episodes / post_boot_pairing_complete / classify_build_sha_changed / parsers); a direct test needs live QuestDB + SSM.
pub async fn reconcile_process_death_episodes(
    questdb: &QuestDbConfig,
    target_ist_day: u64,
    trading_date_ist_nanos: i64,
    boot_ts_ist_nanos: i64,
) -> Vec<FeedEpisodeAuditRow> {
    ensure_feed_episode_audit_table(questdb).await;
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(SCOREBOARD_HTTP_TIMEOUT_SECS))
        .build()
    else {
        error!(
            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
            stage = "reconcile_client_build",
            "SCOREBOARD-01: process-death reconciler HTTP client build failed"
        );
        return Vec::new();
    };
    let sql = build_ws_events_day_sql(target_ist_day);
    let mut rows: Option<Vec<WsAuditEventLite>> = None;
    for attempt in 1..=PROCESS_DEATH_RECONCILE_MAX_ATTEMPTS {
        let parsed = match exec_query(&client, questdb, &sql).await {
            Some(body) => parse_ws_events(&body),
            None => None,
        };
        match parsed {
            // EVERY pairing-candidate key has its post-boot up row (per-key
            // gate — round-2 fix: ANY feed's fast connect must not stop the
            // poll while the Dhan main-feed key still waits out its 429
            // cooldown + stagger + audit-flush latency).
            Some(r) if post_boot_pairing_complete(&r, boot_ts_ist_nanos) => {
                rows = Some(r);
                break;
            }
            // Readable but some candidate key is unpaired YET (429 cooldown
            // / stagger / audit-flush latency — or that feed is disabled
            // and will never reconnect). Keep the latest view; retry.
            Some(r) => rows = Some(r),
            // Read/parse failure — retryable inside the same budget.
            None => {}
        }
        if attempt < PROCESS_DEATH_RECONCILE_MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_secs(
                PROCESS_DEATH_RECONCILE_POLL_INTERVAL_SECS,
            ))
            .await;
        }
    }
    let Some(rows) = rows else {
        error!(
            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
            stage = "reconcile_ws_events_read",
            "SCOREBOARD-01: process-death reconciler could not read/parse today's \
             connection events (every poll attempt failed) — no episodes \
             synthesized this boot (boot-reconciled rows are NOT re-creatable \
             by a later re-run)"
        );
        return Vec::new();
    };
    if !post_boot_pairing_complete(&rows, boot_ts_ist_nanos) {
        info!(
            "feed_scoreboard: the reconcile poll budget expired with some \
             connection(s) still unpaired (feed disabled mid-day, or the \
             reconnect is still pending) — synthesizing what paired; the \
             unpaired keys record no episode this boot (the 15:45 aggregation \
             still counts the coverage hole honestly)"
        );
    }
    let deaths = synthesize_process_death_episodes(&rows, boot_ts_ist_nanos);
    if deaths.is_empty() {
        info!("feed_scoreboard: process-death reconciler found no gaps (clean boot)");
        return Vec::new();
    }
    // Deploy-vs-crash sub-reason (control-plane SSM read; fail-soft).
    let deployed_sha = fetch_deployed_binary_sha().await;
    let sha_changed = classify_build_sha_changed(
        tickvault_common::build_info::BUILD_GIT_SHA,
        deployed_sha.as_deref(),
    );
    let rows_to_write: Vec<FeedEpisodeAuditRow> = deaths
        .iter()
        .map(|d| {
            let evidence_inputs = EpisodeEvidence {
                episode_kind: EPISODE_KIND_PROCESS_DEATH,
                feed: &d.feed,
                source: "boot_reconciled",
                dhan_code: -1,
                ws_gap9_bare_reset_overlap: false,
                ws_gap9_429_overlap: false,
                resilience_peer_evidence: false,
                resource_pressure_overlap: false,
                stall_reason: "",
                build_sha_changed: sha_changed,
            };
            let (blame, blame_reason) = classify_episode(&evidence_inputs);
            // Round-3 sub-reason: a reconnect at/after session close is
            // indistinguishable from the clean scheduled stop — a distinct
            // slug (still ours: OUR process stopped either way) that the
            // headline fold excludes via market_hours=false.
            let blame_reason = if d.post_close_restart {
                "post_close_restart"
            } else {
                blame_reason
            };
            FeedEpisodeAuditRow {
                ts_ist_nanos: d.ts_ist_nanos,
                trading_date_ist_nanos,
                feed: d.feed.clone(),
                ws_type: d.ws_type.clone(),
                connection_index: d.connection_index,
                episode_kind: EPISODE_KIND_PROCESS_DEATH,
                blame,
                blame_reason,
                source: "boot_reconciled".to_string(),
                dhan_code: -1,
                detector: "boot_reconciled",
                // 0 = UNKNOWN (the table doc's convention). The
                // prior-row-to-reconnect gap is an UPPER BOUND, not a
                // measured outage (edge-triggered audit rows make the prior
                // row hours old on a clean session) — recording it here
                // grossly inflated any downtime-by-blame summation (hostile
                // review round 2, 2026-07-10). The bound lives in the
                // evidence string only.
                down_secs: 0,
                // Death-WINDOW flag: [prior_ts, connect_ts] overlapped the
                // session — not the boot instant, not the prior instant.
                market_hours: d.market_hours,
                evidence: format!(
                    "prior state up; last audit row {}s before this boot's \
                     first connect (upper bound, not a measured outage){}",
                    d.gap_upper_bound_secs,
                    if d.post_close_restart {
                        "; reconnect landed post-close — indistinguishable \
                         from a clean scheduled stop, excluded from headline \
                         tallies"
                    } else {
                        ""
                    }
                ),
                run_partial: false,
            }
        })
        .collect();
    // Bounded flush retry (round-2 fix: a QuestDB blip during the
    // boot+3–13 min window used to lose this boot's death episodes FOREVER
    // — no re-run can re-create boot-reconciled rows, only the boot
    // reconciler pairs prior-up → first-connect). DEDUP-idempotent rows
    // make the re-append safe.
    let mut appended = 0_usize;
    let mut flushed = false;
    for flush_attempt in 1..=RECONCILE_FLUSH_RETRY_ATTEMPTS {
        let mut writer = FeedEpisodeAuditWriter::new(questdb);
        appended = 0;
        for row in &rows_to_write {
            match writer.append_row(row) {
                Ok(()) => appended += 1,
                Err(err) => error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "reconcile_append",
                    ?err,
                    "SCOREBOARD-01: process-death episode append failed"
                ),
            }
        }
        match writer.flush() {
            Ok(()) => {
                flushed = true;
                break;
            }
            Err(err) => {
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "reconcile_flush",
                    flush_attempt,
                    ?err,
                    "SCOREBOARD-01: process-death episode flush failed (QuestDB \
                     down?) — boot-reconciled rows are NOT re-creatable by a \
                     later re-run, so the flush retries in place"
                );
                if flush_attempt < RECONCILE_FLUSH_RETRY_ATTEMPTS {
                    tokio::time::sleep(Duration::from_secs(RECONCILE_FLUSH_RETRY_DELAY_SECS)).await;
                }
            }
        }
    }
    // Round-3 honesty split: the counter + success line claim PERSISTED
    // rows, so they fire only on a confirmed flush — an all-retries-failed
    // exit previously incremented the counter and logged "synthesized = N"
    // while ZERO rows reached QuestDB (a Rule-11 false-OK on the metric
    // surface). The rows are still RETURNED either way: the deaths
    // happened, so this run's in-memory tallies + the restart-day floor
    // count them even when persistence failed.
    if flushed {
        metrics::counter!("tv_feed_scoreboard_process_death_synthesized_total")
            .increment(appended as u64);
        info!(
            synthesized = appended,
            "feed_scoreboard: process-death reconciler synthesized episodes \
             (blame ours; deterministic ts — repeat boots UPSERT in place)"
        );
    } else {
        error!(
            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
            stage = "reconcile_flush_exhausted",
            synthesized = rows_to_write.len(),
            persisted = 0,
            "SCOREBOARD-01: every process-death episode flush attempt failed \
             — the episodes are PERMANENTLY LOST from QuestDB (boot-reconciled \
             rows are not re-creatable); this run's card still counts them \
             in-memory, but the month restart count will under-count"
        );
    }
    rows_to_write
}

// ---------------------------------------------------------------------------
// The 15:45 IST daily aggregation
// ---------------------------------------------------------------------------

/// One feed's aggregated day numbers (`-1` = source unavailable).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeedDayNumbers {
    pub ticks: i64,
    pub instruments: i64,
    pub streaming_minutes: i64,
    pub unique_win_minutes: i64,
    pub both_minutes: i64,
    pub disconnects_market: i64,
    pub disconnects_off_hours: i64,
    pub reconnects: i64,
    pub stalls: i64,
    pub blame_broker: i64,
    pub blame_ours: i64,
    pub blame_indeterminate: i64,
    pub restarts: i64,
}

impl FeedDayNumbers {
    fn unavailable() -> Self {
        let s = SCOREBOARD_UNAVAILABLE_SENTINEL;
        Self {
            ticks: s,
            instruments: s,
            streaming_minutes: s,
            unique_win_minutes: s,
            both_minutes: s,
            disconnects_market: s,
            disconnects_off_hours: s,
            reconnects: s,
            stalls: s,
            blame_broker: s,
            blame_ours: s,
            blame_indeterminate: s,
            restarts: s,
        }
    }
}

/// What the daily run produced — the Telegram scorecard is built from this.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreboardSummary {
    pub trading_date_ist: String,
    pub dhan: FeedDayNumbers,
    pub groww: FeedDayNumbers,
    pub session_minutes: i64,
    /// A data SOURCE was unavailable mid-run (read/parse/flush failure) —
    /// distinct from `early_run` so the Telegram footnotes stay honest
    /// about the CAUSE (hostile review 2026-07-10).
    pub partial_coverage: bool,
    pub degraded: bool,
    /// The operator forced this run BEFORE the daily trigger — the card
    /// covers the day only up to the run time (row stamped partial).
    pub early_run: bool,
    /// The app restarted mid-day (a synthesized/tallied in-market process
    /// death) — pre-restart records may under-count, so the row is stamped
    /// partial and the card carries the matching footnote (round 3,
    /// 2026-07-10: the row said partial while the card stayed silent).
    pub restart_partial: bool,
}

/// Runs the daily scoreboard aggregation once. Cold path (once/day at the
/// configured trigger). Fail-soft: missing sources record sentinels + an
/// honest `partial`/`degraded` outcome — the summary is ALWAYS returned so
/// the Telegram can never be silently dropped by a data failure.
///
/// # Errors
/// Returns `Err` only when NOTHING could be measured (both the episode and
/// tick sources unreachable) — the caller pages `DualFeedScorecardAborted`.
// TEST-EXEMPT: orchestration over the unit-tested pure parts (SQL builders / parsers / overlap / tallies / classifier); a direct test needs live QuestDB — covered operationally by TICKVAULT_SCOREBOARD_NOW.
pub async fn run_feed_scoreboard(
    questdb: &QuestDbConfig,
    metrics_port: u16,
    target_ist_day: u64,
    trading_date_label: String,
    forced_early_run: bool,
    is_same_day_run: bool,
    boot_synthesized_deaths: usize,
    boot_reconciled_rows: &[FeedEpisodeAuditRow],
) -> Result<ScoreboardSummary, String> {
    ensure_feed_scoreboard_tables(questdb).await;
    ensure_feed_episode_audit_table(questdb).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(SCOREBOARD_HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("HTTP client build failed: {e}"))?;

    let (day_start, _) = day_bounds_nanos(target_ist_day);
    let trading_date_ist_nanos = day_start;
    let row_ts_ist_nanos =
        day_start.saturating_add(SCOREBOARD_ROW_TS_SECS_OF_DAY_IST * NANOS_PER_SEC);

    let mut sources_complete = true;

    // 1. Same-day correlation evidence (blocking scan off the worker).
    let jsonl_dir = std::path::PathBuf::from(crate::observability::ERRORS_JSONL_DIR);
    let corr = match tokio::task::spawn_blocking(move || {
        scan_errors_jsonl_for_correlation(&jsonl_dir, target_ist_day)
    })
    .await
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "feed_scoreboard: errors.jsonl scan task failed");
            collect_correlation_evidence(&[], target_ist_day, false)
        }
    };

    // 2. Classify today's disconnect episodes from ws_event_audit, UPSERT
    //    them (DEDUP-idempotent — re-runs re-classify in place) and tally
    //    them IN MEMORY. RACE LOCK (hostile review 2026-07-10): the tallies
    //    MUST come from the rows just classified, never from a read-back of
    //    the rows just flushed — the ILP-HTTP 200 ACK means
    //    committed-to-WAL, NOT visible-to-SELECT (WAL-apply lag is
    //    unbounded under load), so a same-run read-back can silently fold
    //    the day's episodes in as ZEROS stamped outcome=complete.
    let ws_sql = build_ws_events_day_sql(target_ist_day);
    // Keep-better guard (round 3, 2026-07-10): when THIS run's evidence is
    // PARTIAL (the errors.jsonl correlation scan was incomplete or aged
    // past the 48h horizon — every row it classifies carries
    // run_partial=true), read the target day's EXISTING episode rows first
    // so an evidence-less re-classification never destructively overwrites
    // an evidence-backed (run_partial=false) verdict — QuestDB DEDUP is
    // last-write-wins, proven live on 9.3.5. A fresh-evidence run
    // (scan_complete) writes run_partial=false rows, which are same-or-
    // better and overwrite in place as before, so the read is skipped.
    let existing_partiality: HashMap<ExistingEpisodeKey, ExistingEpisodeView> =
        if corr.scan_complete {
            HashMap::new()
        } else {
            let existing_sql = build_existing_episode_partiality_day_sql(target_ist_day);
            match exec_query(&client, questdb, &existing_sql).await {
                Some(body) => match parse_existing_episode_partiality(&body) {
                    Some(map) => map,
                    None => {
                        error!(
                            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                            stage = "keep_better_read",
                            "SCOREBOARD-01: existing-episode read-back unparsable — \
                             the keep-better guard is OFF this run; a partial-evidence \
                             re-run may overwrite evidence-backed blame"
                        );
                        HashMap::new()
                    }
                },
                None => {
                    error!(
                        code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                        stage = "keep_better_read",
                        "SCOREBOARD-01: existing-episode read-back failed — the \
                         keep-better guard is OFF this run; a partial-evidence \
                         re-run may overwrite evidence-backed blame"
                    );
                    HashMap::new()
                }
            }
        };
    // `None` = the ws_event_audit read/parse itself FAILED — every value
    // derived from it (reconnects, in-memory tallies) records the -1
    // sentinel, never a fabricated 0 (hostile review 2026-07-10).
    let mut reconnects: Option<BTreeMap<String, i64>> = None;
    let mut mem_tallies: Option<BTreeMap<String, EpisodeTally>> = None;
    match exec_query(&client, questdb, &ws_sql).await {
        Some(body) => match parse_ws_events(&body) {
            Some(rows) => {
                let mut recon: BTreeMap<String, i64> = BTreeMap::new();
                let mut mem: BTreeMap<String, EpisodeTally> = BTreeMap::new();
                let mut writer = FeedEpisodeAuditWriter::new(questdb);
                let mut appended = 0_u64;
                for ev in &rows {
                    // Reconnects on the headline card compare the two
                    // BROKER FEEDS — the order-update trading channel is
                    // excluded (round-2 hostile review 2026-07-10).
                    if ev.event_kind == "reconnected" && is_market_data_ws_type(&ev.ws_type) {
                        *recon.entry(ev.feed.clone()).or_default() += 1;
                    }
                    if let Some(episode) =
                        classify_ws_event_to_episode(ev, &corr, trading_date_ist_nanos)
                    {
                        // Keep-better: never downgrade an evidence-backed
                        // row with an evidence-less re-classification —
                        // keep the EXISTING verdict in the DB (skip the
                        // UPSERT) and fold IT into the tallies.
                        let key: ExistingEpisodeKey = (
                            episode.feed.clone(),
                            episode.ws_type.clone(),
                            episode.connection_index,
                            episode.episode_kind.to_string(),
                            episode.ts_ist_nanos.div_euclid(1_000),
                        );
                        if let Some(existing) = existing_partiality.get(&key)
                            && should_suppress_episode_overwrite(
                                Some(existing.run_partial),
                                episode.run_partial,
                            )
                        {
                            error!(
                                code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                                stage = "blame_regression",
                                feed = %episode.feed,
                                ws_type = %episode.ws_type,
                                existing_blame = %existing.blame,
                                new_blame = episode.blame.as_str(),
                                "SCOREBOARD-01: keep-better SUPPRESSED an \
                                 evidence-less overwrite of an evidence-backed \
                                 episode verdict (re-run past the 48h evidence \
                                 horizon?) — the existing row is kept"
                            );
                            if is_market_data_ws_type(&episode.ws_type) {
                                fold_episode_into_tally(
                                    mem.entry(episode.feed.clone()).or_default(),
                                    episode.episode_kind,
                                    existing.blame.as_str(),
                                    existing.market_hours,
                                );
                            }
                            continue;
                        }
                        // Headline tallies take MARKET-DATA channels only
                        // (the row itself is still persisted below with its
                        // ws_type for forensics).
                        fold_market_data_episode(&mut mem, &episode);
                        match writer.append_row(&episode) {
                            Ok(()) => appended += 1,
                            Err(err) => {
                                error!(
                                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                                    stage = "episode_append",
                                    ?err,
                                    "SCOREBOARD-01: episode append failed"
                                );
                                sources_complete = false;
                            }
                        }
                    }
                }
                match writer.flush() {
                    Ok(()) => {
                        metrics::counter!("tv_feed_scoreboard_episode_rows_total")
                            .increment(appended);
                    }
                    Err(err) => {
                        error!(
                            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                            stage = "episode_flush",
                            ?err,
                            "SCOREBOARD-01: episode flush failed (QuestDB down?)"
                        );
                        sources_complete = false;
                    }
                }
                reconnects = Some(recon);
                mem_tallies = Some(mem);
            }
            None => {
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "ws_events_parse",
                    "SCOREBOARD-01: could not parse today's connection events"
                );
                sources_complete = false;
            }
        },
        None => {
            error!(
                code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                stage = "ws_events_read",
                "SCOREBOARD-01: could not read today's connection events"
            );
            sources_complete = false;
        }
    }

    // 3. Blame tallies: the in-memory step-2 tallies (race-free) MERGED
    //    with (a) THIS boot's reconciler-synthesized rows folded IN MEMORY
    //    (round 3, 2026-07-10: on the RunCatchUp/RunNow immediate paths
    //    this run fires SECONDS after the reconciler's ILP-HTTP flush —
    //    committed-to-WAL is NOT visible-to-SELECT, so a read-back could
    //    return zero rows and the flagship crash-restart day rendered
    //    "restarts: 0"), and (b) EARLIER boots' boot-reconciled rows read
    //    back from feed_episode_audit (those were written by a previous
    //    process, long visible), DEDUPED against (a) by
    //    (feed, ws_type, connection_index, ts) so an already-visible copy
    //    of this boot's row is never double-counted. The full-table
    //    read-back is ONLY the fallback when the ws read itself failed
    //    (rows an earlier same-day run wrote are better than nothing; that
    //    path is already stamped partial) — it applies the same in-memory
    //    fold + dedupe.
    let skip_keys = boot_reconciled_skip_keys(boot_reconciled_rows);
    let fold_boot_rows_in_memory = |mem: &mut BTreeMap<String, EpisodeTally>| {
        for row in boot_reconciled_rows {
            fold_market_data_episode(mem, row);
        }
    };
    let tallies: Option<BTreeMap<String, EpisodeTally>> = match mem_tallies {
        Some(mut mem) => {
            fold_boot_rows_in_memory(&mut mem);
            let boot_sql = build_boot_reconciled_episode_day_sql(target_ist_day);
            match exec_query(&client, questdb, &boot_sql).await {
                Some(body) => {
                    if fold_episode_readback_rows(&mut mem, &body, &skip_keys).is_none() {
                        error!(
                            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                            stage = "boot_reconciled_parse",
                            "SCOREBOARD-01: boot-reconciled episode read-back \
                             unparsable — restart counts may under-count"
                        );
                        sources_complete = false;
                    }
                }
                None => {
                    error!(
                        code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                        stage = "boot_reconciled_read",
                        "SCOREBOARD-01: boot-reconciled episode read-back failed \
                         — restart counts may under-count"
                    );
                    sources_complete = false;
                }
            }
            Some(mem)
        }
        None => {
            let fallback =
                match exec_query(&client, questdb, &build_episode_day_sql(target_ist_day)).await {
                    Some(body) => {
                        let mut mem: BTreeMap<String, EpisodeTally> = BTreeMap::new();
                        fold_boot_rows_in_memory(&mut mem);
                        fold_episode_readback_rows(&mut mem, &body, &skip_keys).map(|_| mem)
                    }
                    None => None,
                };
            if fallback.is_none() {
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "episode_aggregate",
                    "SCOREBOARD-01: episode blame aggregate unavailable — recording sentinels"
                );
            }
            // sources_complete already false (the ws read failed above).
            fallback
        }
    };

    // 4. Per-feed tick / instrument / minute coverage (SQL over the day's
    //    ticks partition — flagged O(day-rows), server-side, cold).
    let mut feed_numbers: BTreeMap<&'static str, FeedDayNumbers> = BTreeMap::new();
    let mut minute_sets: BTreeMap<&'static str, Option<HashSet<String>>> = BTreeMap::new();
    for feed in tickvault_common::feed::Feed::ALL {
        let label = feed.as_str();
        let mut n = FeedDayNumbers::unavailable();
        // Ticks (LOCAL micros-literal builder — see the regression lock).
        let ticks_sql = build_scoreboard_ticks_count_sql(label, target_ist_day);
        if let Some(body) = exec_query(&client, questdb, &ticks_sql).await
            && let Some(count) = parse_questdb_count(&body)
        {
            n.ticks = count;
        }
        // Distinct (security_id, segment) pairs.
        let instr_sql = build_feed_instruments_count_sql(label, target_ist_day);
        if let Some(body) = exec_query(&client, questdb, &instr_sql).await
            && let Some(count) = parse_questdb_count(&body)
        {
            n.instruments = count;
        }
        // Distinct session minutes (≤375 opaque keys).
        let minutes_sql = build_feed_session_minutes_sql(label, target_ist_day);
        let set = exec_query(&client, questdb, &minutes_sql)
            .await
            .and_then(|body| parse_minute_set(&body));
        if let Some(ref s) = set {
            n.streaming_minutes = i64::try_from(s.len()).unwrap_or(i64::MAX);
        }
        if n.ticks < 0 || n.instruments < 0 || set.is_none() {
            sources_complete = false;
        }
        minute_sets.insert(label, set);
        feed_numbers.insert(label, n);
    }

    // 5. Feed-level unique-win / both minutes from the two minute sets.
    if let (Some(Some(dhan_set)), Some(Some(groww_set))) =
        (minute_sets.get("dhan"), minute_sets.get("groww"))
    {
        let (dhan_only, groww_only, both) = compute_minute_overlap(dhan_set, groww_set);
        if let Some(n) = feed_numbers.get_mut("dhan") {
            n.unique_win_minutes = dhan_only;
            n.both_minutes = both;
        }
        if let Some(n) = feed_numbers.get_mut("groww") {
            n.unique_win_minutes = groww_only;
            n.both_minutes = both;
        }
    }

    // 6. Fold the episode tallies in (when the aggregate answered).
    if let Some(ref tallies) = tallies {
        for (label, n) in &mut feed_numbers {
            let t = tallies.get(*label).copied().unwrap_or_default();
            n.disconnects_market = t.disconnects_market;
            n.disconnects_off_hours = t.disconnects_off_hours;
            n.stalls = t.stalls;
            n.restarts = t.restarts;
            n.blame_broker = t.blame_broker;
            n.blame_ours = t.blame_ours;
            n.blame_indeterminate = t.blame_indeterminate;
            // The reconnect source is the ws_event_audit read — when THAT
            // read failed, the column records the -1 sentinel even though
            // the episode tallies (fallback read-back) answered: never a
            // fabricated 0 (hostile review 2026-07-10).
            n.reconnects = reconnects
                .as_ref()
                .map_or(SCOREBOARD_UNAVAILABLE_SENTINEL, |m| {
                    m.get(*label).copied().unwrap_or(0)
                });
        }
    }

    // 7. AUDIT-WS-01 under-count cross-check (self-scrape). SCOPED to
    //    same-day runs ONLY (round-2 hostile review 2026-07-10): the
    //    counter is CURRENT-SESSION state — a past-day backfill inheriting
    //    TODAY's session drops falsely stamped a perfectly-recorded past
    //    day degraded.
    let mut degraded = false;
    if is_same_day_run {
        let metrics_url = format!("http://127.0.0.1:{metrics_port}/metrics");
        if let Ok(resp) = client.get(&metrics_url).send().await
            && resp.status().is_success()
            && let Ok(body) = resp.text().await
            && let Some(dropped) = parse_prom_counter_sum(&body, "tv_ws_event_audit_dropped_total")
            && dropped > 0
        {
            degraded = true;
            error!(
                code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                stage = "audit_drop_crosscheck",
                dropped,
                "SCOREBOARD-01: ws_event_audit dropped rows this session — the \
                 day's episode counts are a floor, not a truth (outcome=degraded)"
            );
        }
    }
    // The dual of the same scoping hole: the counter RESETS on process
    // death, so on a crash-restart day the 15:45 run sees 0 drops even if
    // the pre-crash session dropped audit rows all morning. When THIS
    // boot's reconciler synthesized an IN-MARKET process death for today,
    // the episode source's completeness is unknowable → the day is AT
    // LEAST partial (never a false 'complete' — audit Rule 11).
    //
    // Round-3 LOW fix: the floor is ALSO data-driven — a PAST-day backfill
    // of a day whose tallies carry restarts > 0 (boot-reconciled rows from
    // the day itself) has the SAME unknowable pre-crash audit-drop state,
    // so it can never stamp 'complete' either. Post-close restarts
    // (market_hours=false) are excluded from `restarts` by the fold, so a
    // clean scheduled-stop → evening-start day does NOT flip its completed
    // row to partial.
    let tallied_restarts: i64 = tallies
        .as_ref()
        .map_or(0, |t| t.values().map(|x| x.restarts).sum());
    let restart_day_floor =
        (is_same_day_run && boot_synthesized_deaths > 0) || tallied_restarts > 0;
    if restart_day_floor {
        info!(
            boot_synthesized_deaths,
            tallied_restarts,
            "feed_scoreboard: the process restarted mid-day — pre-crash \
             audit-drop state is unknowable, stamping the day at least partial"
        );
    }
    // The persisted row is stamped partial for an early forced run too —
    // it covers the day only up to the run time (hostile review
    // 2026-07-10: a pre-15:45 forced run consumes the day's single
    // scheduled run, so its mid-day numbers must never masquerade as a
    // complete end-of-day row).
    let partial_coverage = !sources_complete;
    let row_partial = partial_coverage || forced_early_run || restart_day_floor;
    let outcome = if degraded {
        ScoreboardOutcome::Degraded
    } else if row_partial {
        ScoreboardOutcome::Partial
    } else {
        ScoreboardOutcome::Complete
    };

    // 8. Write the two daily rows (deterministic ts — re-runs UPSERT).
    let mut writer = FeedScoreboardWriter::new(questdb);
    for feed in tickvault_common::feed::Feed::ALL {
        let label = feed.as_str();
        let n = feed_numbers
            .get(label)
            .copied()
            .unwrap_or_else(FeedDayNumbers::unavailable);
        let uptime_pct = if n.streaming_minutes >= 0 {
            #[allow(clippy::cast_precision_loss)]
            // APPROVED: display-only percentage over bounded minute counts.
            {
                (n.streaming_minutes as f64 / SCOREBOARD_SESSION_MINUTES as f64) * 100.0
            }
        } else {
            // Unknown — 0.0 paired with partial_coverage=true (never a
            // silent false 0%).
            0.0
        };
        let row = FeedScoreboardDailyRow {
            ts_ist_nanos: row_ts_ist_nanos,
            trading_date_ist_nanos,
            feed: label,
            ticks_captured: n.ticks,
            instruments_seen: n.instruments,
            mapped_instruments: SCOREBOARD_UNAVAILABLE_SENTINEL,
            unmapped_instruments: SCOREBOARD_UNAVAILABLE_SENTINEL,
            covered_instrument_minutes: SCOREBOARD_UNAVAILABLE_SENTINEL,
            unique_win_minutes: n.unique_win_minutes,
            both_minutes: n.both_minutes,
            lag_p50_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
            lag_p99_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
            lag_max_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
            lag_samples: SCOREBOARD_UNAVAILABLE_SENTINEL,
            lag_floor_ms: match *feed {
                tickvault_common::feed::Feed::Dhan => LAG_FLOOR_MS_DHAN,
                tickvault_common::feed::Feed::Groww => LAG_FLOOR_MS_GROWW,
            },
            disconnects_market: n.disconnects_market,
            disconnects_off_hours: n.disconnects_off_hours,
            reconnects: n.reconnects,
            stalls: n.stalls,
            blame_broker: n.blame_broker,
            blame_ours: n.blame_ours,
            blame_indeterminate: n.blame_indeterminate,
            restarts_detected: n.restarts,
            streaming_minutes: n.streaming_minutes,
            session_minutes: SCOREBOARD_SESSION_MINUTES,
            uptime_pct,
            partial_coverage: row_partial,
            coverage_source: CoverageSource::SqlBackfill,
            outcome,
        };
        if let Err(err) = writer.append_daily_row(&row) {
            error!(
                code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                stage = "daily_append",
                feed = label,
                ?err,
                "SCOREBOARD-01: daily row append failed"
            );
        }
    }
    if let Err(err) = writer.flush() {
        error!(
            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
            stage = "daily_flush",
            ?err,
            "SCOREBOARD-01: daily rows flush failed (QuestDB down?) — the \
             DEDUP-idempotent TICKVAULT_SCOREBOARD_NOW re-run backfills"
        );
    }

    metrics::counter!("tv_feed_scoreboard_runs_total", "outcome" => outcome.as_str()).increment(1);

    let dhan = feed_numbers
        .get("dhan")
        .copied()
        .unwrap_or_else(FeedDayNumbers::unavailable);
    let groww = feed_numbers
        .get("groww")
        .copied()
        .unwrap_or_else(FeedDayNumbers::unavailable);
    // Total blackout (nothing measured at all) → the caller pages Aborted.
    if dhan.ticks < 0 && groww.ticks < 0 && tallies.is_none() {
        return Err("every data source was unreachable — nothing measured".to_string());
    }
    Ok(ScoreboardSummary {
        trading_date_ist: trading_date_label,
        dhan,
        groww,
        session_minutes: SCOREBOARD_SESSION_MINUTES,
        partial_coverage,
        degraded,
        early_run: forced_early_run,
        // Threaded to the card footnote (round 3: the persisted row said
        // partial while the card rendered no restart caveat).
        restart_partial: restart_day_floor,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::feed_blame::BlameClass;

    const DAY: u64 = 20_644; // an arbitrary IST day number
    fn day_ts(secs_into_day: i64) -> i64 {
        (DAY as i64) * 86_400 * NANOS_PER_SEC + secs_into_day * NANOS_PER_SEC
    }

    fn ev(
        ts: i64,
        feed: &str,
        ws_type: &str,
        kind: &str,
        source: &str,
        dhan_code: i64,
        market_hours: bool,
    ) -> WsAuditEventLite {
        WsAuditEventLite {
            ts_ist_nanos: ts,
            feed: feed.to_string(),
            ws_type: ws_type.to_string(),
            connection_index: 0,
            event_kind: kind.to_string(),
            source: source.to_string(),
            dhan_code,
            down_secs: 0,
            market_hours,
        }
    }

    #[test]
    fn test_scoreboard_trigger_constant_is_1545_ist() {
        assert_eq!(SCOREBOARD_ROW_TS_SECS_OF_DAY_IST, 56_700);
        assert_eq!(SESSION_START_SECS_OF_DAY_IST, 33_300);
        assert_eq!(SESSION_END_SECS_OF_DAY_IST, 55_800);
        assert_eq!(WS_GAP9_OVERLAP_WINDOW_SECS, 120);
        assert_eq!(RESOURCE_OVERLAP_WINDOW_SECS, 300);
    }

    /// Micros day bounds every builder MUST embed (QuestDB interprets a
    /// bare integer literal in a TIMESTAMP comparison as epoch MICROS —
    /// the 2026-07-10 CRITICAL regression lock at the top of the SQL
    /// section). For a 2026-era day the micros bound is 16 digits; the
    /// broken nanos bound is 19 digits.
    fn day_micros() -> (i64, i64) {
        let start = (DAY as i64) * 86_400 * 1_000_000;
        (start, start + 86_400 * 1_000_000)
    }

    #[test]
    fn test_decide_scoreboard_start_boundaries() {
        let trigger = 56_700_u32; // 15:45:00 IST
        // 15:44:00 → sleep 60s.
        assert_eq!(
            decide_scoreboard_start(trigger - 60, true, false, trigger),
            ScoreboardStart::SleepThenRun(60)
        );
        // At/after the trigger on a trading day → catch-up (day-1/backfill).
        assert_eq!(
            decide_scoreboard_start(trigger, true, false, trigger),
            ScoreboardStart::RunCatchUp
        );
        assert_eq!(
            decide_scoreboard_start(23 * 3600, true, false, trigger),
            ScoreboardStart::RunCatchUp
        );
        // Non-trading day → skip; force overrides both gates.
        assert_eq!(
            decide_scoreboard_start(trigger, false, false, trigger),
            ScoreboardStart::SkipNonTradingDay
        );
        assert_eq!(
            decide_scoreboard_start(trigger, false, true, trigger),
            ScoreboardStart::RunNow
        );
    }

    #[test]
    fn test_is_in_market_hours_secs_boundaries() {
        assert!(!is_in_market_hours_secs(9 * 3600 - 1));
        assert!(is_in_market_hours_secs(9 * 3600));
        assert!(is_in_market_hours_secs(12 * 3600));
        assert!(!is_in_market_hours_secs(15 * 3600 + 30 * 60));
    }

    #[test]
    fn test_parse_scoreboard_date_override_strict_fail_closed() {
        // Valid strict YYYY-MM-DD → (ist day number, label).
        let (day, label) = parse_scoreboard_date_override("2026-07-09").expect("valid date");
        assert_eq!(label, "2026-07-09");
        // 2026-07-09 = 20_643 days since 1970-01-01.
        assert_eq!(day, 20_643);
        // Whitespace tolerated (trimmed), nothing else.
        assert_eq!(
            parse_scoreboard_date_override(" 2026-07-09 ").map(|(d, _)| d),
            Some(20_643)
        );
        // Fail-closed: malformed shapes / impossible dates / pre-epoch /
        // traversal-ish junk all yield None — never a guessed day.
        for bad in [
            "",
            "2026-7-9",
            "2026/07/09",
            "20260709",
            "2026-13-01",
            "2026-02-30",
            "1969-12-31",
            "2026-07-09x",
            "x2026-07-09",
            "2026-07-0..",
            "２026-07-09",
        ] {
            assert_eq!(
                parse_scoreboard_date_override(bad),
                None,
                "{bad:?} must be rejected fail-closed"
            );
        }
    }

    #[test]
    fn test_build_ws_events_day_sql_micros_window() {
        let (start, end) = day_micros();
        // Regression lock: the embedded literals are MICROS (16 digits for
        // a 2026 date), never the silently-empty NANOS form (19 digits).
        assert_eq!(
            start.to_string().len(),
            16,
            "micros bound must be 16 digits"
        );
        let nanos_start = (DAY as i64) * 86_400 * NANOS_PER_SEC;
        assert_eq!(nanos_start.to_string().len(), 19);
        let ws = build_ws_events_day_sql(DAY);
        assert!(ws.contains("from ws_event_audit"), "{ws}");
        assert!(ws.contains("cast(ts as long)"), "micros cast: {ws}");
        assert!(ws.contains(&format!("ts >= {start}")), "{ws}");
        assert!(ws.contains(&format!("ts < {end}")), "{ws}");
        assert!(ws.contains("order by ts"), "{ws}");
        assert!(
            !ws.contains(&nanos_start.to_string()),
            "NANOS literal must never reach the SQL (matches zero rows): {ws}"
        );
    }

    #[test]
    fn test_build_episode_day_sql_micros_window() {
        let (start, end) = day_micros();
        let ep = build_episode_day_sql(DAY);
        assert!(ep.contains("from feed_episode_audit"), "{ep}");
        assert!(
            ep.contains("ws_type"),
            "ws_type must ride along for the market-data headline filter: {ep}"
        );
        assert!(
            ep.contains("connection_index") && ep.contains("cast(ts as long)"),
            "the read-back dedupe key columns must ride along (round 3): {ep}"
        );
        assert!(ep.contains(&format!("ts >= {start}")), "{ep}");
        assert!(ep.contains(&format!("ts < {end}")), "{ep}");
        assert!(
            !ep.contains(&((DAY as i64) * 86_400 * NANOS_PER_SEC).to_string()),
            "nanos literal banned: {ep}"
        );
    }

    #[test]
    fn test_build_boot_reconciled_episode_day_sql_micros_and_detector_filter() {
        // The boot-reconciled variant carries the SAME micros window plus
        // the detector filter (the race-fix merge source).
        let (start, end) = day_micros();
        let boot = build_boot_reconciled_episode_day_sql(DAY);
        assert!(boot.contains("from feed_episode_audit"), "{boot}");
        assert!(
            boot.contains("ws_type"),
            "ws_type must ride along for the market-data headline filter: {boot}"
        );
        assert!(
            boot.contains("connection_index") && boot.contains("cast(ts as long)"),
            "the read-back dedupe key columns must ride along (round 3): {boot}"
        );
        assert!(boot.contains(&format!("ts >= {start}")), "{boot}");
        assert!(boot.contains(&format!("ts < {end}")), "{boot}");
        assert!(boot.contains("detector = 'boot_reconciled'"), "{boot}");
        assert!(
            !boot.contains(&((DAY as i64) * 86_400 * NANOS_PER_SEC).to_string()),
            "nanos literal banned: {boot}"
        );
    }

    #[test]
    fn test_build_scoreboard_ticks_count_sql_micros_window() {
        let (start, end) = day_micros();
        let sql = build_scoreboard_ticks_count_sql("groww", DAY);
        assert!(sql.contains("from ticks"), "{sql}");
        assert!(sql.contains("feed = 'groww'"), "{sql}");
        assert!(sql.contains(&format!("ts >= {start}")), "{sql}");
        assert!(sql.contains(&format!("ts < {end}")), "{sql}");
        assert!(
            !sql.contains(&((DAY as i64) * 86_400 * NANOS_PER_SEC).to_string()),
            "nanos literal banned: {sql}"
        );
    }

    #[test]
    fn test_build_feed_instruments_count_sql_micros_and_segment_qualified() {
        let (start, end) = day_micros();
        // I-P1-11: the instrument distinct is segment-qualified.
        let instr = build_feed_instruments_count_sql("groww", DAY);
        assert!(instr.contains("distinct security_id, segment"), "{instr}");
        assert!(instr.contains("feed = 'groww'"), "{instr}");
        assert!(instr.contains(&format!("ts >= {start}")), "{instr}");
        assert!(instr.contains(&format!("ts < {end}")), "{instr}");
    }

    #[test]
    fn test_build_feed_session_minutes_sql_micros_session_window() {
        // Session-minute window is [09:15, 15:30) in MICROS ts-space.
        let (start, _) = day_micros();
        let mins = build_feed_session_minutes_sql("dhan", DAY);
        let sess_start = start + SESSION_START_SECS_OF_DAY_IST * 1_000_000;
        let sess_end = start + SESSION_END_SECS_OF_DAY_IST * 1_000_000;
        assert!(mins.contains("feed = 'dhan'"), "{mins}");
        assert!(mins.contains(&format!("ts >= {sess_start}")), "{mins}");
        assert!(mins.contains(&format!("ts < {sess_end}")), "{mins}");
        assert!(mins.contains("date_trunc('minute', ts)"), "{mins}");
        assert!(
            !mins.contains(&(start * 1_000).to_string()),
            "nanos literal banned: {mins}"
        );
    }

    #[test]
    fn test_parse_ws_events_from_exec_body() {
        let body = r#"{"columns":[],"dataset":[
            [1784000000000000, "dhan", "main_feed", 0, "disconnected", "Dhan or network", -1, 0, true],
            [1784000060000000, "dhan", "main_feed", 0, "reconnected", "n/a", -1, 60, true],
            ["bad row"],
            [1784000120000000, "groww", "groww_bridge", 0, "connected", "groww_subscribed", -1, 0, false]
        ]}"#;
        let rows = parse_ws_events(body).expect("parse");
        assert_eq!(rows.len(), 3, "malformed rows skipped, never panic");
        assert_eq!(rows[0].event_kind, "disconnected");
        assert_eq!(rows[0].ts_ist_nanos, 1_784_000_000_000_000 * 1_000);
        assert_eq!(rows[1].down_secs, 60);
        assert_eq!(rows[2].feed, "groww");
        // Unparsable body → None (caller records sentinels).
        assert_eq!(parse_ws_events("not json"), None);
    }

    #[test]
    fn test_compute_minute_overlap() {
        let a: HashSet<String> = ["09:15", "09:16", "09:17"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let b: HashSet<String> = ["09:16", "09:17", "09:18", "09:19"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(compute_minute_overlap(&a, &b), (1, 2, 2));
    }

    #[test]
    fn test_parse_minute_set_from_exec_body() {
        let body =
            r#"{"dataset":[["2026-07-10T09:15:00.000000Z"],["2026-07-10T09:16:00.000000Z"]]}"#;
        let set = parse_minute_set(body).expect("parse");
        assert_eq!(set.len(), 2);
        // Unparsable body → None (caller records sentinels).
        assert_eq!(parse_minute_set("not json"), None);
    }

    #[test]
    fn test_parse_errors_jsonl_line_extracts_code_reason_ts() {
        let line = r#"{"timestamp":"2026-07-10T05:05:00.123456Z","level":"ERROR","code":"WS-GAP-09","reason":"bare_dhan_reset","message":"x"}"#;
        let ev = parse_errors_jsonl_line(line).expect("parse");
        assert_eq!(ev.code, "WS-GAP-09");
        assert_eq!(ev.reason.as_deref(), Some("bare_dhan_reset"));
        // 05:05 UTC + 5:30 = 10:35 IST.
        let expected_utc_nanos =
            chrono::DateTime::parse_from_rfc3339("2026-07-10T05:05:00.123456Z")
                .expect("ts")
                .timestamp_nanos_opt()
                .expect("nanos");
        assert_eq!(
            ev.ts_ist_nanos,
            expected_utc_nanos + tickvault_common::constants::IST_UTC_OFFSET_NANOS
        );
        // Codeless / malformed lines → None, never panic.
        assert_eq!(parse_errors_jsonl_line(r#"{"timestamp":"x"}"#), None);
        assert_eq!(parse_errors_jsonl_line("not json"), None);
    }

    #[test]
    fn test_collect_correlation_evidence_and_has_overlap_windows() {
        let events = vec![
            JsonlCodeEvent {
                code: "WS-GAP-09".to_string(),
                reason: Some("bare_dhan_reset".to_string()),
                ts_ist_nanos: day_ts(40_000),
            },
            JsonlCodeEvent {
                code: "RESILIENCE-01".to_string(),
                reason: None,
                ts_ist_nanos: day_ts(41_000),
            },
            JsonlCodeEvent {
                code: "PROC-01".to_string(),
                reason: None,
                ts_ist_nanos: day_ts(42_000),
            },
            // Wrong day — excluded.
            JsonlCodeEvent {
                code: "RESOURCE-01".to_string(),
                reason: None,
                ts_ist_nanos: day_ts(90_000),
            },
        ];
        let corr = collect_correlation_evidence(&events, DAY, true);
        assert!(corr.resilience_same_day);
        assert_eq!(corr.ws_gap9_bare_ts_nanos.len(), 1);
        assert_eq!(corr.resource_ts_nanos.len(), 1, "next-day event excluded");
        assert!(corr.scan_complete);
        // ±120s WS-GAP-09 window.
        assert!(has_overlap(
            &corr.ws_gap9_bare_ts_nanos,
            day_ts(40_100),
            120
        ));
        assert!(!has_overlap(
            &corr.ws_gap9_bare_ts_nanos,
            day_ts(40_200),
            120
        ));
        // ±300s resource window.
        assert!(has_overlap(&corr.resource_ts_nanos, day_ts(42_290), 300));
        assert!(!has_overlap(&corr.resource_ts_nanos, day_ts(42_301), 300));
    }

    #[test]
    fn test_classify_ws_event_to_episode_maps_kinds_and_overlaps() {
        let corr = CorrelationEvidence {
            ws_gap9_bare_ts_nanos: vec![day_ts(40_000)],
            scan_complete: true,
            ..CorrelationEvidence::default()
        };
        // A reset WITH the ±120s WS-GAP-09 line → broker/bare_rst.
        let reset = ev(
            day_ts(40_060),
            "dhan",
            "main_feed",
            "disconnected",
            "Dhan or network",
            -1,
            true,
        );
        let row = classify_ws_event_to_episode(&reset, &corr, day_ts(0)).expect("episode");
        assert_eq!(row.blame, BlameClass::Broker);
        assert_eq!(row.blame_reason, "bare_rst");
        assert_eq!(row.detector, "ws_event_audit");
        assert!(!row.run_partial);
        // Lifecycle kinds are NOT episodes.
        let connect = ev(
            day_ts(41_000),
            "dhan",
            "main_feed",
            "connected",
            "n/a",
            -1,
            true,
        );
        assert!(classify_ws_event_to_episode(&connect, &corr, day_ts(0)).is_none());
        // Partial evidence stamps run_partial.
        let no_scan = CorrelationEvidence::default(); // scan_complete = false
        let row = classify_ws_event_to_episode(&reset, &no_scan, day_ts(0)).expect("episode");
        assert!(row.run_partial);
        assert_eq!(row.blame, BlameClass::Indeterminate, "no corroboration");
    }

    #[test]
    fn test_synthesize_process_death_up_state_prior() {
        // Pre-boot: connected at 10:00 IN SESSION. Boot at 11:00. Post-boot
        // connect at 11:02 → ONE synthesized death at the post-boot
        // connect ts, market_hours from the DEATH window (the prior row).
        let rows = vec![
            ev(
                day_ts(36_000),
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            ),
            ev(
                day_ts(39_720),
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            ),
        ];
        let boot = day_ts(39_600);
        let deaths = synthesize_process_death_episodes(&rows, boot);
        assert_eq!(deaths.len(), 1);
        assert_eq!(deaths[0].ts_ist_nanos, day_ts(39_720), "deterministic ts");
        assert_eq!(deaths[0].gap_upper_bound_secs, 3_720);
        assert_eq!(deaths[0].feed, "dhan");
        assert!(deaths[0].market_hours, "death window was in-session");
        // PER-KEY polling gate: the dhan key's post-boot connect is
        // visible here → complete; pre-boot rows alone leave it pending.
        assert!(post_boot_pairing_complete(&rows, boot));
        assert!(
            !post_boot_pairing_complete(&rows[..1], boot),
            "pre-boot up rows alone must keep the polling gate pending"
        );
    }

    #[test]
    fn test_is_up_kind() {
        for up in ["connected", "reconnected", "sleep_resumed"] {
            assert!(is_up_kind(up), "{up}");
        }
        for down in [
            "disconnected",
            "disconnected_off_hours",
            "sleep_entered",
            "junk",
        ] {
            assert!(!is_up_kind(down), "{down}");
        }
    }

    #[test]
    fn test_synthesize_process_death_premarket_prior_midmarket_crash() {
        // Round-2 CRITICAL: the NORMAL-day topology — the app boots at
        // ~08:31 and the connect row lands ~08:34 PRE-market
        // (market_hours=false); a clean session writes no further up rows.
        // A mid-market crash restarted at 11:02 pairs against that 08:34
        // row: the DEATH WINDOW [08:34, 11:02] overlaps [09:00, 15:30) →
        // ONE episode (the old prior-instant gate synthesized NOTHING).
        let rows = vec![
            ev(
                day_ts(30_840), // 08:34 IST — pre-market, flag false
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                false,
            ),
            ev(
                day_ts(39_720), // 11:02 IST — this boot's connect
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            ),
        ];
        let deaths = synthesize_process_death_episodes(&rows, day_ts(39_600));
        assert_eq!(
            deaths.len(),
            1,
            "the 08:3x-prior + mid-market-crash flagship topology must synthesize"
        );
        assert!(deaths[0].market_hours, "the death window was in-session");
        // A PRE-MARKET crash (window entirely before 09:00 — prior 08:00,
        // restart connect 08:26) stays excluded: no session time was lost.
        let pre_market_only = vec![
            ev(
                day_ts(28_800), // 08:00 IST
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                false,
            ),
            ev(
                day_ts(30_360), // 08:26 IST
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                false,
            ),
        ];
        assert!(
            synthesize_process_death_episodes(&pre_market_only, day_ts(30_000)).is_empty(),
            "a fully pre-market death window must NOT synthesize"
        );
    }

    #[test]
    fn test_synthesize_process_death_overnight_stop_start_cycle_excluded() {
        // The scheduled 16:30 auto-stop → next-day 08:30 start cycle: the
        // reconcile query is DAY-scoped, so yesterday's 16:30-era rows never
        // appear — this boot's view carries only its OWN post-boot connect,
        // no pre-boot row, and synthesizes nothing.
        let today_only = vec![ev(
            day_ts(30_600), // 08:30 IST connect of the fresh start
            "dhan",
            "main_feed",
            "connected",
            "n/a",
            -1,
            false,
        )];
        let boot = day_ts(30_540); // 08:29 IST boot
        assert!(
            synthesize_process_death_episodes(&today_only, boot).is_empty(),
            "an overnight stop/start must never count as a process death"
        );
        // And the per-key gate is vacuously complete (no pre-boot up key).
        assert!(post_boot_pairing_complete(&today_only, boot));
    }

    #[test]
    fn test_synthesize_process_death_pairs_on_reconnected_and_gate_is_per_key() {
        // A boot whose FIRST main-feed connect attempt is rejected (the 429
        // restart-storm case) emits its first SUCCESSFUL connection as
        // `reconnected` — it must pair AND satisfy the gate (round-2 fix:
        // 'connected'-only pairing dropped exactly these keys).
        let rows = vec![
            ev(
                day_ts(36_000), // 10:00 IST — up in session
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            ),
            ev(
                day_ts(40_200), // 11:10 IST — first SUCCESS is `reconnected`
                "dhan",
                "main_feed",
                "reconnected",
                "n/a",
                -1,
                true,
            ),
        ];
        let boot = day_ts(39_600);
        assert!(post_boot_pairing_complete(&rows, boot));
        let deaths = synthesize_process_death_episodes(&rows, boot);
        assert_eq!(deaths.len(), 1, "a post-boot `reconnected` must pair");
        assert_eq!(deaths[0].ts_ist_nanos, day_ts(40_200));
        // PER-KEY gate: Groww's fast post-boot connect must NOT satisfy
        // the gate while the Dhan main-feed key (up pre-boot) is still
        // waiting out its cooldown.
        let groww_only_connected = vec![
            ev(
                day_ts(36_000),
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            ),
            ev(
                day_ts(36_100),
                "groww",
                "groww_bridge",
                "connected",
                "groww_subscribed",
                -1,
                true,
            ),
            ev(
                day_ts(39_650), // Groww reconnects in seconds post-boot…
                "groww",
                "groww_bridge",
                "connected",
                "groww_subscribed",
                -1,
                true,
            ),
        ];
        assert!(
            !post_boot_pairing_complete(&groww_only_connected, boot),
            "ANY feed's connect must not break the poll while another \
             pairing-candidate key is still unpaired"
        );
    }

    #[test]
    fn test_synthesize_process_death_gates_on_death_window_not_boot_instant() {
        // Hostile review 2026-07-10: an IN-SESSION crash (prior up row at
        // 15:20) followed by an OUT-OF-SESSION restart (15:35 boot, 15:40
        // connect) is STILL a death — the gate keys on the prior row's
        // in-session-ness, never on the boot instant.
        let crash_then_late_restart = vec![
            ev(
                day_ts(55_200), // 15:20 IST — in session, market_hours=true
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            ),
            ev(
                day_ts(56_400), // 15:40 IST connect (post-close)
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                false,
            ),
        ];
        let deaths = synthesize_process_death_episodes(&crash_then_late_restart, day_ts(56_100));
        assert_eq!(deaths.len(), 1, "in-session death must synthesize");
        // Round-3 revision: the 15:40 reconnect landed POST-close, so the
        // death is ambiguous with a clean scheduled stop — the row is
        // synthesized (forensic) but stamped off-market + post_close so it
        // never votes in the headline restarts/blame tallies.
        assert!(
            deaths[0].post_close_restart,
            "a post-close reconnect must carry the ambiguity flag"
        );
        assert!(
            !deaths[0].market_hours,
            "a post-close reconnect must be stamped off-market (headline-excluded)"
        );
        // Belt-and-braces: a mis-stamped market_hours=false prior whose ts
        // is inside [09:00, 15:30) still gates in via the ts check.
        let mis_stamped = vec![
            ev(
                day_ts(40_000),
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                false, // wrong flag; ts says 11:06 IST
            ),
            ev(
                day_ts(41_000),
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            ),
        ];
        assert_eq!(
            synthesize_process_death_episodes(&mis_stamped, day_ts(40_500)).len(),
            1
        );
    }

    #[test]
    fn test_synthesize_process_death_skips_down_state_and_out_of_session_death() {
        // Prior row was a DISCONNECT (state down) → clean death of nothing.
        let down_prior = vec![
            ev(
                day_ts(36_000),
                "dhan",
                "main_feed",
                "disconnected",
                "Dhan or network",
                -1,
                true,
            ),
            ev(
                day_ts(39_720),
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            ),
        ];
        assert!(synthesize_process_death_episodes(&down_prior, day_ts(39_600)).is_empty());
        // sleep_entered prior → dormant, not up.
        let sleeping = vec![
            ev(
                day_ts(36_000),
                "dhan",
                "main_feed",
                "sleep_entered",
                "n/a",
                -1,
                false,
            ),
            ev(
                day_ts(39_720),
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            ),
        ];
        assert!(synthesize_process_death_episodes(&sleeping, day_ts(39_600)).is_empty());
        // OUT-OF-SESSION death window (prior up row at 16:35, flag false)
        // → nothing (post-close idle churn, not an in-session death).
        let post_close = vec![
            ev(
                day_ts(59_700), // 16:35 IST
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                false,
            ),
            ev(
                day_ts(60_300), // 16:45 IST connect
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                false,
            ),
        ];
        assert!(synthesize_process_death_episodes(&post_close, day_ts(60_000)).is_empty());
        // No post-boot connect row (yet) → nothing (no deterministic ts).
        let no_connect = vec![ev(
            day_ts(36_000),
            "dhan",
            "main_feed",
            "connected",
            "n/a",
            -1,
            true,
        )];
        assert!(synthesize_process_death_episodes(&no_connect, day_ts(39_600)).is_empty());
        assert!(
            !post_boot_pairing_complete(&no_connect, day_ts(39_600)),
            "an in-session pre-boot up key without its post-boot up row must \
             keep the gate pending"
        );
    }

    #[test]
    fn test_synthesize_post_close_stop_then_evening_start_topology() {
        // Round 3 (MEDIUM): the documented ops workflow — the 16:30
        // scheduled auto-stop (a clean SIGTERM teardown persists NO
        // disconnect row) → same-day manual evening start at 17:30. The
        // evening boot pairs [morning 08:34 connect, 17:30 connect]: the
        // window overlaps the session, but the reconnect landed
        // POST-close, so the death is indistinguishable from the clean
        // stop. The episode IS synthesized (forensic row) but flagged
        // post_close_restart + stamped off-market so it never becomes a
        // phantom in-market restart that re-writes the day's completed
        // scorecard (restarts+1 / blame_ours+1 / partial floor).
        let rows = vec![
            ev(
                day_ts(30_840), // 08:34 IST morning connect
                "groww",
                "groww_bridge",
                "connected",
                "groww_subscribed",
                -1,
                false,
            ),
            ev(
                day_ts(63_000), // 17:30 IST evening connect
                "groww",
                "groww_bridge",
                "connected",
                "groww_subscribed",
                -1,
                false,
            ),
        ];
        let boot = day_ts(62_940); // 17:29 IST manual evening boot
        let deaths = synthesize_process_death_episodes(&rows, boot);
        assert_eq!(deaths.len(), 1, "the forensic row still exists");
        assert!(deaths[0].post_close_restart);
        assert!(
            !deaths[0].market_hours,
            "must never stamp a phantom in-market death"
        );
        // The headline fold EXCLUDES it: restarts stay 0, no blame vote.
        let mut t = EpisodeTally::default();
        fold_episode_into_tally(
            &mut t,
            EPISODE_KIND_PROCESS_DEATH,
            "ours",
            deaths[0].market_hours,
        );
        assert_eq!(
            t,
            EpisodeTally::default(),
            "a post-close restart must not vote in the headline"
        );
        // An in-market restart still counts (control).
        let mut t2 = EpisodeTally::default();
        fold_episode_into_tally(&mut t2, EPISODE_KIND_PROCESS_DEATH, "ours", true);
        assert_eq!(t2.restarts, 1);
        assert_eq!(t2.blame_ours, 1);
        // is_post_close_connect boundary: 15:29:59 pre-close, 15:30:00 post.
        assert!(!is_post_close_connect(day_ts(36_000), day_ts(55_799)));
        assert!(is_post_close_connect(day_ts(36_000), day_ts(55_800)));
    }

    #[test]
    fn test_is_post_close_connect_boundary() {
        // 15:29:59 reconnect = pre-close; 15:30:00 = post-close; and the
        // day-scoping uses the PRIOR row's day.
        assert!(!is_post_close_connect(day_ts(36_000), day_ts(55_799)));
        assert!(is_post_close_connect(day_ts(36_000), day_ts(55_800)));
        assert!(is_post_close_connect(day_ts(30_840), day_ts(63_000)));
    }

    #[test]
    fn test_boot_reconciled_skip_keys_nanos_to_micros() {
        // The skip key carries the SQL side's cast(ts as long) MICROS view
        // of the in-memory nanos ts — the two must meet in one unit.
        let row = FeedEpisodeAuditRow {
            ts_ist_nanos: 1_784_000_180_000_000_500, // non-round nanos
            trading_date_ist_nanos: 0,
            feed: "dhan".to_string(),
            ws_type: "main_feed".to_string(),
            connection_index: 3,
            episode_kind: EPISODE_KIND_PROCESS_DEATH,
            blame: BlameClass::Ours,
            blame_reason: "process_restart",
            source: "boot_reconciled".to_string(),
            dhan_code: -1,
            detector: "boot_reconciled",
            down_secs: 0,
            market_hours: true,
            evidence: String::new(),
            run_partial: false,
        };
        let keys = boot_reconciled_skip_keys(std::slice::from_ref(&row));
        assert!(keys.contains(&(
            "dhan".to_string(),
            "main_feed".to_string(),
            3,
            1_784_000_180_000_000, // micros (nanos / 1000, floor)
        )));
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn test_parse_existing_episode_partiality_skips_malformed_rows() {
        let body = r#"{"dataset":[
            ["groww", "groww_bridge", 0, "process_death", 1784000180000000, true, "ours", false],
            ["missing", "columns"]
        ]}"#;
        let map = parse_existing_episode_partiality(body).expect("parse");
        assert_eq!(map.len(), 1);
        let view = map
            .get(&(
                "groww".to_string(),
                "groww_bridge".to_string(),
                0,
                "process_death".to_string(),
                1_784_000_180_000_000,
            ))
            .expect("row");
        assert!(view.run_partial);
        assert_eq!(view.blame, "ours");
        assert!(!view.market_hours);
    }

    #[test]
    fn test_forced_now_without_date_composes_with_trading_day_validation() {
        // Round 3 (MEDIUM): decide_scoreboard_start returns RunNow on
        // force_now BEFORE the trading-day check (deliberate — weekend
        // backfills of past trading days need it), so the caller MUST
        // apply the same semantic gate to the no-DATE forced arm:
        // today-as-target on a non-trading day is REFUSED (else two
        // all-zero rows stamp outcome='complete' after 15:45 — the exact
        // fabricated-day class the DATE arm already refuses). Pinned here
        // as the decide/validate COMPOSITION the main.rs arm wires.
        assert_eq!(
            decide_scoreboard_start(60_000, false, true, 56_700),
            ScoreboardStart::RunNow,
            "force_now bypasses the run-day check by design"
        );
        assert!(
            validate_scoreboard_backfill_date(DAY, DAY, false).is_err(),
            "the caller's no-DATE gate must refuse a non-trading today"
        );
        assert!(validate_scoreboard_backfill_date(DAY, DAY, true).is_ok());
    }

    #[test]
    fn test_synthesize_process_death_deterministic_ts_and_groww_double_connect() {
        // Groww emits `connected` TWICE per episode (groww_subscribed then
        // groww_sidecar) — the EARLIEST post-boot connect is the episode ts,
        // so repeat reconciles stamp the SAME ts (DEDUP-idempotent).
        let rows = vec![
            ev(
                day_ts(36_000),
                "groww",
                "groww_bridge",
                "reconnected",
                "groww_resumed",
                -1,
                true,
            ),
            ev(
                day_ts(39_700),
                "groww",
                "groww_bridge",
                "connected",
                "groww_subscribed",
                -1,
                true,
            ),
            ev(
                day_ts(39_710),
                "groww",
                "groww_bridge",
                "connected",
                "groww_sidecar",
                -1,
                true,
            ),
        ];
        let a = synthesize_process_death_episodes(&rows, day_ts(39_600));
        let b = synthesize_process_death_episodes(&rows, day_ts(39_600));
        assert_eq!(a, b, "repeat reconciliation is deterministic");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].ts_ist_nanos, day_ts(39_700), "earliest connect wins");
    }

    #[test]
    fn test_deploy_vs_crash_sub_reason() {
        let build = "3144aad3144aad3144aad3144aad3144aad31441";
        let build = &build[..40];
        // Different valid shas → a deploy landed.
        assert_eq!(
            classify_build_sha_changed(build, Some("aafa226aafa226aafa226aafa226aafa226aafa2")),
            Some(true)
        );
        // Identical → crash/restart of the same binary.
        assert_eq!(classify_build_sha_changed(build, Some(build)), Some(false));
        // Unknown build / missing / invalid deployed sha → fail-soft None.
        assert_eq!(classify_build_sha_changed("unknown", Some(build)), None);
        assert_eq!(classify_build_sha_changed(build, None), None);
        assert_eq!(classify_build_sha_changed(build, Some("not-hex!")), None);
        assert_eq!(
            classify_build_sha_changed(build, Some("abc")),
            None,
            "too short"
        );
    }

    #[test]
    fn test_parse_prom_counter_sum_labeled_series() {
        let body = "# HELP tv_ws_event_audit_dropped_total x\n\
                    tv_ws_event_audit_dropped_total{reason=\"full\"} 2\n\
                    tv_ws_event_audit_dropped_total{reason=\"closed\"} 3\n\
                    tv_other_total 9\n";
        assert_eq!(
            parse_prom_counter_sum(body, "tv_ws_event_audit_dropped_total"),
            Some(5),
            "labeled series must SUM"
        );
        // Bare series also matches.
        assert_eq!(parse_prom_counter_sum("m 7\n", "m"), Some(7));
        // A name-prefix metric must NOT match (token boundary).
        assert_eq!(
            parse_prom_counter_sum("tv_other_totals 9\n", "tv_other_total"),
            None
        );
        // Absent → None, never silently zero.
        assert_eq!(parse_prom_counter_sum(body, "tv_absent"), None);
    }

    #[test]
    fn test_fold_episode_readback_rows_tallies_per_feed() {
        let body = r#"{"dataset":[
            ["dhan", "disconnect", "broker", true, "main_feed", 0, 1784000000000000],
            ["dhan", "disconnect", "indeterminate", true, "main_feed", 0, 1784000060000000],
            ["dhan", "off_hours_disconnect", "indeterminate", false, "main_feed", 0, 1784000120000000],
            ["dhan", "process_death", "ours", true, "main_feed", 0, 1784000180000000],
            ["dhan", "disconnect", "indeterminate", true, "order_update", 0, 1784000240000000],
            ["dhan", "process_death", "ours", true, "order_update", 0, 1784000180000000],
            ["groww", "stall_restart", "broker", true, "groww_bridge", 0, 1784000300000000],
            ["groww", "disconnect", "ours", true, "groww_bridge", 0, 1784000360000000]
        ]}"#;
        let mut tallies: BTreeMap<String, EpisodeTally> = BTreeMap::new();
        fold_episode_readback_rows(&mut tallies, body, &HashSet::new()).expect("fold");
        let d = tallies.get("dhan").copied().expect("dhan tally");
        assert_eq!(
            d.disconnects_market, 2,
            "the order_update disconnect must be ABSENT from disconnects_market \
             (round-2 hostile review: the trading channel must not pollute the \
             market-data comparison)"
        );
        assert_eq!(d.disconnects_off_hours, 1);
        assert_eq!(
            d.restarts, 1,
            "one crash = one restart per feed — the order_update process_death \
             row must not double-count Dhan's restarts"
        );
        assert_eq!(d.blame_broker, 1);
        assert_eq!(d.blame_ours, 1, "process_death counts in the blame tally");
        assert_eq!(
            d.blame_indeterminate, 1,
            "off-hours rows are EXCLUDED from the blame tally"
        );
        let g = tallies.get("groww").copied().expect("groww tally");
        assert_eq!(g.stalls, 1);
        assert_eq!(g.blame_broker, 1);
        assert_eq!(g.blame_ours, 1);
    }

    #[test]
    fn test_is_market_data_ws_type_allowlist() {
        assert!(is_market_data_ws_type("main_feed"));
        assert!(is_market_data_ws_type("groww_bridge"));
        for excluded in ["order_update", "depth_20", "depth_200", "junk", ""] {
            assert!(!is_market_data_ws_type(excluded), "{excluded}");
        }
    }

    #[test]
    fn test_fold_market_data_episode_skips_order_update() {
        let row = |ws_type: &str| FeedEpisodeAuditRow {
            ts_ist_nanos: day_ts(40_000),
            trading_date_ist_nanos: day_ts(0),
            feed: "dhan".to_string(),
            ws_type: ws_type.to_string(),
            connection_index: 0,
            episode_kind: EPISODE_KIND_DISCONNECT,
            blame: BlameClass::Broker,
            blame_reason: "bare_rst",
            source: "Dhan or network".to_string(),
            dhan_code: -1,
            detector: "ws_event_audit",
            down_secs: 0,
            market_hours: true,
            evidence: String::new(),
            run_partial: false,
        };
        let mut tallies: BTreeMap<String, EpisodeTally> = BTreeMap::new();
        // An order-update disconnect folds into NOTHING (headline filter).
        fold_market_data_episode(&mut tallies, &row("order_update"));
        assert!(
            tallies.is_empty(),
            "order_update episodes must be absent from the headline tallies"
        );
        // The market-data twin folds normally.
        fold_market_data_episode(&mut tallies, &row("main_feed"));
        let d = tallies.get("dhan").copied().expect("dhan tally");
        assert_eq!(d.disconnects_market, 1);
        assert_eq!(d.blame_broker, 1);
    }

    #[test]
    fn test_fold_episode_into_tally_unknown_kind_counts_nothing() {
        // Round-2 fix: an unknown kind lands in NO count column, so it must
        // not vote in the blame split either — blame sums must always
        // reconcile against the visible incident columns.
        let mut t = EpisodeTally::default();
        fold_episode_into_tally(&mut t, "future_kind", "broker", true);
        assert_eq!(t, EpisodeTally::default(), "every field must stay 0");
    }

    #[test]
    fn test_validate_scoreboard_backfill_date() {
        // Past trading day → OK.
        assert!(validate_scoreboard_backfill_date(20_643, 20_644, true).is_ok());
        // Today (trading) → OK (same-day forced re-run).
        assert!(validate_scoreboard_backfill_date(20_644, 20_644, true).is_ok());
        // FUTURE date → refused (a fabricated all-zero 'complete' day).
        let err = validate_scoreboard_backfill_date(20_645, 20_644, true)
            .expect_err("future date must be refused");
        assert!(err.contains("future"), "{err}");
        // Non-trading target (weekend/holiday typo) → refused, even though
        // the RUN day's trading-day check is force-bypassed.
        let err = validate_scoreboard_backfill_date(20_640, 20_644, false)
            .expect_err("non-trading target must be refused");
        assert!(err.contains("not a trading day"), "{err}");
    }

    #[test]
    fn test_sanitize_scoreboard_trigger_bounds() {
        // In-range values pass through untouched.
        assert_eq!(sanitize_scoreboard_trigger(56_700), (56_700, false));
        assert_eq!(sanitize_scoreboard_trigger(55_800), (55_800, false));
        assert_eq!(sanitize_scoreboard_trigger(86_399), (86_399, false));
        // ≥ 86_400 (the silent never-fires typo) and pre-session-close
        // values fall back to the 15:45 default, flagged invalid.
        assert_eq!(sanitize_scoreboard_trigger(90_000), (56_700, true));
        assert_eq!(sanitize_scoreboard_trigger(0), (56_700, true));
        assert_eq!(sanitize_scoreboard_trigger(55_799), (56_700, true));
    }

    #[test]
    fn test_scan_errors_jsonl_filters_codes_and_skips_bare_symlink_name() {
        // Round-2 fix: only CORRELATION_CODES are retained at parse time
        // and the bare `errors.jsonl` compat symlink name is skipped.
        let dir = std::env::temp_dir().join(format!(
            "tv-scoreboard-scan-test-{}-{}",
            std::process::id(),
            day_ts(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let interesting = r#"{"timestamp":"2026-07-10T05:05:00Z","code":"PROC-01","message":"x"}"#;
        let noise =
            r#"{"timestamp":"2026-07-10T05:05:01Z","code":"AGGREGATOR-SEAL-01","message":"y"}"#;
        std::fs::write(
            dir.join("errors.jsonl.2026-07-10-05"),
            format!("{interesting}\n{noise}\n"),
        )
        .expect("write hourly file");
        // The bare name would double-read the newest hour — must be skipped
        // (write a RESILIENCE line into it; it must NOT surface).
        std::fs::write(
            dir.join("errors.jsonl"),
            r#"{"timestamp":"2026-07-10T05:05:02Z","code":"RESILIENCE-01","message":"z"}"#,
        )
        .expect("write bare compat file");
        // 2026-07-10T05:05Z + 5:30 = IST day 20_644.
        let corr = scan_errors_jsonl_for_correlation(&dir, 20_644);
        assert!(corr.scan_complete);
        assert_eq!(
            corr.resource_ts_nanos.len(),
            1,
            "the PROC-01 line must be retained"
        );
        assert!(
            !corr.resilience_same_day,
            "the bare errors.jsonl symlink-name file must be skipped"
        );
        std::fs::remove_dir_all(&dir).ok();
        // The retained-code list stays in sync with the evidence matcher.
        assert_eq!(CORRELATION_CODES.len(), 7);
        for code in CORRELATION_CODES {
            let ev = JsonlCodeEvent {
                code: code.to_string(),
                reason: Some("bare_dhan_reset".to_string()),
                ts_ist_nanos: day_ts(40_000),
            };
            let out = collect_correlation_evidence(&[ev], DAY, true);
            assert!(
                out.resilience_same_day
                    || !out.ws_gap9_bare_ts_nanos.is_empty()
                    || !out.resource_ts_nanos.is_empty(),
                "{code} must be consumed by collect_correlation_evidence"
            );
        }
    }

    #[test]
    fn test_fold_episode_into_tally_matches_sql_aggregate_rule() {
        // The in-memory race-fix path and the SQL read-back share ONE fold
        // rule — a headline disconnect counts blame; off-hours does not.
        let mut t = EpisodeTally::default();
        fold_episode_into_tally(&mut t, EPISODE_KIND_DISCONNECT, "broker", true);
        fold_episode_into_tally(&mut t, EPISODE_KIND_DISCONNECT, "ours", false);
        fold_episode_into_tally(&mut t, EPISODE_KIND_OFF_HOURS_DISCONNECT, "broker", false);
        fold_episode_into_tally(&mut t, EPISODE_KIND_PROCESS_DEATH, "ours", true);
        fold_episode_into_tally(&mut t, EPISODE_KIND_STALL_RESTART, "broker", true);
        assert_eq!(t.disconnects_market, 1);
        assert_eq!(
            t.disconnects_off_hours, 2,
            "off-market disconnect + off-hours row"
        );
        assert_eq!(t.restarts, 1);
        assert_eq!(t.stalls, 1);
        assert_eq!(t.blame_broker, 2, "headline disconnect + stall");
        assert_eq!(t.blame_ours, 1, "process death");
        assert_eq!(t.blame_indeterminate, 0);
    }

    #[test]
    fn test_fold_episode_readback_rows_dedupes_in_memory_keys() {
        // Round 3: the read-back skips rows whose (feed, ws_type,
        // connection_index, ts) key matches a row this run already folded
        // in-memory — an already-WAL-applied copy of this boot's
        // synthesized row must never double-count restarts.
        let body = r#"{"dataset":[
            ["dhan", "process_death", "ours", true, "main_feed", 0, 1784000180000000],
            ["dhan", "process_death", "ours", true, "main_feed", 1, 1784000180000000]
        ]}"#;
        let mem_row = FeedEpisodeAuditRow {
            ts_ist_nanos: 1_784_000_180_000_000 * 1_000,
            trading_date_ist_nanos: 0,
            feed: "dhan".to_string(),
            ws_type: "main_feed".to_string(),
            connection_index: 0,
            episode_kind: EPISODE_KIND_PROCESS_DEATH,
            blame: BlameClass::Ours,
            blame_reason: "process_restart",
            source: "boot_reconciled".to_string(),
            dhan_code: -1,
            detector: "boot_reconciled",
            down_secs: 0,
            market_hours: true,
            evidence: String::new(),
            run_partial: false,
        };
        let skip = boot_reconciled_skip_keys(std::slice::from_ref(&mem_row));
        let mut tallies: BTreeMap<String, EpisodeTally> = BTreeMap::new();
        fold_market_data_episode(&mut tallies, &mem_row);
        let folded = fold_episode_readback_rows(&mut tallies, body, &skip).expect("fold");
        assert_eq!(folded, 1, "the conn-0 read-back copy must be skipped");
        let d = tallies.get("dhan").copied().expect("dhan");
        assert_eq!(
            d.restarts, 2,
            "in-memory row + the DISTINCT conn-1 read-back row — never 3"
        );
        // Without the skip set the same body double-counts (the race the
        // dedupe exists for).
        let mut naive: BTreeMap<String, EpisodeTally> = BTreeMap::new();
        fold_market_data_episode(&mut naive, &mem_row);
        fold_episode_readback_rows(&mut naive, body, &HashSet::new()).expect("fold");
        assert_eq!(naive.get("dhan").expect("dhan").restarts, 3);
    }

    #[test]
    fn test_should_suppress_episode_overwrite_keep_better_rule() {
        // Round 3: only the evidence-DOWNGRADE combination is suppressed.
        assert!(
            should_suppress_episode_overwrite(Some(false), true),
            "evidence-backed row + evidence-less re-classification = keep better"
        );
        assert!(
            !should_suppress_episode_overwrite(Some(false), false),
            "fresh evidence overwrites in place (value-idempotent)"
        );
        assert!(
            !should_suppress_episode_overwrite(Some(true), true),
            "partial-over-partial re-runs stay idempotent"
        );
        assert!(
            !should_suppress_episode_overwrite(Some(true), false),
            "fresh evidence UPGRADES a partial row"
        );
        assert!(
            !should_suppress_episode_overwrite(None, true),
            "a brand-new key always persists (first run of the day)"
        );
        assert!(!should_suppress_episode_overwrite(None, false));
    }

    #[test]
    fn test_build_existing_episode_partiality_day_sql_and_parse() {
        let (start, end) = day_micros();
        let sql = build_existing_episode_partiality_day_sql(DAY);
        assert!(sql.contains("from feed_episode_audit"), "{sql}");
        assert!(sql.contains("run_partial"), "{sql}");
        assert!(sql.contains("blame"), "{sql}");
        assert!(sql.contains("market_hours"), "{sql}");
        assert!(sql.contains("cast(ts as long)"), "{sql}");
        assert!(sql.contains(&format!("ts >= {start}")), "{sql}");
        assert!(sql.contains(&format!("ts < {end}")), "{sql}");
        assert!(
            !sql.contains(&((DAY as i64) * 86_400 * NANOS_PER_SEC).to_string()),
            "nanos literal banned: {sql}"
        );
        let body = r#"{"dataset":[
            ["dhan", "main_feed", 0, "disconnect", 1784000000000000, false, "ours", true],
            ["dhan", "main_feed", 0, "disconnect", 1784000060000000, true, "broker", true],
            ["bad row"]
        ]}"#;
        let map = parse_existing_episode_partiality(body).expect("parse");
        assert_eq!(map.len(), 2, "malformed rows skipped, never panic");
        let key = (
            "dhan".to_string(),
            "main_feed".to_string(),
            0_i64,
            "disconnect".to_string(),
            1_784_000_000_000_000_i64,
        );
        let view = map.get(&key).expect("row present");
        assert!(!view.run_partial);
        assert_eq!(view.blame, "ours");
        assert!(view.market_hours);
        assert_eq!(parse_existing_episode_partiality("not json"), None);
    }

    #[test]
    fn test_episode_kind_for_event_mapping() {
        assert_eq!(episode_kind_for_event("disconnected"), Some("disconnect"));
        assert_eq!(
            episode_kind_for_event("disconnected_off_hours"),
            Some("off_hours_disconnect")
        );
        for lifecycle in [
            "connected",
            "reconnected",
            "sleep_entered",
            "sleep_resumed",
            "junk",
        ] {
            assert_eq!(episode_kind_for_event(lifecycle), None, "{lifecycle}");
        }
    }

    #[test]
    fn test_scan_errors_jsonl_for_correlation_missing_dir_is_incomplete_not_panic() {
        let corr = scan_errors_jsonl_for_correlation(
            std::path::Path::new("/nonexistent/scoreboard/logs"),
            DAY,
        );
        assert!(!corr.scan_complete, "missing dir = honest partial evidence");
        assert!(!corr.resilience_same_day);
    }
}
