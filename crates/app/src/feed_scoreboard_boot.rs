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
    CoverageSource, FeedScoreboardDailyRow, FeedScoreboardWriter,
    LAG_FLOOR_MS_DHAN, LAG_FLOOR_MS_GROWW, SCOREBOARD_SESSION_MINUTES,
    SCOREBOARD_UNAVAILABLE_SENTINEL, ScoreboardOutcome, ensure_feed_scoreboard_tables,
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

/// Latest trigger that still fires BEFORE the prod box's scheduled 16:30
/// IST auto-stop, with flush headroom (16:15). An ACCEPTED trigger past
/// this sleeps into the EventBridge stop every day — graceful shutdown
/// cancels the task silently by design, so the whole deliverable is
/// disabled with zero signal (round 4, 2026-07-10: the sanitizer's wide
/// [15:30, 24:00) bound re-admitted the exact silent-teardown failure its
/// own doc names). The bound stays wide (a manually-run box legitimately
/// triggers later); the spawn site WARNS loudly instead.
pub const SCOREBOARD_TRIGGER_AUTO_STOP_WARN_SECS: u32 = 16 * 3600 + 15 * 60; // 58_500

/// `true` when an accepted trigger fires at/after the 16:15 IST warn
/// threshold — on the auto-stopped prod box it would never fire. Pure.
#[must_use]
pub fn scoreboard_trigger_after_auto_stop(effective_trigger_secs_of_day_ist: u32) -> bool {
    effective_trigger_secs_of_day_ist >= SCOREBOARD_TRIGGER_AUTO_STOP_WARN_SECS
}

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
/// lock above). `pub(crate)` since 2026-07-10: the tick-conservation audit's
/// `build_conservation_ticks_count_sql` shares this single micros source so
/// no per-file nanos re-derivation can regress (main-nanos-verdict hand-off).
pub(crate) fn day_bounds_micros(target_ist_day: u64) -> (i64, i64) {
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

/// The target day's EXISTING `feed_scoreboard_daily` outcomes + lag
/// columns — the DAILY-row keep-better guard input (round 4, 2026-07-10 —
/// MEDIUM): a same-day evening RunCatchUp rerun re-measures the FRESH
/// process-local audit-drop counter as 0 and would UPSERT
/// outcome='complete' over the 15:45 run's 'degraded' verdict at the same
/// deterministic ts — the drop evidence is session-local and durable
/// NOWHERE except the overwritten row. The lag columns joined the read in
/// PR-C review round 1 (2026-07-11 — HIGH): the same rerun's FRESH
/// process has EMPTY day histograms, so without them the UPSERT erased the
/// 15:45 run's MEASURED lag distribution with −1 (see
/// [`fold_existing_lag_keep_better`]). Micros literals.
#[must_use]
pub fn build_existing_daily_outcome_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select feed, outcome, lag_p50_ms, lag_p99_ms, lag_max_ms, \
         lag_samples, unique_win_minutes, both_minutes, mapped_instruments, \
         unmapped_instruments, covered_instrument_minutes, coverage_source \
         from feed_scoreboard_daily \
         where ts >= {start} and ts < {end}"
    )
}

/// Parse the [`build_existing_daily_outcome_sql`] response into a
/// feed → outcome map. Pure; `None` = unparsable body.
#[must_use]
pub fn parse_existing_daily_outcomes(body: &str) -> Option<BTreeMap<String, String>> {
    let rows = parse_dataset(body)?;
    let mut out = BTreeMap::new();
    for row in rows {
        if let Some(cols) = row.as_array()
            && cols.len() >= 2
            && let (Some(feed), Some(outcome)) = (cols[0].as_str(), cols[1].as_str())
        {
            out.insert(feed.to_string(), outcome.to_string());
        }
    }
    Some(out)
}

/// One feed's EXISTING daily-row lag columns (PR-C review round 1,
/// 2026-07-11) — the lag keep-better guard input. `-1` = the existing row
/// never measured lag (pre-PR-C day / thin day / backfill).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingDailyLag {
    pub p50_ms: i64,
    pub p99_ms: i64,
    pub max_ms: i64,
    pub samples: i64,
}

/// Parse the [`build_existing_daily_outcome_sql`] response into a
/// feed → existing-lag map (columns 2..=5 of the same body the outcome
/// parse reads). Pure; `None` = unparsable body; a row without the lag
/// columns (defensive) records sentinels, never fabricated values.
#[must_use]
pub fn parse_existing_daily_lag(body: &str) -> Option<BTreeMap<String, ExistingDailyLag>> {
    let rows = parse_dataset(body)?;
    let mut out = BTreeMap::new();
    for row in rows {
        if let Some(cols) = row.as_array()
            && let Some(feed) = cols.first().and_then(|c| c.as_str())
        {
            let col = |i: usize| -> i64 {
                cols.get(i)
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(SCOREBOARD_UNAVAILABLE_SENTINEL)
            };
            out.insert(
                feed.to_string(),
                ExistingDailyLag {
                    p50_ms: col(2),
                    p99_ms: col(3),
                    max_ms: col(4),
                    samples: col(5),
                },
            );
        }
    }
    Some(out)
}

/// Lag keep-better rule (PR-C review round 1, 2026-07-11 — HIGH): when
/// THIS run measured no day lag (`n.lag_samples < 0` — a fresh post-close
/// process's empty histograms, a thin <50-sample day, or a past-day
/// backfill) but the day's EXISTING row carries a MEASURED distribution
/// (`existing.samples >= 0`), fold the existing measured values forward
/// instead of erasing them with −1 at the deterministic-ts UPSERT. A run
/// that measured real samples always writes its own values (a rerun may
/// update). Returns `true` when the fold fired — the caller logs
/// `stage="lag_regression"`. Pure.
pub fn fold_existing_lag_keep_better(
    n: &mut FeedDayNumbers,
    existing: Option<&ExistingDailyLag>,
) -> bool {
    if n.lag_samples >= 0 {
        return false; // this run measured — its distribution wins
    }
    let Some(e) = existing else {
        return false; // no existing row — nothing to preserve
    };
    if e.samples < 0 {
        return false; // existing row never measured — −1 stands honestly
    }
    n.lag_p50_ms = e.p50_ms;
    n.lag_p99_ms = e.p99_ms;
    n.lag_max_ms = e.max_ms;
    n.lag_samples = e.samples;
    true
}

/// One feed's EXISTING daily-row coverage columns (scoreboard PR-D) — the
/// coverage keep-better guard input (columns 6..=11 of the SAME
/// [`build_existing_daily_outcome_sql`] body the outcome/lag parses read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingDailyCoverage {
    pub unique_win_minutes: i64,
    pub both_minutes: i64,
    pub mapped_instruments: i64,
    pub unmapped_instruments: i64,
    pub covered_instrument_minutes: i64,
    /// The persisted `coverage_source` label (`"in_memory"` / `"mixed"` /
    /// `"sql_backfill"`); empty when the column is absent (pre-PR-D rows).
    pub coverage_source: String,
}

/// Parse the [`build_existing_daily_outcome_sql`] response into a
/// feed → existing-coverage map. Pure; `None` = unparsable body; rows
/// without the coverage columns (pre-PR-D schema) record sentinels + an
/// empty source label, never fabricated values.
#[must_use]
pub fn parse_existing_daily_coverage(
    body: &str,
) -> Option<BTreeMap<String, ExistingDailyCoverage>> {
    let rows = parse_dataset(body)?;
    let mut out = BTreeMap::new();
    for row in rows {
        if let Some(cols) = row.as_array()
            && let Some(feed) = cols.first().and_then(|c| c.as_str())
        {
            let col = |i: usize| -> i64 {
                cols.get(i)
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(SCOREBOARD_UNAVAILABLE_SENTINEL)
            };
            out.insert(
                feed.to_string(),
                ExistingDailyCoverage {
                    unique_win_minutes: col(6),
                    both_minutes: col(7),
                    mapped_instruments: col(8),
                    unmapped_instruments: col(9),
                    covered_instrument_minutes: col(10),
                    coverage_source: cols
                        .get(11)
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
            );
        }
    }
    Some(out)
}

/// Rank of a coverage source for the keep-better comparison: the in-memory
/// presence registry (exact per-instrument compare) outranks a mixed
/// (partial-session registry) row, which outranks the SQL minute-set
/// approximation. Unknown/absent labels rank lowest.
#[must_use]
pub fn coverage_source_rank(label: &str) -> u8 {
    match label {
        "in_memory" => 2,
        "mixed" => 1,
        _ => 0,
    }
}

/// Coverage keep-better rule (scoreboard PR-D — mirror of the lag/outcome
/// guards): a rerun whose coverage came from a LOWER-ranked source than the
/// day's EXISTING row (a post-close deploy restart's empty registry falling
/// back to SQL, or a past-day backfill) folds the existing measured
/// coverage columns + source forward instead of erasing the 15:45 run's
/// registry-derived numbers at the deterministic-ts UPSERT. A strictly
/// better-ranked run always writes its own values.
///
/// **Equal rank is VALUE-AWARE** (PR-D review round 1, HIGH): an evening
/// same-day rerun re-registers today's slots (warm-snapshot boot) but its
/// fresh process folded ZERO bits — equal-rank 'mixed' vs 'mixed' would
/// otherwise erase the 15:45 run's measured coverage with zeros. A rerun
/// that measured materially nothing (`covered_instrument_minutes == 0`)
/// while the existing row measured something folds the existing row
/// forward — the outcome keep-better's measured-zero precedent (round 5,
/// 2026-07-10). Returns `true` when the fold fired — the caller logs
/// `stage="coverage_regression"` and skips the step-8b detail rows. Pure.
///
/// Honest residual (PR-D review round 2, LOW): equal rank is value-aware
/// ONLY at zero — a FORCED mid-day run's coverage row
/// (`TICKVAULT_SCOREBOARD_NOW` at ~14:00, `mixed`) can still be replaced
/// by a later same-day run's SMALLER nonzero post-restart window (forced
/// run → in-session crash → pre-close reboot → the 15:45 drain measures
/// only the post-reboot minutes; `mixed` == `mixed`, nonzero → it wins,
/// and step 8b rewrites the detail rows from that window). Reachable only
/// via a forced mid-session run + in-session crash + pre-close reboot —
/// the normal 15:45-first-write flow cannot hit it (a pre-15:45 crash
/// means no existing row). Documented in the runbook §4 triage note.
pub fn fold_existing_coverage_keep_better(
    n: &mut FeedDayNumbers,
    existing: Option<&ExistingDailyCoverage>,
) -> bool {
    let Some(e) = existing else {
        return false; // no existing row — nothing to preserve
    };
    let existing_rank = coverage_source_rank(&e.coverage_source);
    let new_rank = coverage_source_rank(n.coverage_source.as_str());
    if new_rank > existing_rank {
        return false; // this run's source is strictly better — it wins
    }
    if new_rank == existing_rank
        && !(n.covered_instrument_minutes == 0 && e.covered_instrument_minutes > 0)
    {
        return false; // equal rank + a material measurement — it wins
    }
    n.unique_win_minutes = e.unique_win_minutes;
    n.both_minutes = e.both_minutes;
    n.mapped_instruments = e.mapped_instruments;
    n.unmapped_instruments = e.unmapped_instruments;
    n.covered_instrument_minutes = e.covered_instrument_minutes;
    n.coverage_source = if existing_rank == 2 {
        CoverageSource::InMemory
    } else {
        CoverageSource::Mixed
    };
    true
}

/// Daily-outcome keep-better rule (round 4, 2026-07-10): `true` when
/// writing `new` would ERASE an existing 'degraded' verdict for the day —
/// a rerun must NEVER downgrade degraded → partial/complete (the degraded
/// evidence was the ORIGINAL session's audit-drop counter, unrecoverable
/// by any rerun). Every other combination proceeds (a rerun may still
/// upgrade partial → complete with fresh full evidence). Pure.
#[must_use]
pub fn should_keep_degraded_outcome(
    existing_outcomes: &BTreeMap<String, String>,
    new: ScoreboardOutcome,
) -> bool {
    !matches!(new, ScoreboardOutcome::Degraded)
        && existing_outcomes.values().any(|o| o == "degraded")
}

/// RunCatchUp already-ran latch (round 4, 2026-07-10 — LOW): `true` when
/// BOTH feeds' daily rows already exist for the day with a TERMINAL
/// outcome — 'complete' OR 'feed_off' (round 5, 2026-07-10: on a
/// single-feed-profile day the OFF feed's row is permanently 'feed_off',
/// so requiring complete-on-both left the latch dead for exactly the
/// profile that runs all month with one feed off — every evening boot
/// re-ran and re-sent a duplicate card; a rerun also cannot "improve" a
/// feed_off row, so treating it terminal is safe). A post-trigger
/// same-day boot (the post-close deploy restart is the dominant shape)
/// then SKIPS the redundant re-run + duplicate Telegram card instead of
/// re-aggregating an already-vouched day. Partial days re-run (a rerun
/// may improve them); degraded days re-run under the daily keep-better
/// guard. Pure; the caller additionally requires zero in-market
/// boot-synthesized deaths (a real crash day must never skip its restart
/// floor) and never applies the latch to forced
/// (`TICKVAULT_SCOREBOARD_NOW`) runs.
#[must_use]
pub fn catchup_rerun_is_redundant(existing_outcomes: &BTreeMap<String, String>) -> bool {
    ["dhan", "groww"].iter().all(|feed| {
        existing_outcomes
            .get(*feed)
            .is_some_and(|o| o == "complete" || o == "feed_off")
    })
}

/// The `ws_event_audit.source` slug both feeds stamp on the
/// operator-toggle disable row (the Groww bridge's feed-disable falling
/// edge Disconnected row; the Dhan dormant-entry SleepEntered row since
/// round 5) — the durable marker that distinguishes a
/// runtime-disabled-for-the-day feed from an enabled-but-broker-dead one.
pub const FEED_DISABLE_SOURCE: &str = "feed_disabled";

/// Seconds-of-IST-day for an IST-epoch-nanos timestamp. Pure.
fn secs_of_day_ist_from_nanos(ts_ist_nanos: i64) -> i64 {
    ts_ist_nanos.div_euclid(NANOS_PER_SEC).rem_euclid(86_400)
}

/// `true` for a MARKET-DATA up-kind `ws_event_audit` row whose ts falls
/// INSIDE the market-hours window ([09:00, 15:30) IST) — the feed-off
/// inference's session-scoped up signal (round 5, 2026-07-10 — HIGH): every
/// config-enabled feed writes a boot Connected row at ~08:33, BEFORE the
/// API server can even accept a runtime toggle, so counting whole-day up
/// rows made the runtime-disable path the round-4 fix named essentially
/// unreachable (the 08:33 row always defeated `!had_any_up_row`).
///
/// PR-C1 (2026-07-13 — the Phase B "Honest obligation" acceptance
/// criterion, CLOSED): rows are additionally gated on
/// [`is_market_data_ws_type`] — the Q4-i-rewired ORDER-UPDATE WS (a
/// trading-channel socket that stays connected on the permanently-off Dhan
/// feed) writes Dhan-feed up-kind rows INSIDE the session, and without the
/// filter one such Reconnected row would defeat the round-6 `feed_off`
/// classification forever. Pure.
#[must_use]
pub fn is_session_up_row(ev: &WsAuditEventLite) -> bool {
    is_market_data_ws_type(&ev.ws_type)
        && is_up_kind(&ev.event_kind)
        && u32::try_from(secs_of_day_ist_from_nanos(ev.ts_ist_nanos))
            .is_ok_and(is_in_market_hours_secs)
}

/// `true` for a PRE-SESSION operator-toggle disable row
/// (`source='feed_disabled'` with ts before the 09:15 session open) — the
/// boot-connect-then-disable day's qualifier (round 5, 2026-07-10). The
/// pre-session bound is honesty-critical: an IN-session disable of a
/// zero-tick feed means the feed was ON when trading began and delivered
/// nothing — that prefix is a real measured-zero window, never softened
/// into feed_off. Pure.
#[must_use]
pub fn is_pre_session_feed_disable_row(ev: &WsAuditEventLite) -> bool {
    ev.source == FEED_DISABLE_SOURCE
        && secs_of_day_ist_from_nanos(ev.ts_ist_nanos) < SESSION_START_SECS_OF_DAY_IST
}

/// `true` for a MARKET-DATA up-kind row that lands BEFORE the 09:15 session
/// open — the state-at-open comparison's re-enable signal (round 6,
/// 2026-07-10 — MEDIUM): a pre-session disable→RE-ENABLE flap leaves both a
/// `feed_disabled` marker AND a later pre-session up row (Dhan
/// SleepResumed / Groww Connected); only the LAST of the two toggles is
/// the feed's state when trading began.
///
/// PR-C1 (2026-07-13): gated on [`is_market_data_ws_type`] — a pre-session
/// ORDER-UPDATE Connected row is trading-channel machinery, never a
/// market-data re-enable signal (see [`is_session_up_row`]). Pure.
#[must_use]
pub fn is_pre_session_up_row(ev: &WsAuditEventLite) -> bool {
    is_market_data_ws_type(&ev.ws_type)
        && is_up_kind(&ev.event_kind)
        && secs_of_day_ist_from_nanos(ev.ts_ist_nanos) < SESSION_START_SECS_OF_DAY_IST
}

/// State-at-session-open from the day's pre-session toggle timestamps
/// (round 6, 2026-07-10 — MEDIUM): the disable marker qualifies feed_off
/// ONLY when it is the LAST pre-session toggle event — a marker followed
/// by a later pre-session up row (the 08:40-disable / 08:50-re-enable
/// flap) means the feed was back ON at open, and a broker-dead session
/// after that flap is the honest catastrophic measured-zero day, never
/// softened into feed_off. Existence of a marker alone (the round-5
/// predicate) is NOT enough. An equal-timestamp tie resolves to NOT-off
/// (conservative: stays loud). Pure.
#[must_use]
pub fn pre_session_disable_is_state_at_open(
    latest_pre_session_disable_ts: Option<i64>,
    latest_pre_session_up_ts: Option<i64>,
) -> bool {
    match (latest_pre_session_disable_ts, latest_pre_session_up_ts) {
        (Some(disable_ts), Some(up_ts)) => disable_ts > up_ts,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Pairing window for [`parked_wake_indices`]: a WS-GAP-04 wake's
/// SleepResumed row and the dormant gate's re-park `feed_disabled` marker
/// land ~1s apart (wake ~09:00:00, marker ~09:00:01); 10s absorbs
/// scheduler jitter without pairing across genuine episodes.
pub const PARKED_WAKE_PAIR_WINDOW_NANOS: i64 = 10 * NANOS_PER_SEC;

/// Indices of `sleep_resumed` rows the dormant gate immediately RE-PARKED
/// (round 6, 2026-07-10 — MEDIUM): the WS-GAP-04 overnight wake emits its
/// SleepResumed audit row at ~09:00:00 IST BEFORE the run-loop's
/// feed_enabled gate parks the connection and stamps the
/// SleepEntered/`feed_disabled` marker (~09:00:01) — so a feed the
/// operator disabled WHILE SLEEPING writes one session-window "up" row
/// despite never streaming, defeating the feed-off inference. A
/// `sleep_resumed` row followed within [`PARKED_WAKE_PAIR_WINDOW_NANOS`]
/// by a `feed_disabled` marker for the SAME
/// (feed, ws_type, connection_index) is a parked wake, not evidence the
/// feed was up — the fold excludes it from BOTH up signals (session-window
/// and pre-session state-at-open). O(n·m) over one day's audit rows
/// (≤ hundreds; cold path, one run/day). Pure.
#[must_use]
pub fn parked_wake_indices(rows: &[WsAuditEventLite]) -> HashSet<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, wake)| wake.event_kind == "sleep_resumed")
        .filter(|(_, wake)| {
            rows.iter().any(|m| {
                m.source == FEED_DISABLE_SOURCE
                    && m.feed == wake.feed
                    && m.ws_type == wake.ws_type
                    && m.connection_index == wake.connection_index
                    && m.ts_ist_nanos >= wake.ts_ist_nanos
                    && m.ts_ist_nanos.saturating_sub(wake.ts_ist_nanos)
                        <= PARKED_WAKE_PAIR_WINDOW_NANOS
            })
        })
        .map(|(idx, _)| idx)
        .collect()
}

/// Feed-was-OFF-for-the-day inference (round 4, 2026-07-10 — the round-2
/// finding left open across rounds; REDESIGNED round 5, 2026-07-10 —
/// HIGH): a feed that delivered a MEASURED zero tick count with ZERO
/// up-kind rows inside the session window was switched off for the day —
/// its row must stamp the distinct 'feed_off' outcome (excluded from the
/// month sums) and the card must say "no contest" instead of declaring a
/// winner.
///
/// The round-5 redesign (the boot-Connected-row defeat): up rows are
/// SESSION-SCOPED ([`is_session_up_row`], [09:00, 15:30) IST) so the
/// ~08:33 boot connect no longer defeats the inference — but zero session
/// up rows ALONE cannot distinguish "runtime-disabled at 08:40" from
/// "enabled with a dead broker all day" (both leave the 08:33 connect and
/// nothing in session). The disambiguator (STATE-AT-OPEN since round 6,
/// 2026-07-10 — MEDIUM; existence of a marker was not enough) is the
/// durable pre-session `source='feed_disabled'` toggle row being the LAST
/// pre-session toggle ([`pre_session_disable_is_state_at_open`] over the
/// day's latest [`is_pre_session_feed_disable_row`] /
/// [`is_pre_session_up_row`] timestamps):
///
/// | day shape | any up | disable = state at open | verdict |
/// |---|---|---|---|
/// | config-off all day (no rows at all) | no | no | feed_off |
/// | boot connect 08:33 → runtime-disable 08:40 | yes | yes | feed_off |
/// | disable 08:40 → RE-ENABLE 08:50, broker dead in session | yes | no (the 08:50 up row is the last toggle) | NOT feed_off — the round-6 flap topology stays loud |
/// | boot connect 08:33, enabled, broker dead | yes | no | NOT feed_off — a real catastrophic measured-zero day stays loud |
///
/// The durable marker OUTRANKS the live runtime flag (round 6, the
/// re-enable-before-first-run window): when the disable marker is the
/// state at open with zero session up rows and zero ticks, a re-enable
/// between 15:30 and the 15:45 trigger (`runtime_enabled_now =
/// Some(true)`) no longer blocks feed_off — the flag is read at RUN time,
/// not session time. `Some(true)` stays load-bearing only for the
/// NO-MARKER arm (the enabled-but-broker-dead day, which has no
/// `feed_disabled` row). A backfill (`None`) infers from data alone
/// (honest residual: an enabled feed that never achieved a single
/// successful connect ALL day — zero rows entirely — is indistinguishable
/// from config-off in a backfill; the runbook names it). A `-1` ticks
/// sentinel never qualifies (unmeasured is not zero). Pure.
#[must_use]
pub fn is_feed_off_day(
    had_session_up_row: bool,
    had_any_up_row: bool,
    disable_is_state_at_open: bool,
    ticks: i64,
    runtime_enabled_now: Option<bool>,
) -> bool {
    if ticks != 0 || had_session_up_row {
        return false;
    }
    if disable_is_state_at_open {
        // The durable pre-session marker (last toggle before open) wins
        // over the run-instant flag — see the doc table above.
        return true;
    }
    !had_any_up_row && runtime_enabled_now != Some(true)
}

/// Daily-row feed_off keep-better rule (round 5, 2026-07-10 — HIGH,
/// erasure facet): `true` when writing this feed's row would UPGRADE an
/// existing 'feed_off' verdict to complete/partial while the rerun still
/// measured NO ticks for the feed (`ticks <= 0` — zero, or the `-1`
/// unmeasured sentinel, which cannot disprove off either). The scenario:
/// a config-off day correctly stamped feed_off, then a same-day evening
/// boot with the feed re-enabled for TOMORROW (a natural month-long-test
/// action) reruns with `runtime_enabled_now = Some(true)` and an evening
/// Connected row — the fresh inference says "not off" and the DEDUP
/// last-write-wins UPSERT would erase the feed_off row with
/// complete-with-zeros, re-crowning the false one-horse winner. A rerun
/// that measured REAL ticks (> 0) may upgrade — the feed genuinely
/// streamed, so the day was never a no-contest. Mirrors
/// [`should_keep_degraded_outcome`]; the caller logs
/// `stage="outcome_regression"`. Pure.
#[must_use]
pub fn should_keep_feed_off_outcome(
    existing_feed_outcome: Option<&str>,
    new_is_feed_off: bool,
    new_ticks: i64,
) -> bool {
    !new_is_feed_off && existing_feed_outcome == Some("feed_off") && new_ticks <= 0
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

/// Step 5: stamp each feed's `unique_win_minutes` / `both_minutes` from
/// the two session minute sets — UNLESS the PARTNER feed was off for the
/// day, in which case the comparison columns take the `-1` sentinel
/// (round 5, 2026-07-10 — MEDIUM): exclusive-vs-nothing is not a
/// measurement, and the running feed's ~375 "unique win" minutes on a
/// one-horse day flowed straight into the month verdict's headline
/// `sum(unique_win_minutes)` (the runbook's row-level
/// `outcome != 'feed_off'` filter removed only the OFF feed's row). The
/// DB row itself must not carry a fabricated competitive win — the `-1`
/// is skipped by the runbook's sentinel-guarded sums, and the month SQL
/// additionally excludes the whole day (day-level subquery, runbook §2).
/// Pure.
pub fn apply_minute_overlap_and_feed_off_sentinels(
    feed_numbers: &mut BTreeMap<&'static str, FeedDayNumbers>,
    minute_sets: &BTreeMap<&'static str, Option<HashSet<String>>>,
    feed_off: &BTreeMap<&'static str, bool>,
) {
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
    for (feed, partner) in [("dhan", "groww"), ("groww", "dhan")] {
        if feed_off.get(partner).copied().unwrap_or(false)
            && let Some(n) = feed_numbers.get_mut(feed)
        {
            n.unique_win_minutes = SCOREBOARD_UNAVAILABLE_SENTINEL;
            n.both_minutes = SCOREBOARD_UNAVAILABLE_SENTINEL;
        }
    }
}

// Scoreboard PR-D presence-coverage fold — RETIRED 2026-07-18 (stage-4
// dead-producer sweep): `apply_presence_coverage`,
// `presence_drain_one_sided_feed` and `build_coverage_detail_rows` were
// deleted with the in-memory presence registry
// (crates/core/src/pipeline/feed_presence.rs) — producer-less since the
// live feeds retired (Dhan 2026-07-13, Groww 2026-07-15). The
// `coverage_source` schema, the rank table and the 6d coverage keep-better
// survive so historical `in_memory`/`mixed` rows stay protected; new runs
// stamp the honest `sql_backfill` approximation.

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
///
/// Maintenance note (PR-C1, 2026-07-13 hostile-review L3): this is an
/// ALLOWLIST — a future feed's market-data ws_type (e.g. the GDF trial's
/// `gdf_feed`, `gdf-third-feed-scope-2026-07-13.md`) MUST be added here, or
/// its up rows are excluded from the feed_off / any_up classification and
/// the day misreads `feed_off` despite live GDF rows.
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

/// Day-filter for THIS boot's reconciled rows (round 4, 2026-07-10 —
/// HIGH): the reconciler always targets the BOOT day, but a
/// `TICKVAULT_SCOREBOARD_DATE` backfill retargets a PAST day — folding
/// today's synthesized in-market process-death rows into the past day's
/// tallies incremented the past day's restarts/blame and (via the
/// data-driven floor) flipped its previously-correct completed row to
/// Partial (deterministic-ts UPSERT is last-write-wins). Only rows whose
/// ts lies inside the TARGET day's bounds may fold in-memory or key the
/// read-back dedupe. Pure.
#[must_use]
pub fn filter_boot_rows_to_day(
    rows: &[FeedEpisodeAuditRow],
    target_ist_day: u64,
) -> Vec<FeedEpisodeAuditRow> {
    let (start, end) = day_bounds_nanos(target_ist_day);
    rows.iter()
        .filter(|r| r.ts_ist_nanos >= start && r.ts_ist_nanos < end)
        .cloned()
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
    /// `false` when the evidence CANNOT be complete: an I/O failure (the
    /// log directory was unreadable / a file open failed) OR — applied by
    /// the caller via [`target_within_evidence_retention`] — the target
    /// day is older than the ~48h errors.jsonl retention horizon (round-4
    /// hostile review 2026-07-10: a readable dir whose retained files
    /// simply do not COVER an aged-out backfill day previously reported
    /// `true`, silently bypassing BOTH `run_partial` and the keep-better
    /// read). Episodes classified from partial evidence are stamped
    /// `run_partial` (runbook note; 805 defaults broker, RSTs default
    /// indeterminate).
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

/// errors.jsonl hourly files are retained ~48h (the observability
/// retention sweeper) — the correlation-evidence horizon in IST days. A
/// target day older than this cannot be COVERED by the retained files
/// even when every file opens cleanly, so I/O success alone must never
/// claim complete evidence (round-4 hostile review 2026-07-10).
///
/// WHY 1 AND NOT 2 (round-5 hostile review 2026-07-10 — the boundary
/// off-by-one): the sweeper (`observability.rs::sweep_errors_jsonl_retention`,
/// hourly cadence, retention_hours = 48) deletes by file MTIME — any hourly
/// file older than `now − 48h` is gone. Full-day coverage of a target day
/// therefore requires `cutoff = now − 48h ≤ target_day_start`, i.e.
/// `now ≤ target_day_start + 48h`. For target = today−1 that bound is
/// `today_start + 24h` — satisfied at EVERY instant of today, so
/// yesterday's 24 hourly files always survive. For target = today−2 the
/// bound is `today_start` itself — satisfied only at exactly midnight, and
/// the SESSION hours (the RESILIENCE-01 / WS-GAP-09 / PROC / RESOURCE
/// evidence window, mtime ≈ 10:00–16:00 IST of D−2) are swept for any run
/// after ~10:00 today. A `2` here let a D−2 backfill claim complete
/// evidence over a readable-but-empty dir, skip the keep-better read, and
/// destructively re-stamp evidence-backed blame `run_partial=false`.
pub const ERRORS_JSONL_EVIDENCE_RETENTION_DAYS: u64 = 1;

/// Round-4 evidence-horizon gate (2026-07-10 — HIGH): `true` when the
/// retained ~48h of errors.jsonl files can still COVER the target day
/// (`target_ist_day + 1 >= today_ist_day` — the mtime-sweep derivation on
/// [`ERRORS_JSONL_EVIDENCE_RETENTION_DAYS`]; round 5 tightened 2 → 1
/// because a D−2 target is never fully covered by an on-day run). The
/// scan's I/O-level
/// `complete` flag AND this horizon TOGETHER decide `scan_complete` —
/// gating only on I/O success made the round-3 keep-better guard vacuous
/// for its flagship scenario (the >48h month-end backfill: readable dir,
/// zero covering events, every re-classified row falsely stamped
/// `run_partial=false`, the evidence-backed blame destructively
/// overwritten with no `blame_regression` log). Pure.
#[must_use]
pub fn target_within_evidence_retention(target_ist_day: u64, today_ist_day: u64) -> bool {
    target_ist_day.saturating_add(ERRORS_JSONL_EVIDENCE_RETENTION_DAYS) >= today_ist_day
}

/// Scan the errors.jsonl directory (blocking file I/O — callers wrap in
/// `spawn_blocking`). Missing dir / unreadable files degrade to
/// `scan_complete = false` evidence — fail-soft, never a panic. NOTE: the
/// caller must ALSO apply [`target_within_evidence_retention`] — this scan
/// reports I/O completeness only and cannot know whether the retained
/// files cover the target day (round 4, 2026-07-10). O(≤48h of
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

/// Map a `ws_event_audit.event_kind` (+ its `source` for stall rows) to a
/// scoreboard episode kind. Pure.
/// Connect / reconnect / sleep kinds are lifecycle, not episodes → `None`.
///
/// Scoreboard PR-B (2026-07-10): a `stall_restarted` row (the Groww stall
/// watchdog's kill+relaunch — NOT an up-kind and NOT a plain disconnect)
/// maps to the stall episode kinds; the `source` slug splits the
/// never-streamed arm (`stall_never_streamed`) from the classic /
/// auth-stale / entitlement causes, all of which are `stall_restart`.
#[must_use]
pub fn episode_kind_for_event(event_kind: &str, source: &str) -> Option<&'static str> {
    match event_kind {
        "disconnected" => Some(EPISODE_KIND_DISCONNECT),
        "disconnected_off_hours" => Some(EPISODE_KIND_OFF_HOURS_DISCONNECT),
        // The literal is pinned against WsEventKind::StallRestarted.as_str()
        // by test_stall_restarted_literal_matches_event_kind.
        "stall_restarted" => Some(
            if source == tickvault_common::feed_blame::STALL_SOURCE_NEVER_STREAMED {
                EPISODE_KIND_NEVER_STREAMED_RESTART
            } else {
                EPISODE_KIND_STALL_RESTART
            },
        ),
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
    let episode_kind = episode_kind_for_event(&ev.event_kind, &ev.source)?;
    // Scoreboard PR-B (2026-07-10): a stall row's `source` carries the
    // FIXED cause slug (`stall_silent_socket` / `stall_never_streamed` /
    // `stall_auth_stale` / `stall_entitlement`) — thread it into the
    // classifier's `stall_reason` so the stall arms (broker silent-socket /
    // never-streamed / entitlement vs ours token-minter-stale) activate.
    let is_stall = matches!(
        episode_kind,
        EPISODE_KIND_STALL_RESTART | EPISODE_KIND_NEVER_STREAMED_RESTART
    );
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
        stall_reason: if is_stall { &ev.source } else { "" },
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
        detector: if is_stall {
            "stall_row"
        } else {
            "ws_event_audit"
        },
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

/// A post-close reconnect gets the CLEAN scheduled-stop carve-out only
/// when the feed streamed through the session close — its last streamed
/// session minute is at/after this IST seconds-of-day threshold (15:28;
/// two minutes of slack for end-of-session tick sparsity). A last
/// streamed minute BEFORE this is a HOLE before close — a clean 15:30
/// close cannot leave one, so the death was a REAL in-market crash
/// (round 4, 2026-07-10: a 15:26 crash whose 429-cooldown reconnect
/// landed at ~15:31 was carved out, rendered "restarts: 0" and let the
/// day stamp outcome='complete').
pub const POST_CLOSE_CLEAN_STOP_MIN_LAST_MINUTE_SECS: i64 = 15 * 3600 + 28 * 60; // 55_680

/// Latest streamed session minute as IST seconds-of-day, parsed from the
/// opaque minute keys (`YYYY-MM-DDTHH:MM:…` — the ts column stores IST
/// wall-clock per data-integrity.md, so the HH:MM substring IS the IST
/// time of day). `None` = no session minute / unparsable keys. Pure.
#[must_use]
pub fn last_minute_secs_of_day(minutes: &HashSet<String>) -> Option<i64> {
    minutes
        .iter()
        .filter_map(|m| {
            let h: i64 = m.get(11..13)?.parse().ok()?;
            let mi: i64 = m.get(14..16)?.parse().ok()?;
            Some(h * 3600 + mi * 60)
        })
        .max()
}

/// Round-4 post-close disambiguation (2026-07-10 — MEDIUM): `true` when a
/// synthesized death whose reconnect landed at/after 15:30 IST was a REAL
/// in-market crash after all — the feed's last streamed session minute
/// leaves a hole before the close (`< 15:28`), which a clean scheduled
/// stop cannot produce. Only a feed that streamed through ~15:28+ keeps
/// the scheduled-stop carve-out. `None` (zero streamed session minutes
/// with a prior-up row) is ALSO incompatible with a clean full-session
/// stop and classifies real (honest residuals, BOTH named in the runbook
/// §3 `post_close_restart` row: a feed whose broker was silent the entire
/// day then cleanly stopped mis-classifies — the crash-before-first-tick
/// day dominates; and the SILENT-TAIL variant — a broker tick-silent from
/// before ~15:28 with the socket up (pings keep the watchdog quiet, no
/// disconnect row) then cleanly 16:30-stopped + evening-restarted produces
/// the same `< 15:28` shape and self-blames a process that survived to the
/// scheduled stop — conservative error direction, cross-check the
/// FEED-STALL signals before signing; the per-SID tick-gap detector
/// retired in PR-C3, 2026-07-14). Pure.
#[must_use]
pub fn post_close_reconnect_is_real_death(last_streamed_minute_secs: Option<i64>) -> bool {
    match last_streamed_minute_secs {
        Some(m) => m < POST_CLOSE_CLEAN_STOP_MIN_LAST_MINUTE_SECS,
        None => true,
    }
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
///
/// PR-B fix round 1 (2026-07-10 review HIGH): `stall_restarted` rows are
/// TRANSPARENT here — see [`synthesize_process_death_episodes`] (the two
/// last-pre trackers move in lockstep so the gate and the synthesis agree
/// on which keys are pairing candidates).
#[must_use]
pub fn post_boot_pairing_complete(rows: &[WsAuditEventLite], boot_ts_ist_nanos: i64) -> bool {
    let mut state: BTreeMap<(&str, &str, i64), (Option<&str>, bool)> = BTreeMap::new();
    for ev in rows {
        let key = (ev.feed.as_str(), ev.ws_type.as_str(), ev.connection_index);
        let entry = state.entry(key).or_insert((None, false));
        if ev.ts_ist_nanos < boot_ts_ist_nanos {
            // Lockstep with synthesize_process_death_episodes: a stall
            // kill+relaunch row never occupies the last-pre slot (the
            // literal is pinned by test_stall_restarted_literal_matches_event_kind).
            if ev.event_kind != "stall_restarted" {
                entry.0 = Some(ev.event_kind.as_str());
            }
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
///
/// Stall-row transparency (PR-B fix round 1, 2026-07-10 review HIGH): a
/// `stall_restarted` row is a MACHINERY artifact — the Groww stall
/// watchdog's kill+relaunch restores streaming WITHOUT a fresh up-kind
/// row (the bridge's Connected/Reconnected latches re-arm only on
/// feed-disable / bridge-death falling edges; a sidecar kill is invisible
/// to them). Letting it occupy the last-pre-boot slot acted as a DOWN
/// marker: after the day's FIRST stall kill, a later in-market process
/// death on that key was silently dropped (`!is_up_kind → continue`) and
/// the boot-only synthesized episode was permanently lost. Stall rows are
/// therefore SKIPPED when tracking the last-pre-boot row (the
/// [`parked_wake_indices`] transparent-row precedent) — a genuine down
/// row (`disconnected` / `sleep_entered`) still occupies the slot, so the
/// skip can never resurrect a genuinely-down key.
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
            // A stall kill+relaunch row is TRANSPARENT for pairing (see
            // the fn doc; literal pinned by
            // test_stall_restarted_literal_matches_event_kind).
            if ev.event_kind == "stall_restarted" {
                continue;
            }
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
    let mut deaths = synthesize_process_death_episodes(&rows, boot_ts_ist_nanos);
    if deaths.is_empty() {
        info!("feed_scoreboard: process-death reconciler found no gaps (clean boot)");
        return Vec::new();
    }
    // Round-4 post-close disambiguation (2026-07-10 — MEDIUM): the round-3
    // carve-out over-corrected — a REAL in-market crash whose reconnect
    // lands at/after 15:30 (the 429-cooldown 15:26-crash shape, or any
    // delayed recovery) rendered "restarts: 0" and let the day stamp
    // complete. The day's per-feed streamed-minute set disambiguates: a
    // clean scheduled stop streams through the close, a crash leaves a
    // hole before it. Query failure keeps the carve-out (ambiguous —
    // loudly logged), never a phantom in-market death on an I/O blip.
    // Only the KNOWN feed labels may reach the interpolated SQL literal
    // (the module's no-user-input-in-SQL contract — the feed strings here
    // come back from ws_event_audit rows, not from Feed::ALL).
    let post_close_feeds: std::collections::BTreeSet<String> = deaths
        .iter()
        .filter(|d| d.post_close_restart)
        .map(|d| d.feed.clone())
        .filter(|f| {
            tickvault_common::feed::Feed::ALL
                .iter()
                .any(|known| known.as_str() == f)
        })
        .collect();
    for feed in post_close_feeds {
        let minutes_sql = build_feed_session_minutes_sql(&feed, target_ist_day);
        let last = match exec_query(&client, questdb, &minutes_sql).await {
            Some(body) => match parse_minute_set(&body) {
                Some(set) => last_minute_secs_of_day(&set),
                None => {
                    error!(
                        code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                        stage = "post_close_disambiguation",
                        feed = %feed,
                        "SCOREBOARD-01: post-close disambiguation minute read \
                         unparsable — keeping the scheduled-stop carve-out \
                         (a real crash recovered post-close may render \
                         restarts=0 this boot)"
                    );
                    continue;
                }
            },
            None => {
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "post_close_disambiguation",
                    feed = %feed,
                    "SCOREBOARD-01: post-close disambiguation minute read \
                     failed — keeping the scheduled-stop carve-out (a real \
                     crash recovered post-close may render restarts=0 this \
                     boot)"
                );
                continue;
            }
        };
        if post_close_reconnect_is_real_death(last) {
            warn!(
                feed = %feed,
                last_streamed_minute_secs = last,
                "feed_scoreboard: post-close reconnect but the feed's last \
                 streamed minute leaves a hole before the 15:30 close — a \
                 clean scheduled stop cannot do that; reclassifying the \
                 synthesized death as a REAL in-market process death \
                 (round 4, 2026-07-10)"
            );
            for d in deaths
                .iter_mut()
                .filter(|d| d.post_close_restart && d.feed == feed)
            {
                d.post_close_restart = false;
                d.market_hours = true;
            }
        }
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
// REST 1m pull digest (Groww REST plan PR-5 — operator Quote 2 2026-07-13:
// "always clearly note within a second — or within how many seconds
// precisely — we are fetching this live real OHLCV, along with the option
// chain API"). Aggregates the day's `rest_fetch_audit` per-fetch forensics
// rows per (feed, leg) — pull outcome counts, rate-limit hits, and the
// close→data freshness distribution — plus a latency-only fallback from the
// `spot_1m_rest` per-row `close_to_data_ms` column for feeds whose per-fetch
// forensics emits have not landed yet (the Dhan spot leg today — its audit
// emit sites are the flagged follow-up in rest-1m-pipeline-error-codes.md).
// Pure aggregation, O(day rows ≤ ~3K) — flagged O(N), cold, once per day.
// ---------------------------------------------------------------------------

/// A pull retrieved this many ms (or more) after its minute closed is a
/// LATE recovery — a later fire's backfill / the post-close sweep repaired
/// it, not the prompt in-minute ladder (the #1499 semantics split: prompt
/// ladders finish well inside the minute by construction). Late repairs are
/// counted separately and EXCLUDED from the prompt freshness distribution
/// so one 2-hour sweep repair can never skew the "how fast after close"
/// answer.
pub const REST_LEG_LATE_RECOVERY_MS: i64 = 60_000;

/// One parsed `rest_fetch_audit` row (the digest's working subset). Pure
/// data; rows with unexpected shapes are skipped by the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestFetchLiteRow {
    pub feed: String,
    pub leg: String,
    pub outcome: String,
    /// Minute close → retrieval ms; `-1` sentinel on non-ok rows.
    pub close_to_data_ms: i64,
    /// 429 attempts inside this fetch's ladder.
    pub rate_limited_count: i64,
    /// The row's error-class slug (`pre_boot` splits restart-window rows
    /// out of "never recovered" — Fix E, 2026-07-17); empty when absent.
    pub error_class: String,
}

/// One (feed, leg) day aggregate for the scorecard digest. `-1` = that
/// number has no measurement source (never a fabricated zero — Rule 11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestLegDaySummary {
    /// Wire feed slug (`dhan` / `groww`).
    pub feed: String,
    /// Wire leg slug (`spot_1m` / `chain_1m` / `contract_1m`).
    pub leg: String,
    /// Fetches whose target minute was retrieved (`outcome='ok'`).
    pub ok_fetches: i64,
    /// Fetches that ended without the target minute (every non-ok,
    /// non-named-gap outcome: empty / error / rate_limited / no_token /
    /// skipped).
    pub failed_fetches: i64,
    /// Finally-unrecovered minutes (`outcome='named_gap'`, every class
    /// EXCEPT `pre_boot` — Fix E, 2026-07-17).
    pub named_gaps: i64,
    /// `named_gap` rows classed `pre_boot` — minutes from before this
    /// process started (audit bookkeeping, never a pull failure).
    pub pre_boot_gaps: i64,
    /// Sum of 429 attempts across the day's fetches.
    pub rate_limited_hits: i64,
    /// Ok rows retrieved ≥ [`REST_LEG_LATE_RECOVERY_MS`] after close —
    /// repaired by a later fire / the sweep, honest real delay.
    pub late_recovered: i64,
    /// Prompt-pull close→data distribution (nearest-rank percentiles over
    /// ok rows below the late threshold); `-1` when no prompt sample.
    pub close_p50_ms: i64,
    pub close_p99_ms: i64,
    pub close_max_ms: i64,
    /// Prompt samples the distribution is built from; `0` = measured zero
    /// (counts exist but every pull failed); `-1` = no latency source.
    pub close_samples: i64,
}

/// The day's `rest_fetch_audit` rows (the digest's working subset). Micros
/// literals — the ONLY representation legal in an embedded QuestDB
/// TIMESTAMP comparison (the module regression lock).
#[must_use]
pub fn build_rest_fetch_audit_day_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select feed, leg, outcome, close_to_data_ms, rate_limited_count, \
         error_class from rest_fetch_audit where ts >= {start} and ts < {end}"
    )
}

/// The day's per-row `spot_1m_rest.close_to_data_ms` values per feed — the
/// latency-only fallback source for feeds without per-fetch forensics rows
/// yet (the Dhan spot leg). Only measured (>= 0) values are fetched; the
/// -1 sentinel rows (unmeasured) never reach the distribution. Micros
/// literals.
#[must_use]
pub fn build_spot1m_close_latency_day_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select feed, close_to_data_ms from spot_1m_rest \
         where ts >= {start} and ts < {end} and close_to_data_ms >= 0"
    )
}

/// Parse the [`build_rest_fetch_audit_day_sql`] response. Pure; `None` =
/// unparsable body; rows with unexpected shapes are SKIPPED (best-effort),
/// never a panic.
#[must_use]
pub fn parse_rest_fetch_audit_lite(body: &str) -> Option<Vec<RestFetchLiteRow>> {
    let rows = parse_dataset(body)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let cols = match row.as_array() {
            Some(c) if c.len() >= 5 => c,
            _ => continue,
        };
        let (Some(feed), Some(leg), Some(outcome)) =
            (cols[0].as_str(), cols[1].as_str(), cols[2].as_str())
        else {
            continue;
        };
        out.push(RestFetchLiteRow {
            feed: feed.to_string(),
            leg: leg.to_string(),
            outcome: outcome.to_string(),
            close_to_data_ms: cols[3].as_i64().unwrap_or(-1),
            rate_limited_count: cols[4].as_i64().unwrap_or(0),
            error_class: cols
                .get(5)
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string(),
        });
    }
    Some(out)
}

/// Parse the [`build_spot1m_close_latency_day_sql`] response into
/// `(feed, close_to_data_ms)` pairs. Pure; `None` = unparsable body.
#[must_use]
pub fn parse_feed_close_latency_rows(body: &str) -> Option<Vec<(String, i64)>> {
    let rows = parse_dataset(body)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(cols) = row.as_array()
            && cols.len() >= 2
            && let (Some(feed), Some(ms)) = (cols[0].as_str(), cols[1].as_i64())
        {
            out.push((feed.to_string(), ms));
        }
    }
    Some(out)
}

/// Nearest-rank percentile over an ASCENDING-sorted slice; `-1` on empty
/// input (the honest "no sample" sentinel). Pure.
#[must_use]
pub fn percentile_nearest_rank(sorted_asc: &[i64], pct: u32) -> i64 {
    if sorted_asc.is_empty() {
        return SCOREBOARD_UNAVAILABLE_SENTINEL;
    }
    let n = sorted_asc.len();
    let rank = n
        .saturating_mul(pct.min(100) as usize)
        .div_ceil(100)
        .clamp(1, n);
    sorted_asc[rank - 1]
}

/// Fold one leg's prompt-latency samples into the percentile columns.
fn fill_latency_distribution(s: &mut RestLegDaySummary, mut prompt_ms: Vec<i64>) {
    prompt_ms.sort_unstable();
    s.close_samples = i64::try_from(prompt_ms.len()).unwrap_or(i64::MAX);
    s.close_p50_ms = percentile_nearest_rank(&prompt_ms, 50);
    s.close_p99_ms = percentile_nearest_rank(&prompt_ms, 99);
    s.close_max_ms = prompt_ms
        .last()
        .copied()
        .unwrap_or(SCOREBOARD_UNAVAILABLE_SENTINEL);
}

/// Aggregate the day's per-fetch forensics rows (+ the spot latency
/// fallback pairs) into per-(feed, leg) digest summaries. Pure,
/// deterministic order (feed, then leg). Semantics:
///
/// - `ok` rows below [`REST_LEG_LATE_RECOVERY_MS`] feed the prompt
///   freshness distribution; at/above it they count `late_recovered`
///   (repaired later — the real delay stays honest, out of the prompt
///   distribution).
/// - `named_gap` rows count separately (never a "failed fetch" — they are
///   the finally-unrecovered minutes).
/// - Every other outcome counts `failed_fetches`; `rate_limited_count`
///   sums across ALL rows.
/// - A feed with a `spot_1m` data-table latency row but NO audit rows for
///   that leg gets a latency-only summary (counts stay `-1` — that source
///   records no outcomes). The same late split applies (backfilled data
///   rows carry the real > 60s repair delay per the #1499 semantics).
#[must_use]
pub fn aggregate_rest_leg_day(
    audit_rows: &[RestFetchLiteRow],
    spot_latency_fallback: &[(String, i64)],
) -> Vec<RestLegDaySummary> {
    let mut map: BTreeMap<(String, String), (RestLegDaySummary, Vec<i64>)> = BTreeMap::new();
    for r in audit_rows {
        let key = (r.feed.clone(), r.leg.clone());
        let entry = map.entry(key).or_insert_with(|| {
            (
                RestLegDaySummary {
                    feed: r.feed.clone(),
                    leg: r.leg.clone(),
                    ok_fetches: 0,
                    failed_fetches: 0,
                    named_gaps: 0,
                    pre_boot_gaps: 0,
                    rate_limited_hits: 0,
                    late_recovered: 0,
                    close_p50_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
                    close_p99_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
                    close_max_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
                    close_samples: 0,
                },
                Vec::new(),
            )
        });
        let (s, prompt) = entry;
        s.rate_limited_hits = s
            .rate_limited_hits
            .saturating_add(r.rate_limited_count.max(0));
        match r.outcome.as_str() {
            "ok" => {
                s.ok_fetches = s.ok_fetches.saturating_add(1);
                if r.close_to_data_ms >= 0 {
                    if r.close_to_data_ms >= REST_LEG_LATE_RECOVERY_MS {
                        s.late_recovered = s.late_recovered.saturating_add(1);
                    } else {
                        prompt.push(r.close_to_data_ms);
                    }
                }
            }
            // Fix E: `pre_boot`-classed rows are restart-window
            // bookkeeping, never "never recovered" pull failures.
            "named_gap" if r.error_class == "pre_boot" => {
                s.pre_boot_gaps = s.pre_boot_gaps.saturating_add(1);
            }
            "named_gap" => s.named_gaps = s.named_gaps.saturating_add(1),
            _ => s.failed_fetches = s.failed_fetches.saturating_add(1),
        }
    }
    // Latency-only fallback: a feed whose spot leg persisted measured
    // close→data values but wrote NO audit rows (the Dhan spot leg until
    // its forensics follow-up lands) still answers Quote 2 — with counts
    // honestly left at the -1 sentinel.
    let mut fallback: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (feed, ms) in spot_latency_fallback {
        if *ms >= 0 {
            fallback.entry(feed.clone()).or_default().push(*ms);
        }
    }
    for (feed, values) in fallback {
        let key = (feed.clone(), "spot_1m".to_string());
        if map.contains_key(&key) {
            continue; // the per-fetch forensics source wins
        }
        let mut s = RestLegDaySummary {
            feed,
            leg: "spot_1m".to_string(),
            ok_fetches: SCOREBOARD_UNAVAILABLE_SENTINEL,
            failed_fetches: SCOREBOARD_UNAVAILABLE_SENTINEL,
            named_gaps: SCOREBOARD_UNAVAILABLE_SENTINEL,
            pre_boot_gaps: SCOREBOARD_UNAVAILABLE_SENTINEL,
            rate_limited_hits: SCOREBOARD_UNAVAILABLE_SENTINEL,
            late_recovered: 0,
            close_p50_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
            close_p99_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
            close_max_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
            close_samples: 0,
        };
        let prompt: Vec<i64> = values
            .iter()
            .copied()
            .filter(|ms| {
                if *ms >= REST_LEG_LATE_RECOVERY_MS {
                    s.late_recovered = s.late_recovered.saturating_add(1);
                    false
                } else {
                    true
                }
            })
            .collect();
        fill_latency_distribution(&mut s, prompt);
        map.insert(key, (s, Vec::new()));
    }
    map.into_values()
        .map(|(mut s, prompt)| {
            if !prompt.is_empty() {
                fill_latency_distribution(&mut s, prompt);
            }
            s
        })
        .collect()
}

/// Static gauge-label allowlist for the digest's dashboard gauge — metric
/// labels must never carry unbounded parsed strings. `None` = an
/// unexpected slug (skipped, defensive).
#[must_use]
pub fn rest_leg_gauge_labels(feed: &str, leg: &str) -> Option<(&'static str, &'static str)> {
    let f = match feed {
        "dhan" => "dhan",
        "groww" => "groww",
        _ => return None,
    };
    let l = match leg {
        "spot_1m" => "spot_1m",
        "chain_1m" => "chain_1m",
        "contract_1m" => "contract_1m",
        _ => return None,
    };
    Some((f, l))
}

/// Map the day summaries into the Telegram card's digest lines. The FOUR
/// canonical (feed, leg) pairs ALWAYS render — the operator's Quote-2
/// answer must exist even as an honest "not measured yet" — and any EXTRA
/// measured pair (the PR-4 contract leg, once its forensics rows land)
/// lights up automatically after them. Wire slugs are mapped to plain
/// English here so no slug ever reaches Telegram (commandment 2).
#[must_use]
pub fn build_rest_leg_score_lines(
    summaries: &[RestLegDaySummary],
) -> Vec<tickvault_core::notification::events::RestLegScoreLine> {
    use tickvault_core::notification::events::RestLegScoreLine;
    let feed_display = |f: &str| match f {
        "dhan" => "Dhan".to_string(),
        "groww" => "Groww".to_string(),
        other => other.to_string(),
    };
    let leg_display = |l: &str| match l {
        "spot_1m" => "spot candles".to_string(),
        "chain_1m" => "option chain".to_string(),
        "contract_1m" => "option contracts".to_string(),
        other => other.to_string(),
    };
    let to_line = |feed: &str, leg: &str, s: Option<&RestLegDaySummary>| -> RestLegScoreLine {
        let sent = SCOREBOARD_UNAVAILABLE_SENTINEL;
        match s {
            Some(s) => RestLegScoreLine {
                feed: feed_display(feed),
                leg: leg_display(leg),
                ok_fetches: s.ok_fetches,
                failed_fetches: s.failed_fetches,
                named_gaps: s.named_gaps,
                pre_boot_gaps: s.pre_boot_gaps,
                rate_limited_hits: s.rate_limited_hits,
                late_recovered: s.late_recovered,
                close_p50_ms: s.close_p50_ms,
                close_p99_ms: s.close_p99_ms,
                close_max_ms: s.close_max_ms,
                close_samples: s.close_samples,
            },
            None => RestLegScoreLine {
                feed: feed_display(feed),
                leg: leg_display(leg),
                ok_fetches: sent,
                failed_fetches: sent,
                named_gaps: sent,
                pre_boot_gaps: sent,
                rate_limited_hits: sent,
                late_recovered: sent,
                close_p50_ms: sent,
                close_p99_ms: sent,
                close_max_ms: sent,
                close_samples: sent,
            },
        }
    };
    const CANONICAL: [(&str, &str); 4] = [
        ("dhan", "spot_1m"),
        ("dhan", "chain_1m"),
        ("groww", "spot_1m"),
        ("groww", "chain_1m"),
    ];
    let mut out = Vec::with_capacity(summaries.len().max(4));
    for (feed, leg) in CANONICAL {
        let s = summaries.iter().find(|s| s.feed == feed && s.leg == leg);
        out.push(to_line(feed, leg, s));
    }
    for s in summaries {
        if !CANONICAL.contains(&(s.feed.as_str(), s.leg.as_str())) {
            out.push(to_line(&s.feed, &s.leg, Some(s)));
        }
    }
    // G6 (fix round 2): the F3 not-measured `info!` moved OUT of this
    // builder into the AGGREGATION path (`log_rest_leg_measurement_gaps`,
    // called right after `aggregate_rest_leg_day`) — this builder was
    // only invoked on the telegram-enabled branch of main.rs, so on a
    // telegram-disabled boot the §2b always-render contract was satisfied
    // neither on the card nor in logs.
    out
}

/// The CANONICAL card pairs whose lines carry MEASURED latency but NO
/// pull counts (`ok_fetches == -1`, `close_samples > 0`) — the documented
/// `spot_1m_rest` latency-only fallback source (a forensics-writer outage
/// window or a pre-2026-07-14 backfill day). These pairs RENDER count-less
/// on the card (G6, fix round 2) and are logged as latency-only — never
/// as "not measured" (that log statement was false for exactly this arm).
#[must_use]
pub fn latency_only_canonical_rest_pairs(
    lines: &[tickvault_core::notification::events::RestLegScoreLine],
) -> Vec<(String, String)> {
    CANONICAL_DISPLAY
        .iter()
        .filter(|(feed, leg)| {
            let counts_measured = lines
                .iter()
                .any(|l| l.feed == *feed && l.leg == *leg && l.ok_fetches >= 0);
            let latency_measured = lines
                .iter()
                .any(|l| l.feed == *feed && l.leg == *leg && l.close_samples > 0);
            !counts_measured && latency_measured
        })
        .map(|(feed, leg)| ((*feed).to_string(), (*leg).to_string()))
        .collect()
}

/// F3 log-side always-render contract, re-homed to the AGGREGATION path
/// (G6, fix round 2) so it fires even when the scorecard Telegram is
/// disabled — one truthful `info!` per canonical pair that is either
/// wholly unmeasured (nothing on the card) or latency-only (renders a
/// count-less delay segment). See the RULE-CONTRACT TENSION note on
/// `unmeasured_canonical_rest_pairs`.
pub fn log_rest_leg_measurement_gaps(summaries: &[RestLegDaySummary]) {
    let lines = build_rest_leg_score_lines(summaries);
    for (feed, leg) in unmeasured_canonical_rest_pairs(&lines) {
        info!(
            %feed,
            %leg,
            "rest-leg digest: {feed}/{leg} not measured — omitted from the \
             scorecard card per the 2026-07-15 cleanliness directive; the \
             always-render contract is satisfied in logs pending the \
             rule-file supersession"
        );
    }
    for (feed, leg) in latency_only_canonical_rest_pairs(&lines) {
        info!(
            %feed,
            %leg,
            "rest-leg digest: {feed}/{leg} latency measured, pull counts \
             missing (the spot_1m_rest latency-only fallback source) — \
             renders a count-less delay segment on the scorecard card"
        );
    }
}

/// The four canonical card pairs (display names) the §2b contract names.
const CANONICAL_DISPLAY: [(&str, &str); 4] = [
    ("Dhan", "spot candles"),
    ("Dhan", "option chain"),
    ("Groww", "spot candles"),
    ("Groww", "option chain"),
];

/// The CANONICAL card pairs (Dhan/Groww × spot candles/option chain) with
/// NOTHING measured today — no pull counts AND no latency samples. Pure so
/// the F3 not-measured logging is unit-testable.
///
/// G6 (fix round 2): a pair on the documented `spot_1m_rest` latency-only
/// fallback (`ok_fetches == -1`, `close_samples > 0`) is MEASURED — it was
/// previously misclassified unmeasured here (a false log statement while
/// its real latency sat in the same lines vec) and simultaneously dropped
/// from the card. Latency-only pairs are named separately by
/// [`latency_only_canonical_rest_pairs`].
#[must_use]
pub fn unmeasured_canonical_rest_pairs(
    lines: &[tickvault_core::notification::events::RestLegScoreLine],
) -> Vec<(String, String)> {
    CANONICAL_DISPLAY
        .iter()
        .filter(|(feed, leg)| {
            !lines.iter().any(|l| {
                l.feed == *feed && l.leg == *leg && (l.ok_fetches >= 0 || l.close_samples > 0)
            })
        })
        .map(|(feed, leg)| ((*feed).to_string(), (*leg).to_string()))
        .collect()
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
    /// Day exchange→receipt lag distribution (scoreboard PR-C) — drained
    /// from the in-memory per-feed day histograms on SAME-DAY runs only.
    /// `-1` = not measured (past-day backfill — the histograms are
    /// process-local and day-scoped, so a backfill can never read them; a
    /// thin day below the 50-sample floor also stays `-1`, never a
    /// fabricated distribution). A rerun that measured nothing folds the
    /// day's EXISTING measured row forward instead of writing `-1` over it
    /// (the step-6c lag keep-better — PR-C review round 1, 2026-07-11).
    pub lag_p50_ms: i64,
    pub lag_p99_ms: i64,
    pub lag_max_ms: i64,
    pub lag_samples: i64,
    pub disconnects_market: i64,
    pub disconnects_off_hours: i64,
    pub reconnects: i64,
    pub stalls: i64,
    pub blame_broker: i64,
    pub blame_ours: i64,
    pub blame_indeterminate: i64,
    pub restarts: i64,
    /// Per-instrument coverage columns (scoreboard PR-D) — REAL only when
    /// the in-memory presence registry supplied them on a same-day run;
    /// `-1` sentinels otherwise (a rerun that lost the registry folds the
    /// day's EXISTING measured columns forward — the coverage keep-better).
    pub mapped_instruments: i64,
    pub unmapped_instruments: i64,
    pub covered_instrument_minutes: i64,
    /// Where THIS run's coverage numbers came from (per feed).
    pub coverage_source: CoverageSource,
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
            lag_p50_ms: s,
            lag_p99_ms: s,
            lag_max_ms: s,
            lag_samples: s,
            disconnects_market: s,
            disconnects_off_hours: s,
            reconnects: s,
            stalls: s,
            blame_broker: s,
            blame_ours: s,
            blame_indeterminate: s,
            restarts: s,
            mapped_instruments: s,
            unmapped_instruments: s,
            covered_instrument_minutes: s,
            coverage_source: CoverageSource::SqlBackfill,
        }
    }

    /// Fold one feed's drained day-lag summary in (scoreboard PR-C). `None`
    /// (backfill day / thin histogram / fold disabled) keeps the `-1`
    /// sentinels — the card renders "not measured yet", never a fabricated
    /// zero (Rule 11). Pure — the process-global histogram read stays in
    /// the thin caller.
    fn apply_day_lag(
        &mut self,
        summary: Option<tickvault_core::pipeline::feed_lag_monitor::DayLagSummary>,
    ) {
        if let Some(s) = summary {
            self.lag_p50_ms = s.p50_ms;
            self.lag_p99_ms = s.p99_ms;
            self.lag_max_ms = s.max_ms;
            self.lag_samples = s.samples;
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
    /// Dhan was switched OFF for the day (round 4, 2026-07-10) — its row
    /// stamps the distinct 'feed_off' outcome and the card says "no
    /// contest" instead of declaring a one-horse winner.
    pub dhan_feed_off: bool,
    /// Groww was switched OFF for the day (round 4, 2026-07-10).
    pub groww_feed_off: bool,
    /// REST 1m pull digest aggregates per (feed, leg) — the Quote-2 daily
    /// answer (Groww REST plan PR-5). Empty on days with no measurable
    /// pull records (pre-deploy days / read failure — see the flag below).
    pub rest_legs: Vec<RestLegDaySummary>,
    /// `true` when the pull-record read/parse failed while building the
    /// digest — the card carries the honest cause footnote; the rest of
    /// the scorecard is UNAFFECTED (the digest is additive by design).
    pub rest_legs_read_failed: bool,
}

/// RunCatchUp already-ran probe (round 4, 2026-07-10 — LOW): reads the
/// day's existing daily-row outcomes; `true` when BOTH feeds already carry
/// outcome='complete' (see [`catchup_rerun_is_redundant`]) so a
/// post-trigger same-day boot (the post-close deploy restart) skips the
/// redundant re-run + duplicate Telegram card. Any read/parse failure
/// returns `false` — never skip on uncertainty. Forced runs and boots
/// with in-market synthesized deaths never consult this (caller-gated).
// TEST-EXEMPT: thin exec_query wrapper over the unit-tested pure parts (build_existing_daily_outcome_sql / parse_existing_daily_outcomes / catchup_rerun_is_redundant); a direct test needs live QuestDB.
pub async fn day_already_scored_complete(questdb: &QuestDbConfig, target_ist_day: u64) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(SCOREBOARD_HTTP_TIMEOUT_SECS))
        .build()
    else {
        return false;
    };
    let sql = build_existing_daily_outcome_sql(target_ist_day);
    match exec_query(&client, questdb, &sql).await {
        Some(body) => {
            parse_existing_daily_outcomes(&body).is_some_and(|m| catchup_rerun_is_redundant(&m))
        }
        None => false,
    }
}

/// Runs the daily scoreboard aggregation once. Cold path (once/day at the
/// configured trigger). Fail-soft: missing sources record sentinels + an
/// honest `partial`/`degraded` outcome — the summary is ALWAYS returned so
/// the Telegram can never be silently dropped by a data failure.
/// `runtime_enabled_now` = the CURRENT per-feed runtime flags
/// `(dhan, groww)` — `Some` on same-day runs only (the feed-off-day
/// disambiguator; a past-day backfill passes `None` and infers from data
/// alone).
///
/// # Errors
/// Returns `Err` only when NOTHING could be measured (both the episode and
/// tick sources unreachable) — the caller pages `DualFeedScorecardAborted`.
#[allow(clippy::too_many_arguments)] // APPROVED: once-per-day orchestration entry with one caller; a params struct would just relocate the arity.
// TEST-EXEMPT: orchestration over the unit-tested pure parts (SQL builders / parsers / overlap / tallies / classifier); a direct test needs live QuestDB — covered operationally by TICKVAULT_SCOREBOARD_NOW.
pub async fn run_feed_scoreboard(
    questdb: &QuestDbConfig,
    metrics_port: u16,
    target_ist_day: u64,
    trading_date_label: String,
    forced_early_run: bool,
    coverage_detail_rows: bool,
    is_same_day_run: bool,
    boot_synthesized_deaths: usize,
    boot_reconciled_rows: &[FeedEpisodeAuditRow],
    runtime_enabled_now: Option<(bool, bool)>,
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
    let mut corr = match tokio::task::spawn_blocking(move || {
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
    // Round-4 evidence-horizon downgrade (2026-07-10 — HIGH): I/O success
    // is NOT evidence coverage — errors.jsonl retention is ~48h, so a
    // backfill of an older day scans a READABLE dir whose files cannot
    // cover the target day. Leaving scan_complete=true there (a) skipped
    // the keep-better read entirely and (b) stamped every re-classified
    // row run_partial=false — the exact silent destructive overwrite of
    // evidence-backed blame the round-3 guard exists for. Downgrading
    // here feeds BOTH the run_partial stamp and the keep-better gate.
    let today_ist_day = u64::try_from(
        chrono::Utc::now()
            .timestamp()
            .saturating_add(i64::from(
                tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
            ))
            .div_euclid(86_400),
    )
    .unwrap_or(0);
    if corr.scan_complete && !target_within_evidence_retention(target_ist_day, today_ist_day) {
        info!(
            target_ist_day,
            today_ist_day,
            "feed_scoreboard: the target day is past the ~48h errors.jsonl \
             evidence horizon — evidence treated as PARTIAL (rows stamp \
             run_partial; the keep-better guard engages)"
        );
        corr.scan_complete = false;
    }
    let corr = corr;

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
    // The feed-off-day inference inputs (round 4, redesigned round 5,
    // hardened round 6, 2026-07-10): per feed — up-kind rows INSIDE the
    // session window (the ~08:33 boot connect must not defeat a
    // runtime-disable day; a parked WS-GAP-04 wake never counts), up-kind
    // rows anywhere in the day, and the LATEST pre-session
    // (source='feed_disabled' marker, up-kind row) timestamps for the
    // state-at-open comparison (a disable→re-enable flap must not soften
    // a broker-dead session). `None` = the ws read failed, so feed-off
    // can never be claimed (unmeasured is not off).
    struct FeedUpSignals {
        session_up: HashSet<String>,
        any_up: HashSet<String>,
        /// feed → (latest pre-session `feed_disabled` marker ts, latest
        /// pre-session up-kind row ts) — IST nanos; `None` = no such row.
        pre_session_toggles: BTreeMap<String, (Option<i64>, Option<i64>)>,
    }
    let mut up_signals: Option<FeedUpSignals> = None;
    match exec_query(&client, questdb, &ws_sql).await {
        Some(body) => match parse_ws_events(&body) {
            Some(rows) => {
                let mut recon: BTreeMap<String, i64> = BTreeMap::new();
                let mut mem: BTreeMap<String, EpisodeTally> = BTreeMap::new();
                let mut ups = FeedUpSignals {
                    session_up: HashSet::new(),
                    any_up: HashSet::new(),
                    pre_session_toggles: BTreeMap::new(),
                };
                // WS-GAP-04 wakes the dormant gate immediately re-parked
                // (round 6, 2026-07-10 — MEDIUM): their SleepResumed rows
                // are machinery artifacts, not up evidence.
                let parked = parked_wake_indices(&rows);
                let mut writer = FeedEpisodeAuditWriter::new(questdb);
                let mut appended = 0_u64;
                for (idx, ev) in rows.iter().enumerate() {
                    // Reconnects on the headline card compare the two
                    // BROKER FEEDS — the order-update trading channel is
                    // excluded (round-2 hostile review 2026-07-10).
                    if ev.event_kind == "reconnected" && is_market_data_ws_type(&ev.ws_type) {
                        *recon.entry(ev.feed.clone()).or_default() += 1;
                    }
                    // PR-C1 (2026-07-13): up-signals count MARKET-DATA rows
                    // ONLY (the Phase B "Honest obligation" acceptance
                    // criterion) — the Q4-i-rewired order-update WS keeps
                    // writing Dhan up-kind rows on a permanently-off Dhan
                    // feed and must never defeat feed_off. The predicates
                    // carry the same filter internally; this outer gate
                    // covers the `any_up` fold.
                    if is_market_data_ws_type(&ev.ws_type) && is_up_kind(&ev.event_kind) {
                        ups.any_up.insert(ev.feed.clone());
                        if !parked.contains(&idx) {
                            if is_session_up_row(ev) {
                                ups.session_up.insert(ev.feed.clone());
                            }
                            if is_pre_session_up_row(ev) {
                                let e = ups
                                    .pre_session_toggles
                                    .entry(ev.feed.clone())
                                    .or_insert((None, None));
                                e.1 = Some(e.1.map_or(ev.ts_ist_nanos, |t| t.max(ev.ts_ist_nanos)));
                            }
                        }
                    }
                    if is_pre_session_feed_disable_row(ev) {
                        let e = ups
                            .pre_session_toggles
                            .entry(ev.feed.clone())
                            .or_insert((None, None));
                        e.0 = Some(e.0.map_or(ev.ts_ist_nanos, |t| t.max(ev.ts_ist_nanos)));
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
                up_signals = Some(ups);
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
    //    ROUND-4 DAY FILTER (2026-07-10 — HIGH): the reconciler targets
    //    the BOOT day, so on a `TICKVAULT_SCOREBOARD_DATE` past-day
    //    backfill THIS boot's (today's) rows must fold into NOTHING — the
    //    unconditional fold incremented the past day's restarts/blame and
    //    flipped its completed row to Partial via the data-driven floor.
    let boot_rows_for_day = filter_boot_rows_to_day(boot_reconciled_rows, target_ist_day);
    let skip_keys = boot_reconciled_skip_keys(&boot_rows_for_day);
    let fold_boot_rows_in_memory = |mem: &mut BTreeMap<String, EpisodeTally>| {
        for row in &boot_rows_for_day {
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

    // 5a. The day's EXISTING daily-row outcomes + lag columns — read ONCE,
    //     shared by the feed_off keep-better (5b), the lag keep-better
    //     (6c), and the degraded keep-better (7b). A failed read turns ALL
    //     the guards off for this run, loudly.
    let existing_daily_body = {
        let daily_sql = build_existing_daily_outcome_sql(target_ist_day);
        exec_query(&client, questdb, &daily_sql).await
    };
    let existing_daily: Option<BTreeMap<String, String>> = {
        match existing_daily_body
            .as_deref()
            .map(parse_existing_daily_outcomes)
        {
            Some(Some(existing)) => Some(existing),
            Some(None) | None => {
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "outcome_keep_better_read",
                    "SCOREBOARD-01: existing daily-outcome read failed/unparsable \
                     — the daily keep-better guards are OFF this run; a rerun may \
                     erase a 'degraded'/'feed_off' verdict or a measured lag \
                     distribution"
                );
                None
            }
        }
    };
    // The lag columns of the SAME body (PR-C review round 1, 2026-07-11) —
    // the lag keep-better (6c) input; a failed read already errored above.
    let existing_lag: Option<BTreeMap<String, ExistingDailyLag>> = existing_daily_body
        .as_deref()
        .and_then(parse_existing_daily_lag);

    // 5b. Feed-off-day detection (round 4; redesigned round 5; hardened
    //     round 6, 2026-07-10): a feed with a measured zero tick count,
    //     zero up-kind rows INSIDE the session window (parked WS-GAP-04
    //     wakes excluded), and either no up rows at all (config-off) or a
    //     pre-session 'feed_disabled' toggle row that is the LAST
    //     pre-session toggle (state-at-open — a disable→re-enable flap
    //     does NOT qualify) was switched off — its row stamps the
    //     distinct 'feed_off' outcome (excluded from the month sums) and
    //     the card says "no contest". The enabled-but-dead-broker day can
    //     NEVER be softened into feed_off (no disable-at-open state + a
    //     boot up row keeps it loud on same-day runs AND backfills); a
    //     failed ws read claims nothing. The durable state-at-open marker
    //     outranks the run-instant enabled flag (the 15:30–15:45
    //     re-enable window). The keep-better arm preserves an existing
    //     'feed_off' row against a same-day evening rerun that
    //     re-measured zeros with the feed now re-enabled for tomorrow.
    let mut feed_off: BTreeMap<&'static str, bool> = BTreeMap::new();
    for feed in tickvault_common::feed::Feed::ALL {
        let label = feed.as_str();
        let ticks = feed_numbers
            .get(label)
            .map_or(SCOREBOARD_UNAVAILABLE_SENTINEL, |n| n.ticks);
        let enabled_now = runtime_enabled_now.map(|(dhan_on, groww_on)| {
            if matches!(feed, tickvault_common::feed::Feed::Dhan) {
                dhan_on
            } else {
                groww_on
            }
        });
        let inferred = up_signals.as_ref().is_some_and(|ups| {
            let (disable_ts, up_ts) = ups
                .pre_session_toggles
                .get(label)
                .copied()
                .unwrap_or((None, None));
            is_feed_off_day(
                ups.session_up.contains(label),
                ups.any_up.contains(label),
                pre_session_disable_is_state_at_open(disable_ts, up_ts),
                ticks,
                enabled_now,
            )
        });
        let kept = existing_daily.as_ref().is_some_and(|m| {
            should_keep_feed_off_outcome(m.get(label).map(String::as_str), inferred, ticks)
        });
        if kept {
            error!(
                code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                stage = "outcome_regression",
                feed = label,
                "SCOREBOARD-01: this rerun would ERASE the day's existing \
                 'feed_off' verdict with complete-with-zeros (the feed \
                 measured no ticks — a re-enable for tomorrow does not \
                 rewrite today's no-contest) — keeping outcome=feed_off"
            );
        }
        if inferred {
            info!(
                feed = label,
                "feed_scoreboard: {label} was switched off for the day — \
                 stamping its row outcome='feed_off' (excluded from the \
                 month verdict; the card says no contest)"
            );
        }
        feed_off.insert(label, inferred || kept);
    }

    // 5c. Feed-level unique-win / both minutes from the two minute sets —
    //     with the `-1` sentinel on the PARTNER's comparison columns on a
    //     feed-off day (round 5, 2026-07-10 — MEDIUM: exclusive-vs-nothing
    //     is not a measurement; the phantom ~375 unique minutes skewed the
    //     month verdict's headline sum).
    apply_minute_overlap_and_feed_off_sentinels(&mut feed_numbers, &minute_sets, &feed_off);

    // 5d. Per-instrument presence drain — RETIRED 2026-07-18 (stage-4
    //     dead-producer sweep): the in-memory presence registry was deleted
    //     (zero record/register producers on the REST-only runtime), so
    //     there is no drain. The coverage path degrades HONESTLY to the
    //     step-5c SQL minute-set approximation — `coverage_source` stays
    //     `sql_backfill` ("`None` ... keeps the SQL numbers + sentinels,
    //     honestly") and the 6d coverage keep-better still protects
    //     historical registry-derived rows from lower-ranked reruns.

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

    // 6b. Day lag distributions (scoreboard PR-C): drain the in-memory
    //     per-feed day histograms (feed_lag_monitor — fed by the same
    //     record_* calls that drive the live lag gauges; replay/re-tail
    //     excluded at record time, NEVER a SQL approximation over the
    //     replay-contaminated `received_at` column). SCOPED to same-day
    //     runs ONLY: the histograms are process-local + day-scoped (IST
    //     midnight reset), so a past-day backfill can never read them — its
    //     lag columns stay -1 sentinels, honestly. A mid-day restart leaves
    //     only the post-restart window in the histogram; the restart-day
    //     partial floor (step 7c) already stamps such a day partial and the
    //     card carries the restart footnote — measured-but-partial, never
    //     fabricated (Rule 11).
    if is_same_day_run {
        for feed in tickvault_common::feed::Feed::ALL {
            if let Some(n) = feed_numbers.get_mut(feed.as_str()) {
                n.apply_day_lag(tickvault_core::pipeline::feed_lag_monitor::day_lag_summary(
                    *feed,
                ));
            }
        }
    }

    // 6c. LAG keep-better (PR-C review round 1, 2026-07-11 — HIGH): a
    //     same-day evening RunCatchUp rerun fires precisely on
    //     partial/degraded days (the latch skips only complete/feed_off
    //     days), and a post-close deploy restart runs it in a FRESH
    //     process whose day histograms are EMPTY — day_lag_summary returns
    //     None, the sentinels stand, and step 8's deterministic-ts UPSERT
    //     would erase the 15:45 run's MEASURED lag distribution with −1 on
    //     exactly the incident days where it matters most (a past-day
    //     backfill over a measured row is the same erasure). Mirror of the
    //     outcome/episode keep-better guards: an unmeasured rerun folds the
    //     existing measured lag columns forward; a rerun that measured
    //     real samples still updates.
    for feed in tickvault_common::feed::Feed::ALL {
        let label = feed.as_str();
        if let Some(n) = feed_numbers.get_mut(label) {
            let existing = existing_lag.as_ref().and_then(|m| m.get(label));
            if fold_existing_lag_keep_better(n, existing) {
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "lag_regression",
                    feed = label,
                    kept_samples = n.lag_samples,
                    "SCOREBOARD-01: this rerun measured no day lag (fresh \
                     process / thin histogram / backfill) but the day's \
                     existing row carries a measured lag distribution — \
                     keeping the existing measured lag columns instead of \
                     overwriting them with -1"
                );
            }
        }
    }

    // 6d. COVERAGE keep-better (scoreboard PR-D — mirror of the 6c lag
    //     guard): a rerun whose coverage fell back to SQL (fresh post-close
    //     process with an empty registry, or a past-day backfill) must not
    //     erase the 15:45 run's registry-derived per-instrument columns at
    //     the deterministic-ts UPSERT. Uses the SHARED step-5a body; a
    //     failed read already errored there and the guard is OFF.
    let existing_coverage: Option<BTreeMap<String, ExistingDailyCoverage>> = existing_daily_body
        .as_deref()
        .and_then(parse_existing_daily_coverage);
    for feed in tickvault_common::feed::Feed::ALL {
        let label = feed.as_str();
        if let Some(n) = feed_numbers.get_mut(label) {
            let existing = existing_coverage.as_ref().and_then(|m| m.get(label));
            if fold_existing_coverage_keep_better(n, existing) {
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "coverage_regression",
                    feed = label,
                    "SCOREBOARD-01: this rerun's coverage came from a \
                     lower-ranked source than the day's existing row \
                     (registry unavailable) — keeping the existing \
                     registry-derived coverage columns instead of \
                     overwriting them"
                );
            }
        }
    }

    // 6e. REST 1m pull digest (Groww REST plan PR-5 — the operator's
    //     Quote-2 mandate: "within how many seconds precisely"). Reads the
    //     day's rest_fetch_audit forensics rows + the spot_1m_rest
    //     latency-fallback column and aggregates per (feed, leg). ADDITIVE
    //     + best-effort: a read/parse failure logs SCOREBOARD-01
    //     (stage=rest_leg_*), flags the card's honest footnote, and NEVER
    //     touches the episode/coverage/lag sections, the daily rows, or
    //     the run outcome. Nothing is persisted — the aggregates are
    //     recomputed from the durable audit table on every run, so
    //     backfills/reruns are idempotent by construction and no
    //     keep-better guard is needed (the PR-C conventions apply at
    //     RENDER time via the -1 sentinels).
    let mut rest_legs_read_failed = false;
    let audit_lite: Vec<RestFetchLiteRow> = {
        let sql = build_rest_fetch_audit_day_sql(target_ist_day);
        match exec_query(&client, questdb, &sql).await {
            Some(body) => parse_rest_fetch_audit_lite(&body).unwrap_or_else(|| {
                rest_legs_read_failed = true;
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "rest_leg_parse",
                    "SCOREBOARD-01: the day's rest_fetch_audit body was \
                     unparsable — the pull digest degrades to the honest \
                     footnote (the rest of the card is unaffected)"
                );
                Vec::new()
            }),
            None => {
                rest_legs_read_failed = true;
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "rest_leg_read",
                    "SCOREBOARD-01: the day's rest_fetch_audit rows could not \
                     be read — the pull digest degrades to the honest \
                     footnote (the rest of the card is unaffected)"
                );
                Vec::new()
            }
        }
    };
    let spot_latency_fallback: Vec<(String, i64)> = {
        let sql = build_spot1m_close_latency_day_sql(target_ist_day);
        match exec_query(&client, questdb, &sql).await {
            Some(body) => parse_feed_close_latency_rows(&body).unwrap_or_else(|| {
                rest_legs_read_failed = true;
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "rest_leg_fallback_parse",
                    "SCOREBOARD-01: the day's spot candle latency body was \
                     unparsable — the latency-only fallback lines degrade"
                );
                Vec::new()
            }),
            None => {
                rest_legs_read_failed = true;
                error!(
                    code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                    stage = "rest_leg_fallback_read",
                    "SCOREBOARD-01: the day's spot candle latency rows could \
                     not be read — the latency-only fallback lines degrade"
                );
                Vec::new()
            }
        }
    };
    let rest_legs = aggregate_rest_leg_day(&audit_lite, &spot_latency_fallback);
    // F3 log-side always-render contract (re-homed here by G6, fix
    // round 2): fires on EVERY aggregation run — including
    // telegram-disabled boots, where the card never renders at all.
    log_rest_leg_measurement_gaps(&rest_legs);
    // Dashboard gauge (plan PR-5 "+ dashboard gauge") — /metrics-local
    // (NOT CloudWatch-shipped; the tv_index_futures_selected precedent, so
    // zero alarm/cost impact), set on SAME-DAY runs only (a backfill's
    // gauge would misrepresent NOW), static label values only (the bounded
    // feed/leg allowlist — parsed strings never become label values).
    if is_same_day_run {
        for s in &rest_legs {
            if s.close_p99_ms >= 0
                && let Some((feed_label, leg_label)) = rest_leg_gauge_labels(&s.feed, &s.leg)
            {
                #[allow(clippy::cast_precision_loss)]
                // APPROVED: display-only gauge of a bounded daily latency.
                metrics::gauge!(
                    "tv_rest_leg_close_to_data_p99_ms",
                    "feed" => feed_label,
                    "leg" => leg_label
                )
                .set(s.close_p99_ms as f64);
            }
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
    let mut outcome = if degraded {
        ScoreboardOutcome::Degraded
    } else if row_partial {
        ScoreboardOutcome::Partial
    } else {
        ScoreboardOutcome::Complete
    };

    // 7b. DAILY-outcome keep-better (round 4, 2026-07-10 — MEDIUM): a
    //     same-day evening RunCatchUp rerun re-measures the FRESH
    //     process's audit-drop counter as 0 and would UPSERT
    //     outcome='complete' over the 15:45 run's 'degraded' verdict —
    //     the drop evidence is session-local and durable NOWHERE except
    //     the row being overwritten. A rerun must never downgrade
    //     degraded → partial/complete. Uses the SHARED step-5a read
    //     (round 5); a failed read already errored there and the guard is
    //     OFF (mirrors the episode keep-better).
    if !matches!(outcome, ScoreboardOutcome::Degraded)
        && let Some(ref existing) = existing_daily
        && should_keep_degraded_outcome(existing, outcome)
    {
        error!(
            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
            stage = "outcome_regression",
            new_outcome = outcome.as_str(),
            "SCOREBOARD-01: this rerun would ERASE the day's \
             existing 'degraded' verdict (the original session's \
             audit-drop evidence is unrecoverable) — keeping \
             outcome=degraded"
        );
        degraded = true;
        outcome = ScoreboardOutcome::Degraded;
    }

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
            // Scoreboard PR-D: registry-derived on same-day runs (step
            // 5d); an unmeasured rerun folds the existing row's
            // registry-derived values forward (step 6d coverage
            // keep-better); -1 sentinels only when NOTHING ever measured
            // the day.
            mapped_instruments: n.mapped_instruments,
            unmapped_instruments: n.unmapped_instruments,
            covered_instrument_minutes: n.covered_instrument_minutes,
            unique_win_minutes: n.unique_win_minutes,
            both_minutes: n.both_minutes,
            // Scoreboard PR-C: measured from the in-memory day histograms
            // on same-day runs (step 6b); an unmeasured rerun folds the
            // existing row's measured values forward (step 6c lag
            // keep-better — review round 1, 2026-07-11); -1 sentinels only
            // when NOTHING ever measured the day — never fabricated,
            // never erasing a measured distribution.
            lag_p50_ms: n.lag_p50_ms,
            lag_p99_ms: n.lag_p99_ms,
            lag_max_ms: n.lag_max_ms,
            lag_samples: n.lag_samples,
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
            // Scoreboard PR-D: per-feed source truth — 'in_memory' when
            // the registry covered the full session, 'mixed' on a mid-day
            // restart, 'sql_backfill' when the registry was unavailable
            // (or a higher-ranked existing row was folded forward, 6d).
            coverage_source: n.coverage_source,
            // A feed switched off for the day stamps the distinct
            // 'feed_off' outcome so the month sums can exclude the
            // one-horse days (round 4, 2026-07-10).
            outcome: if feed_off.get(label).copied().unwrap_or(false) {
                ScoreboardOutcome::FeedOff
            } else {
                outcome
            },
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

    // 8b. Per-instrument coverage detail rows (scoreboard PR-D) — config-
    //     gated ([scoreboard] coverage_detail_rows), same deterministic ts
    //     so re-runs UPSERT in place (DEDUP: ts, trading_date_ist,
    //     security_id, exchange_segment, feed — full I-P1-11 pair + feed).
    //     ~1.5K rows, cold, once per run; a failed append/flush degrades
    //     loudly and never blocks the daily rows (already flushed above).
    //     PR-D fix round 1 (review MEDIUM): 8b is additionally gated on
    //     the 6d verdict — when 6d folded the day's EXISTING coverage
    //     forward for any feed, this run's drain is provably
    //     lower-fidelity (an evening rerun's zero-bit registry / a
    //     registry-less fallback), so writing its detail rows would
    //     clobber the 15:45 run's real per-instrument rows at the same
    //     DEDUP key (and, on a one-lane re-registration, ADD phantom
    //     singleton rows under a flipped feed key).
    // 8b. Per-instrument `feed_coverage_daily` detail rows — RETIRED
    //     2026-07-18 (stage-4 dead-producer sweep): the presence registry
    //     (their ONLY source) is deleted, so no detail row can ever be
    //     built again. The `[scoreboard] coverage_detail_rows` config gate
    //     is inert (logged below); the table + storage writer stay for the
    //     historical rows (SEBI/forensic — trackA table policy).
    if coverage_detail_rows {
        info!(
            "feed_scoreboard: per-instrument coverage detail rows retired \
             2026-07-18 (presence registry deleted) — the config gate is \
             inert; coverage_source degrades to sql_backfill"
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
        dhan_feed_off: feed_off.get("dhan").copied().unwrap_or(false),
        groww_feed_off: feed_off.get("groww").copied().unwrap_or(false),
        rest_legs,
        rest_legs_read_failed,
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
    fn test_day_bounds_micros_window() {
        // The shared micros source (#1474 promoted it to pub(crate) so the
        // tick-conservation audit reuses it): start = day × 86_400 × 1e6,
        // end = start + one IST day in micros — NEVER nanos (the QuestDB
        // TIMESTAMP-comparison regression lock).
        let (start, end) = day_bounds_micros(DAY);
        assert_eq!(start, (DAY as i64) * 86_400 * 1_000_000);
        assert_eq!(end, start + 86_400 * 1_000_000);
        // Adversarial input stays bounded (saturating math, no panic).
        let (s, e) = day_bounds_micros(u64::MAX);
        assert!(s <= e);
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
    fn test_process_death_synthesizes_through_pre_boot_stall_restarted_row() {
        // PR-B fix round 1 (review HIGH): a `stall_restarted` row is
        // TRANSPARENT for pairing. Topology: morning connect 08:35 → stall
        // kill+relaunch 11:00 (writes a stall row; the relaunch restores
        // streaming with NO fresh up-kind row) → in-market process crash →
        // boot 14:05 with post-boot connect 14:06. Pre-fix the stall row
        // displaced the connect in the last-pre slot (`!is_up_kind` →
        // continue) and the boot-only death episode was permanently lost.
        let rows = vec![
            ev(
                day_ts(30_900), // 08:35 IST
                "groww",
                "groww_bridge",
                "connected",
                "groww_subscribed",
                -1,
                false,
            ),
            ev(
                day_ts(39_600), // 11:00 IST — stall kill+relaunch
                "groww",
                "groww_bridge",
                "stall_restarted",
                "stall_silent_socket",
                -1,
                true,
            ),
            ev(
                day_ts(50_760), // 14:06 IST — this boot's connect
                "groww",
                "groww_bridge",
                "connected",
                "groww_subscribed",
                -1,
                true,
            ),
        ];
        let boot = day_ts(50_700); // 14:05 IST
        let deaths = synthesize_process_death_episodes(&rows, boot);
        assert_eq!(
            deaths.len(),
            1,
            "a pre-boot stall row must not block process-death synthesis"
        );
        assert_eq!(deaths[0].ts_ist_nanos, day_ts(50_760));
        assert!(deaths[0].market_hours, "death window was in-session");
        // Lockstep: the polling gate sees the SAME candidate — complete
        // with the post-boot connect, pending without it.
        assert!(post_boot_pairing_complete(&rows, boot));
        assert!(
            !post_boot_pairing_complete(&rows[..2], boot),
            "the connected+stall pre-boot pair must keep the key a PENDING \
             pairing candidate (the stall row must not hide it from the gate)"
        );
    }

    #[test]
    fn test_stall_restarted_transparent_skip_does_not_resurrect_down_key() {
        // The transparent skip must NOT resurrect a genuinely-down key: a
        // `disconnected` row before the stall row still occupies the
        // last-pre slot, so no death synthesizes.
        let rows = vec![
            ev(
                day_ts(36_000), // 10:00 IST — genuine down marker
                "groww",
                "groww_bridge",
                "disconnected",
                "feed_disabled",
                -1,
                true,
            ),
            ev(
                day_ts(39_600), // 11:00 IST — stall row after the down row
                "groww",
                "groww_bridge",
                "stall_restarted",
                "stall_silent_socket",
                -1,
                true,
            ),
            ev(
                day_ts(50_760), // 14:06 IST — post-boot connect
                "groww",
                "groww_bridge",
                "connected",
                "groww_subscribed",
                -1,
                true,
            ),
        ];
        let boot = day_ts(50_700);
        assert!(
            synthesize_process_death_episodes(&rows, boot).is_empty(),
            "a down-prior key must stay down — the stall skip must not \
             synthesize a death for a connection that was disconnected"
        );
        // The gate agrees: last-pre = disconnected → not a pairing
        // candidate → vacuously complete even before any post-boot row.
        assert!(post_boot_pairing_complete(&rows[..2], boot));
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
        assert_eq!(
            episode_kind_for_event("disconnected", "n/a"),
            Some("disconnect")
        );
        assert_eq!(
            episode_kind_for_event("disconnected_off_hours", "n/a"),
            Some("off_hours_disconnect")
        );
        for lifecycle in [
            "connected",
            "reconnected",
            "sleep_entered",
            "sleep_resumed",
            "junk",
        ] {
            assert_eq!(
                episode_kind_for_event(lifecycle, "n/a"),
                None,
                "{lifecycle}"
            );
        }
    }

    #[test]
    fn test_episode_kind_for_stall_restarted_rows() {
        // PR-B: the stall-watchdog lifecycle row maps to the stall episode
        // kinds — the never-streamed slug splits the kind; every other cause
        // slug (incl. a future unknown one — fail-safe to the classic kind,
        // never dropped) is a stall_restart.
        use tickvault_common::feed_blame::{
            STALL_SOURCE_AUTH_STALE, STALL_SOURCE_ENTITLEMENT, STALL_SOURCE_NEVER_STREAMED,
            STALL_SOURCE_SILENT_SOCKET,
        };
        assert_eq!(
            episode_kind_for_event("stall_restarted", STALL_SOURCE_NEVER_STREAMED),
            Some(EPISODE_KIND_NEVER_STREAMED_RESTART)
        );
        for source in [
            STALL_SOURCE_SILENT_SOCKET,
            STALL_SOURCE_AUTH_STALE,
            STALL_SOURCE_ENTITLEMENT,
            "some_future_slug",
        ] {
            assert_eq!(
                episode_kind_for_event("stall_restarted", source),
                Some(EPISODE_KIND_STALL_RESTART),
                "{source}"
            );
        }
    }

    #[test]
    fn test_stall_restarted_literal_matches_event_kind() {
        // Lockstep pin: the SQL-read string literal above must equal the
        // emitter's wire label — a WsEventKind rename can never silently
        // detach the aggregation from the stall rows.
        assert_eq!(
            tickvault_common::ws_event_types::WsEventKind::StallRestarted.as_str(),
            "stall_restarted"
        );
        // And a stall row must never be an up-kind (pairing/feed-off logic).
        assert!(!is_up_kind("stall_restarted"));
    }

    #[test]
    fn test_classify_stall_row_to_episode_blame_and_detector() {
        use tickvault_common::feed_blame::{
            STALL_SOURCE_AUTH_STALE, STALL_SOURCE_NEVER_STREAMED, STALL_SOURCE_SILENT_SOCKET,
        };
        let corr = CorrelationEvidence {
            scan_complete: true,
            ..CorrelationEvidence::default()
        };
        let mk = |source: &str| WsAuditEventLite {
            ts_ist_nanos: day_ts(11 * 3600),
            feed: "groww".to_string(),
            ws_type: "groww_bridge".to_string(),
            connection_index: 0,
            event_kind: "stall_restarted".to_string(),
            source: source.to_string(),
            dhan_code: -1,
            down_secs: 42,
            market_hours: true,
        };
        // Silent socket → broker, detector stall_row, stall_reason threaded.
        let row = classify_ws_event_to_episode(&mk(STALL_SOURCE_SILENT_SOCKET), &corr, 0)
            .expect("stall rows classify to episodes");
        assert_eq!(row.episode_kind, EPISODE_KIND_STALL_RESTART);
        assert_eq!(row.blame, BlameClass::Broker);
        assert_eq!(row.blame_reason, "silent_socket");
        assert_eq!(row.detector, "stall_row");
        assert_eq!(row.down_secs, 42);
        // Auth-stale → OURS (the shared token minter is our duty).
        let row = classify_ws_event_to_episode(&mk(STALL_SOURCE_AUTH_STALE), &corr, 0)
            .expect("stall rows classify to episodes");
        assert_eq!(row.blame, BlameClass::Ours);
        assert_eq!(row.blame_reason, "token_minter_stale");
        // Never-streamed → the distinct kind, broker.
        let row = classify_ws_event_to_episode(&mk(STALL_SOURCE_NEVER_STREAMED), &corr, 0)
            .expect("stall rows classify to episodes");
        assert_eq!(row.episode_kind, EPISODE_KIND_NEVER_STREAMED_RESTART);
        assert_eq!(row.blame_reason, "never_streamed");
        // The tally fold counts BOTH stall kinds in the stalls column, never
        // in the disconnect columns (scope item 3 verification).
        let mut mem: BTreeMap<String, EpisodeTally> = BTreeMap::new();
        for source in [STALL_SOURCE_SILENT_SOCKET, STALL_SOURCE_NEVER_STREAMED] {
            let row = classify_ws_event_to_episode(&mk(source), &corr, 0).expect("classifies");
            fold_market_data_episode(&mut mem, &row);
        }
        let t = mem.get("groww").copied().unwrap_or_default();
        assert_eq!(t.stalls, 2);
        assert_eq!(t.disconnects_market, 0);
        assert_eq!(t.disconnects_off_hours, 0);
        assert_eq!(t.blame_broker, 2, "stall blame feeds the headline split");
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

    // -----------------------------------------------------------------------
    // Round-4 hostile-review fixes (2026-07-10)
    // -----------------------------------------------------------------------

    fn boot_row(ts: i64, market_hours: bool) -> FeedEpisodeAuditRow {
        FeedEpisodeAuditRow {
            ts_ist_nanos: ts,
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
            market_hours,
            evidence: String::new(),
            run_partial: false,
        }
    }

    #[test]
    fn test_filter_boot_rows_to_day_blocks_cross_day_backfill_contamination() {
        // Round-4 HIGH: the reconciler targets the BOOT day, so on a
        // TICKVAULT_SCOREBOARD_DATE past-day backfill THIS boot's (today's)
        // in-market rows must fold into NOTHING for the past target day —
        // previously they incremented the past day's restarts/blame and
        // engaged the data-driven Partial floor over its completed row.
        let boot_day_row = boot_row(day_ts(11 * 3600), true); // today 11:00 IST
        let past_day = DAY - 3; // backfill target: 3 days ago
        let filtered = filter_boot_rows_to_day(std::slice::from_ref(&boot_day_row), past_day);
        assert!(
            filtered.is_empty(),
            "a boot-day row must never fold into a past-day backfill"
        );
        // The fold over the filtered set tallies ZERO restarts → the
        // data-driven floor (tallied_restarts > 0) stays OFF.
        let mut mem: BTreeMap<String, EpisodeTally> = BTreeMap::new();
        for row in &filtered {
            fold_market_data_episode(&mut mem, row);
        }
        let tallied_restarts: i64 = mem.values().map(|t| t.restarts).sum();
        assert_eq!(tallied_restarts, 0, "no floor on the past day");
        // And the dedupe keys must come from the filtered set too — a
        // cross-day key would never match the past day's read-back anyway,
        // but the contract is day-scoped keys.
        assert!(boot_reconciled_skip_keys(&filtered).is_empty());
        // Same-day run (target == boot day): the row folds normally.
        let same_day = filter_boot_rows_to_day(std::slice::from_ref(&boot_day_row), DAY);
        assert_eq!(same_day.len(), 1);
        let mut mem: BTreeMap<String, EpisodeTally> = BTreeMap::new();
        for row in &same_day {
            fold_market_data_episode(&mut mem, row);
        }
        assert_eq!(mem.get("dhan").map_or(0, |t| t.restarts), 1);
    }

    #[test]
    fn test_target_within_evidence_retention_gates_run_partial_and_keep_better() {
        // Round-4 HIGH: I/O success is NOT evidence coverage. A 5-day-old
        // backfill target must classify as PARTIAL evidence even when every
        // retained errors.jsonl file opened cleanly — feeding BOTH the
        // run_partial stamp and the keep-better read gate.
        assert!(target_within_evidence_retention(DAY, DAY), "same day");
        assert!(
            target_within_evidence_retention(DAY, DAY + 1),
            "yesterday's 24 hourly files are always < 48h old at any \
             instant of today (mtime sweep: cutoff < yesterday's day start)"
        );
        // Round-5 boundary flip (2026-07-10 — MEDIUM off-by-one): the 48h
        // MTIME sweep covers a target day only while
        // now ≤ target_day_start + 48h — for target = today−2 that holds
        // only at exactly midnight, and the D−2 SESSION-hour files are
        // swept for any run after ~10:00 today. A 2-day-old backfill is
        // therefore PAST the horizon: run_partial=true + the keep-better
        // read ENGAGED (the destructive blame-overwrite bypass at exactly
        // the boundary day).
        assert!(
            !target_within_evidence_retention(DAY, DAY + 2),
            "2-day-old target is PAST the mtime-sweep horizon — a D−2 \
             backfill can never hold the day's full session evidence"
        );
        assert!(
            !target_within_evidence_retention(DAY, DAY + 3),
            "3-day-old target is past the retention horizon"
        );
        assert!(
            !target_within_evidence_retention(DAY, DAY + 5),
            "the flagship 5-day-old month-end backfill is partial evidence"
        );
        // The downgraded evidence stamps run_partial=true on every
        // re-classified row…
        let corr = CorrelationEvidence {
            scan_complete: false, // the horizon downgrade applied
            ..CorrelationEvidence::default()
        };
        let event = ev(
            day_ts(10 * 3600),
            "dhan",
            "main_feed",
            "disconnected",
            "dhan_code",
            805,
            true,
        );
        let episode = classify_ws_event_to_episode(&event, &corr, day_ts(0)).expect("episode");
        assert!(
            episode.run_partial,
            "aged-out evidence must stamp run_partial=true"
        );
        // …and the keep-better rule then SUPPRESSES the evidence-less
        // overwrite of the existing evidence-backed (run_partial=false) row.
        assert!(
            should_suppress_episode_overwrite(Some(false), episode.run_partial),
            "the >48h backfill must be suppressed against an evidence-backed row"
        );
    }

    #[test]
    fn test_build_existing_daily_outcome_sql_micros_window() {
        // Round-4 MEDIUM: the daily keep-better guard input — micros
        // literals per the module's regression lock (a nanos literal
        // silently matches ZERO rows on QuestDB 9.3.5).
        let sql = build_existing_daily_outcome_sql(DAY);
        let (start, end) = day_micros();
        assert!(sql.contains("from feed_scoreboard_daily"), "{sql}");
        assert!(sql.contains(&format!("ts >= {start}")), "{sql}");
        assert!(sql.contains(&format!("ts < {end}")), "{sql}");
        // PR-C review round 1 (2026-07-11): the lag keep-better reads the
        // SAME body — the lag columns must ride the outcome query.
        for col in ["lag_p50_ms", "lag_p99_ms", "lag_max_ms", "lag_samples"] {
            assert!(
                sql.contains(col),
                "lag keep-better column {col} missing: {sql}"
            );
        }
        // Scoreboard PR-D: the coverage keep-better reads the SAME body —
        // the coverage columns must ride the outcome query too.
        for col in [
            "unique_win_minutes",
            "both_minutes",
            "mapped_instruments",
            "unmapped_instruments",
            "covered_instrument_minutes",
            "coverage_source",
        ] {
            assert!(
                sql.contains(col),
                "coverage keep-better column {col} missing: {sql}"
            );
        }
        assert!(
            !sql.contains(&((DAY as i64) * 86_400 * NANOS_PER_SEC).to_string()),
            "nanos literal banned: {sql}"
        );
    }

    #[test]
    fn test_should_keep_degraded_outcome_and_parse_existing_daily_outcomes() {
        // Round-4 MEDIUM: an evening RunCatchUp rerun (fresh process, drop
        // counter 0) must never erase the 15:45 run's 'degraded' verdict.
        let body = r#"{"dataset":[["dhan","degraded"],["groww","degraded"]]}"#;
        let existing = parse_existing_daily_outcomes(body).expect("parse");
        assert_eq!(existing.get("dhan").map(String::as_str), Some("degraded"));
        assert!(
            should_keep_degraded_outcome(&existing, ScoreboardOutcome::Complete),
            "degraded → complete is the erased-verdict regression"
        );
        assert!(
            should_keep_degraded_outcome(&existing, ScoreboardOutcome::Partial),
            "degraded → partial is a downgrade too"
        );
        assert!(
            !should_keep_degraded_outcome(&existing, ScoreboardOutcome::Degraded),
            "a degraded rerun writes degraded — nothing to keep"
        );
        // A partial/complete prior day never blocks (reruns may upgrade).
        let body = r#"{"dataset":[["dhan","partial"],["groww","complete"]]}"#;
        let existing = parse_existing_daily_outcomes(body).expect("parse");
        assert!(!should_keep_degraded_outcome(
            &existing,
            ScoreboardOutcome::Complete
        ));
        // First run of the day: no rows, no keep.
        let existing = parse_existing_daily_outcomes(r#"{"dataset":[]}"#).expect("parse");
        assert!(!should_keep_degraded_outcome(
            &existing,
            ScoreboardOutcome::Complete
        ));
        assert_eq!(parse_existing_daily_outcomes("not json"), None);
    }

    #[test]
    fn test_parse_existing_daily_lag_reads_columns_2_through_5() {
        // PR-C review round 1 (2026-07-11): the lag keep-better parses the
        // SAME extended body the outcome parse reads (cols 0..=1 outcome,
        // 2..=5 lag) — a 6-column row yields both maps; a defensive short
        // row records sentinels, never fabricated values.
        let body = r#"{"dataset":[["dhan","partial",1200,2900,5400,48213],["groww","partial",-1,-1,-1,-1]]}"#;
        let lag = parse_existing_daily_lag(body).expect("parse");
        assert_eq!(
            lag.get("dhan"),
            Some(&ExistingDailyLag {
                p50_ms: 1200,
                p99_ms: 2900,
                max_ms: 5400,
                samples: 48213
            })
        );
        assert_eq!(
            lag.get("groww"),
            Some(&ExistingDailyLag {
                p50_ms: -1,
                p99_ms: -1,
                max_ms: -1,
                samples: -1
            })
        );
        // The outcome parse still reads the same body (shared read).
        let outcomes = parse_existing_daily_outcomes(body).expect("parse");
        assert_eq!(outcomes.get("dhan").map(String::as_str), Some("partial"));
        // Defensive: a 2-column legacy row records lag sentinels.
        let short = r#"{"dataset":[["dhan","complete"]]}"#;
        let lag = parse_existing_daily_lag(short).expect("parse");
        assert_eq!(lag.get("dhan").map(|l| l.samples), Some(-1));
        assert_eq!(parse_existing_daily_lag("not json"), None);
    }

    #[test]
    fn test_fold_existing_lag_keep_better_preserves_measured_on_unmeasured_rerun() {
        // PR-C review round 1 (2026-07-11 — HIGH): a post-close same-day
        // RunCatchUp rerun in a FRESH process (empty day histograms →
        // day_lag_summary None → sentinels) must fold the 15:45 run's
        // MEASURED lag columns forward instead of erasing them with −1.
        let existing = ExistingDailyLag {
            p50_ms: 1200,
            p99_ms: 2900,
            max_ms: 5400,
            samples: 48213,
        };
        let mut n = FeedDayNumbers::unavailable();
        assert!(
            fold_existing_lag_keep_better(&mut n, Some(&existing)),
            "an unmeasured rerun over a measured row must fire the keep-better"
        );
        assert_eq!(
            (n.lag_p50_ms, n.lag_p99_ms, n.lag_max_ms, n.lag_samples),
            (1200, 2900, 5400, 48213),
            "the measured distribution must be folded forward verbatim"
        );
        // A rerun that measured REAL samples still updates (its own
        // distribution wins — a rerun may improve a partial day).
        let mut n = FeedDayNumbers::unavailable();
        n.lag_p50_ms = 900;
        n.lag_p99_ms = 2100;
        n.lag_max_ms = 4000;
        n.lag_samples = 61_000;
        assert!(
            !fold_existing_lag_keep_better(&mut n, Some(&existing)),
            "a measured rerun must keep its own distribution"
        );
        assert_eq!(
            (n.lag_p50_ms, n.lag_p99_ms, n.lag_max_ms, n.lag_samples),
            (900, 2100, 4000, 61_000)
        );
        // No existing row / an existing row that never measured: the −1
        // sentinels stand honestly and nothing logs.
        let mut n = FeedDayNumbers::unavailable();
        assert!(!fold_existing_lag_keep_better(&mut n, None));
        assert_eq!(n.lag_samples, SCOREBOARD_UNAVAILABLE_SENTINEL);
        let never_measured = ExistingDailyLag {
            p50_ms: -1,
            p99_ms: -1,
            max_ms: -1,
            samples: -1,
        };
        assert!(!fold_existing_lag_keep_better(
            &mut n,
            Some(&never_measured)
        ));
        assert_eq!(n.lag_samples, SCOREBOARD_UNAVAILABLE_SENTINEL);
    }

    #[test]
    fn test_is_feed_off_day_inference() {
        // Round-4 MEDIUM (the round-2 finding), REDESIGNED round 5,
        // hardened round 6: a feed with measured-zero ticks, zero SESSION
        // up rows, and either no up rows at all (config-off) or a
        // pre-session disable marker that is the feed's STATE AT OPEN
        // (runtime-disabled) was switched off — never a one-horse win.
        // Args: (session_up, any_up, disable_is_state_at_open, ticks,
        //        enabled_now)
        assert!(
            is_feed_off_day(false, false, false, 0, None),
            "backfill, config-off all day (zero rows): data-only inference"
        );
        assert!(
            is_feed_off_day(false, false, false, 0, Some(false)),
            "same-day config-off + runtime-disabled"
        );
        assert!(
            !is_feed_off_day(false, false, false, 0, Some(true)),
            "same-day ENABLED but zero everything = a catastrophic day, \
             never softened into feed_off"
        );
        assert!(
            !is_feed_off_day(true, true, false, 0, None),
            "an in-session up row means the feed WAS on during trading"
        );
        assert!(
            !is_feed_off_day(false, false, false, 42, None),
            "ticks flowed = not off"
        );
        assert!(
            !is_feed_off_day(false, false, false, SCOREBOARD_UNAVAILABLE_SENTINEL, None),
            "a -1 sentinel is UNMEASURED, not zero — never claims feed_off"
        );
    }

    #[test]
    fn test_is_session_up_row_scopes_up_kinds_to_market_hours_window() {
        // Round-5 HIGH: the feed-off inference counts up rows ONLY inside
        // [09:00, 15:30) IST — boundary sweep.
        let up_at = |secs: i64| {
            ev(
                day_ts(secs),
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                true,
            )
        };
        assert!(
            !is_session_up_row(&up_at(8 * 3600 + 59 * 60 + 59)),
            "08:59:59"
        );
        assert!(is_session_up_row(&up_at(9 * 3600)), "09:00:00 inclusive");
        assert!(
            is_session_up_row(&up_at(15 * 3600 + 29 * 60 + 59)),
            "15:29:59"
        );
        assert!(
            !is_session_up_row(&up_at(15 * 3600 + 30 * 60)),
            "15:30 exclusive"
        );
        // Down kinds never count regardless of window.
        let down = ev(
            day_ts(10 * 3600),
            "dhan",
            "main_feed",
            "disconnected",
            "n/a",
            -1,
            true,
        );
        assert!(!is_session_up_row(&down));
    }

    /// PR-C1 (2026-07-13) — the Phase B "Honest obligation" acceptance
    /// criterion, CLOSED: the Q4-i-rewired order-update WS writes Dhan-feed
    /// up-kind rows (Connected/Reconnected) INSIDE the session on a
    /// permanently-off Dhan feed; those trading-channel rows must NEVER
    /// defeat the round-6 `feed_off` classification. Genuine market-data
    /// up rows (main_feed / groww_bridge) still count.
    #[test]
    fn test_is_session_up_row_ignores_order_update_ws() {
        let in_session = 10 * 3600; // 10:00 IST — inside [09:00, 15:30)
        for kind in ["connected", "reconnected", "sleep_resumed"] {
            let ou = ev(
                day_ts(in_session),
                "dhan",
                "order_update",
                kind,
                "n/a",
                -1,
                true,
            );
            assert!(
                !is_session_up_row(&ou),
                "an in-session order-update `{kind}` row must not count as a \
                 session up signal (it would block feed_off forever)"
            );
        }
        // Market-data rows on BOTH feeds still count unchanged.
        let dhan_up = ev(
            day_ts(in_session),
            "dhan",
            "main_feed",
            "reconnected",
            "n/a",
            -1,
            true,
        );
        assert!(is_session_up_row(&dhan_up), "main_feed up still counts");
        let groww_up = ev(
            day_ts(in_session),
            "groww",
            "groww_bridge",
            "connected",
            "n/a",
            -1,
            true,
        );
        assert!(is_session_up_row(&groww_up), "groww_bridge up still counts");
    }

    /// PR-C1 (2026-07-13): the pre-session arm mirrors the session arm — a
    /// pre-session order-update Connected row is trading-channel machinery,
    /// never a market-data re-enable signal for the state-at-open compare.
    #[test]
    fn test_is_pre_session_up_row_ignores_order_update_ws() {
        let pre_session = 8 * 3600 + 50 * 60; // 08:50 IST — before 09:15 open
        let ou = ev(
            day_ts(pre_session),
            "dhan",
            "order_update",
            "connected",
            "n/a",
            -1,
            false,
        );
        assert!(
            !is_pre_session_up_row(&ou),
            "a pre-session order-update Connected row must not read as a re-enable"
        );
        let dhan_up = ev(
            day_ts(pre_session),
            "dhan",
            "main_feed",
            "sleep_resumed",
            "n/a",
            -1,
            false,
        );
        assert!(
            is_pre_session_up_row(&dhan_up),
            "a pre-session main_feed up row still counts (the flap re-enable signal)"
        );
    }

    #[test]
    fn test_is_pre_session_feed_disable_row_source_and_bound() {
        // Round-5 HIGH: the disable marker must carry the shared
        // 'feed_disabled' slug AND land before the 09:15 session open.
        let marker = |secs: i64, source: &str| {
            ev(
                day_ts(secs),
                "groww",
                "groww_bridge",
                "disconnected",
                source,
                -1,
                false,
            )
        };
        assert!(is_pre_session_feed_disable_row(&marker(
            8 * 3600 + 40 * 60,
            FEED_DISABLE_SOURCE
        )));
        assert!(
            !is_pre_session_feed_disable_row(&marker(
                SESSION_START_SECS_OF_DAY_IST,
                FEED_DISABLE_SOURCE
            )),
            "09:15:00 is session — an in-session disable never qualifies"
        );
        assert!(
            !is_pre_session_feed_disable_row(&marker(8 * 3600 + 40 * 60, "n/a")),
            "only the operator-toggle slug is a marker"
        );
        assert_eq!(FEED_DISABLE_SOURCE, "feed_disabled");
    }

    #[test]
    fn test_feed_off_topology_runtime_disable_at_0840_vs_broker_dead_day() {
        // Round-5 HIGH topology 1 — the runtime /api/feeds disable at
        // 08:40 on a dhan-only day: the groww boot Connected row at 08:33
        // (PRE-session) plus the pre-session 'feed_disabled' marker must
        // classify the day feed_off; the round-4 whole-day up-row rule
        // made this unreachable (the 08:33 row always defeated it).
        let boot_connect = ev(
            day_ts(8 * 3600 + 33 * 60),
            "groww",
            "groww_bridge",
            "connected",
            "groww_subscribed",
            -1,
            false,
        );
        let disable = ev(
            day_ts(8 * 3600 + 40 * 60),
            "groww",
            "groww_bridge",
            "disconnected_off_hours",
            "feed_disabled",
            -1,
            false,
        );
        assert!(is_up_kind(&boot_connect.event_kind));
        assert!(
            !is_session_up_row(&boot_connect),
            "the 08:33 boot connect is PRE-session — it must not defeat \
             the feed-off inference"
        );
        assert!(
            is_pre_session_feed_disable_row(&disable),
            "the 08:40 toggle row is the pre-session disable marker"
        );
        assert!(
            is_feed_off_day(false, true, true, 0, Some(false)),
            "boot-connect-then-disable day: feed_off (no false winner)"
        );
        // The Dhan dormant-entry marker (SleepEntered, source=feed_disabled
        // since round 5) qualifies identically.
        let dhan_dormant = ev(
            day_ts(8 * 3600 + 41 * 60),
            "dhan",
            "main_feed",
            "sleep_entered",
            "feed_disabled",
            -1,
            false,
        );
        assert!(is_pre_session_feed_disable_row(&dhan_dormant));
        // Topology 2 — ENABLED-but-broker-dead groww day: the same 08:33
        // boot connect, NO disable marker, zero ticks. NOT feed_off — a
        // real catastrophic day stays a measured zero (a real winner
        // verdict is allowed). Honesty-critical on the same-day run AND
        // the backfill (enabled flag unavailable).
        assert!(
            !is_feed_off_day(false, true, false, 0, Some(true)),
            "same-day enabled-but-broker-dead: measured zero, never feed_off"
        );
        assert!(
            !is_feed_off_day(false, true, false, 0, None),
            "backfill of the broker-dead day: the boot up row without a \
             disable marker keeps it loud"
        );
        // An IN-session disable of a zero-tick feed is NOT pre-session —
        // the 09:15–disable prefix was a real measured-zero window.
        let midsession_disable = ev(
            day_ts(9 * 3600 + 30 * 60),
            "groww",
            "groww_bridge",
            "disconnected",
            "feed_disabled",
            -1,
            true,
        );
        assert!(!is_pre_session_feed_disable_row(&midsession_disable));
        // An in-session reconnect IS a session up row (the feed was on).
        let session_up = ev(
            day_ts(10 * 3600),
            "groww",
            "groww_bridge",
            "reconnected",
            "n/a",
            -1,
            true,
        );
        assert!(is_session_up_row(&session_up));
        assert!(
            !is_feed_off_day(true, true, true, 0, Some(false)),
            "a session up row always defeats feed_off"
        );
    }

    #[test]
    fn test_pre_session_disable_is_state_at_open_flap_topology() {
        // Round-6 MEDIUM: the disable marker qualifies feed_off only as
        // the feed's STATE AT SESSION OPEN — a pre-session
        // disable→re-enable flap (08:40 off, 08:50 back on) followed by a
        // broker-dead session must stay the loud catastrophic
        // measured-zero day on BOTH the same-day run and the backfill.
        let disable_0840 = day_ts(8 * 3600 + 40 * 60);
        let resume_0850 = day_ts(8 * 3600 + 50 * 60);
        let resume = ev(
            resume_0850,
            "dhan",
            "main_feed",
            "sleep_resumed",
            "n/a",
            -1,
            false,
        );
        assert!(
            is_pre_session_up_row(&resume),
            "08:50 resume is pre-session"
        );
        let state_at_open =
            pre_session_disable_is_state_at_open(Some(disable_0840), Some(resume_0850));
        assert!(
            !state_at_open,
            "the 08:50 re-enable is the LAST pre-session toggle — the \
             08:40 marker is not the state at open"
        );
        assert!(
            !is_feed_off_day(false, true, state_at_open, 0, Some(false)),
            "flap + broker-dead session, same-day run: NOT feed_off"
        );
        assert!(
            !is_feed_off_day(false, true, state_at_open, 0, None),
            "flap + broker-dead session, backfill: NOT feed_off"
        );
        // Unit sweep of the state-at-open comparison.
        assert!(pre_session_disable_is_state_at_open(Some(10), None));
        assert!(pre_session_disable_is_state_at_open(Some(20), Some(10)));
        assert!(!pre_session_disable_is_state_at_open(Some(10), Some(20)));
        assert!(
            !pre_session_disable_is_state_at_open(Some(10), Some(10)),
            "equal-ts tie resolves NOT-off (conservative: stays loud)"
        );
        assert!(!pre_session_disable_is_state_at_open(None, Some(10)));
        assert!(!pre_session_disable_is_state_at_open(None, None));
    }

    #[test]
    fn test_is_pre_session_up_row_bounds() {
        // Pre-session up-row bound sweep: [09:00, 09:15) rows are BOTH
        // session-window and pre-session; 09:15:00 is neither pre-session
        // nor (for a 15:40 resume) session-scoped.
        let up_at = |secs: i64| {
            ev(
                day_ts(secs),
                "dhan",
                "main_feed",
                "connected",
                "n/a",
                -1,
                false,
            )
        };
        assert!(is_pre_session_up_row(&up_at(9 * 3600 + 14 * 60 + 59)));
        assert!(!is_pre_session_up_row(&up_at(
            SESSION_START_SECS_OF_DAY_IST
        )));
        let down = ev(
            day_ts(8 * 3600),
            "dhan",
            "main_feed",
            "disconnected",
            "n/a",
            -1,
            false,
        );
        assert!(!is_pre_session_up_row(&down), "down kinds never count");
    }

    #[test]
    fn test_parked_wake_indices_disable_while_sleeping() {
        // Round-6 MEDIUM: the WS-GAP-04 wake emits SleepResumed at
        // ~09:00:00 BEFORE the dormant gate parks the connection and
        // stamps the feed_disabled marker (~09:00:01) — that wake row is
        // machinery, not up evidence, and must not defeat feed_off on a
        // disabled-while-sleeping day.
        let wake = ev(
            day_ts(9 * 3600),
            "dhan",
            "main_feed",
            "sleep_resumed",
            "n/a",
            -1,
            true,
        );
        let park = ev(
            day_ts(9 * 3600 + 1),
            "dhan",
            "main_feed",
            "sleep_entered",
            FEED_DISABLE_SOURCE,
            -1,
            true,
        );
        let rows = vec![wake.clone(), park.clone()];
        let parked = parked_wake_indices(&rows);
        assert!(parked.contains(&0), "wake immediately re-parked is PARKED");
        assert!(!parked.contains(&1));
        // The paired day classifies feed_off: no session up (wake
        // excluded), marker is the state at open, zero ticks.
        assert!(is_session_up_row(&wake), "raw row IS session-window up");
        let state_at_open = pre_session_disable_is_state_at_open(Some(park.ts_ist_nanos), None);
        assert!(is_feed_off_day(false, true, state_at_open, 0, Some(false)));
        // Control: an unpaired wake (feed re-enabled / genuinely resumed)
        // stays a session up row.
        assert!(
            parked_wake_indices(&rows[..1]).is_empty(),
            "no marker → not parked"
        );
        // Cross-key markers never pair.
        let other_conn = WsAuditEventLite {
            connection_index: 1,
            ..park.clone()
        };
        assert!(
            parked_wake_indices(&[wake.clone(), other_conn]).is_empty(),
            "different connection_index → not parked"
        );
        let other_feed = WsAuditEventLite {
            feed: "groww".to_string(),
            ws_type: "groww_bridge".to_string(),
            ..park.clone()
        };
        assert!(
            parked_wake_indices(&[wake.clone(), other_feed]).is_empty(),
            "different feed → not parked"
        );
        // A marker outside the pairing window (a later, unrelated
        // disable) never retro-parks a genuine wake.
        let late_marker = WsAuditEventLite {
            ts_ist_nanos: day_ts(9 * 3600) + PARKED_WAKE_PAIR_WINDOW_NANOS + NANOS_PER_SEC,
            ..park.clone()
        };
        assert!(
            parked_wake_indices(&[wake.clone(), late_marker]).is_empty(),
            "marker beyond the pairing window → not parked"
        );
        // A marker BEFORE the wake never pairs (ordering matters).
        let earlier_marker = WsAuditEventLite {
            ts_ist_nanos: day_ts(9 * 3600) - NANOS_PER_SEC,
            ..park
        };
        assert!(
            parked_wake_indices(&[wake, earlier_marker]).is_empty(),
            "marker before the wake → not parked"
        );
    }

    #[test]
    fn test_feed_off_marker_outranks_live_flag_reenable_window() {
        // Round-6 LOW: the durable state-at-open marker outranks the
        // run-instant enabled flag — disable 08:40 (no later pre-session
        // up), re-enable 15:40 for tomorrow, first run 15:45 with
        // runtime_enabled_now = Some(true): still feed_off (the 15:40
        // resume row is neither a session up row nor pre-session).
        let evening_resume = ev(
            day_ts(15 * 3600 + 40 * 60),
            "groww",
            "groww_bridge",
            "connected",
            "groww_subscribed",
            -1,
            false,
        );
        assert!(!is_session_up_row(&evening_resume), "15:40 is post-session");
        assert!(!is_pre_session_up_row(&evening_resume));
        let state_at_open =
            pre_session_disable_is_state_at_open(Some(day_ts(8 * 3600 + 40 * 60)), None);
        assert!(state_at_open);
        assert!(
            is_feed_off_day(false, true, state_at_open, 0, Some(true)),
            "disable 08:40 + re-enable 15:40 + first run 15:45 => feed_off"
        );
        // The Some(true) veto stays load-bearing for the NO-marker
        // enabled-but-broker-dead arm.
        assert!(!is_feed_off_day(false, true, false, 0, Some(true)));
    }

    #[test]
    fn test_apply_minute_overlap_and_feed_off_sentinels() {
        // Round-5 MEDIUM: on a feed-off day the PARTNER's comparison
        // columns take the -1 sentinel — exclusive-vs-nothing is not a
        // measurement, and the phantom ~375 unique minutes skewed the
        // month verdict's headline sum.
        let minute = |h: i64, m: i64| format!("2026-07-10T{h:02}:{m:02}:00.000000Z");
        let dhan_set: HashSet<String> = [minute(9, 15), minute(9, 16), minute(9, 17)]
            .into_iter()
            .collect();
        let mk = |dhan_set: HashSet<String>, groww_set: HashSet<String>| {
            let mut nums: BTreeMap<&'static str, FeedDayNumbers> = BTreeMap::new();
            nums.insert("dhan", FeedDayNumbers::unavailable());
            nums.insert("groww", FeedDayNumbers::unavailable());
            let mut sets: BTreeMap<&'static str, Option<HashSet<String>>> = BTreeMap::new();
            sets.insert("dhan", Some(dhan_set));
            sets.insert("groww", Some(groww_set));
            (nums, sets)
        };
        // Normal dual-feed day: real overlap, no sentinels.
        let (mut nums, sets) = mk(dhan_set.clone(), [minute(9, 15)].into_iter().collect());
        let both_on: BTreeMap<&'static str, bool> =
            [("dhan", false), ("groww", false)].into_iter().collect();
        apply_minute_overlap_and_feed_off_sentinels(&mut nums, &sets, &both_on);
        assert_eq!(nums["dhan"].unique_win_minutes, 2);
        assert_eq!(nums["dhan"].both_minutes, 1);
        assert_eq!(nums["groww"].unique_win_minutes, 0);
        // Groww-off day: dhan's "3 exclusive minutes vs nothing" is NOT a
        // measurement — sentinel, never a phantom month-sum win.
        let (mut nums, sets) = mk(dhan_set, HashSet::new());
        let groww_off: BTreeMap<&'static str, bool> =
            [("dhan", false), ("groww", true)].into_iter().collect();
        apply_minute_overlap_and_feed_off_sentinels(&mut nums, &sets, &groww_off);
        assert_eq!(
            nums["dhan"].unique_win_minutes, SCOREBOARD_UNAVAILABLE_SENTINEL,
            "the RUNNING feed's row on a feed-off day carries no phantom win"
        );
        assert_eq!(nums["dhan"].both_minutes, SCOREBOARD_UNAVAILABLE_SENTINEL);
        // The OFF feed's own measured zeros stay (its row is feed_off and
        // excluded day-level by the runbook SQL anyway).
        assert_eq!(nums["groww"].unique_win_minutes, 0);
    }

    #[test]
    fn test_should_keep_feed_off_outcome_evening_rerun_preserves_no_contest() {
        // Round-5 HIGH (erasure facet) topology: a config-off day stamped
        // feed_off, then a same-day EVENING boot with the feed re-enabled
        // for tomorrow reruns (enabled_now=Some(true), an evening
        // Connected row) and re-measures zero ticks — the rerun must keep
        // feed_off, never re-crown the false one-horse winner.
        assert!(
            should_keep_feed_off_outcome(Some("feed_off"), false, 0),
            "evening rerun with zero ticks preserves feed_off"
        );
        assert!(
            should_keep_feed_off_outcome(Some("feed_off"), false, -1),
            "an unmeasured rerun cannot disprove feed_off either"
        );
        assert!(
            !should_keep_feed_off_outcome(Some("feed_off"), false, 42),
            "real ticks measured = the feed genuinely streamed; upgrade allowed"
        );
        assert!(
            !should_keep_feed_off_outcome(Some("feed_off"), true, 0),
            "a rerun that itself infers feed_off has nothing to keep"
        );
        assert!(
            !should_keep_feed_off_outcome(Some("complete"), false, 0),
            "only an existing feed_off row is protected"
        );
        assert!(!should_keep_feed_off_outcome(None, false, 0), "first run");
        // …and the RunCatchUp latch treats the preserved day as terminal
        // (test_catchup_rerun_is_redundant_terminal_outcomes) so the
        // evening boot skips the rerun entirely — no duplicate winner card.
    }

    #[test]
    fn test_last_minute_secs_of_day_and_post_close_reconnect_is_real_death() {
        // Round-4 MEDIUM: only a feed that streamed through ~15:28 keeps
        // the scheduled-stop carve-out; a hole before close = a REAL
        // in-market crash (the 15:26-crash → 15:31-reconnect shape).
        let minute = |h: i64, m: i64| format!("2026-07-10T{h:02}:{m:02}:00.000000Z");
        let set: HashSet<String> = [minute(9, 15), minute(12, 0), minute(15, 27)]
            .into_iter()
            .collect();
        assert_eq!(last_minute_secs_of_day(&set), Some(15 * 3600 + 27 * 60));
        assert!(
            post_close_reconnect_is_real_death(Some(15 * 3600 + 27 * 60)),
            "last minute 15:27 < 15:28 threshold = real in-market death"
        );
        assert!(
            !post_close_reconnect_is_real_death(Some(15 * 3600 + 28 * 60)),
            "streamed through 15:28 = the clean scheduled-stop shape"
        );
        assert!(
            !post_close_reconnect_is_real_death(Some(15 * 3600 + 29 * 60)),
            "streamed through 15:29 keeps the carve-out"
        );
        assert!(
            post_close_reconnect_is_real_death(None),
            "zero streamed session minutes with a prior-up row is \
             incompatible with a clean full-session stop"
        );
        // Unparsable keys yield None (fail toward the real-death arm —
        // documented residual; query FAILURE keeps the carve-out upstream).
        let junk: HashSet<String> = ["gibberish".to_string()].into_iter().collect();
        assert_eq!(last_minute_secs_of_day(&junk), None);
        assert_eq!(last_minute_secs_of_day(&HashSet::new()), None);
    }

    #[test]
    fn test_catchup_rerun_is_redundant_terminal_outcomes() {
        // Round-4 LOW + round-5 fix: the post-close deploy-restart
        // RunCatchUp skips when BOTH feeds already carry a TERMINAL row —
        // 'complete' OR 'feed_off'. Requiring complete-on-both left the
        // latch permanently dead on single-feed profiles (every day is a
        // feed-off day there) and re-sent a duplicate card every evening
        // boot.
        let both: BTreeMap<String, String> = [
            ("dhan".to_string(), "complete".to_string()),
            ("groww".to_string(), "complete".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(catchup_rerun_is_redundant(&both));
        let mut single_feed = both.clone();
        single_feed.insert("groww".to_string(), "feed_off".to_string());
        assert!(
            catchup_rerun_is_redundant(&single_feed),
            "{{complete, feed_off}} is terminal — the single-feed-profile \
             evening boot sends no duplicate card"
        );
        for worse in ["partial", "degraded"] {
            let mut m = both.clone();
            m.insert("groww".to_string(), worse.to_string());
            assert!(
                !catchup_rerun_is_redundant(&m),
                "{worse} days must re-run (partial may improve; degraded is \
                 keep-better-protected)"
            );
        }
        let mixed: BTreeMap<String, String> = [
            ("dhan".to_string(), "partial".to_string()),
            ("groww".to_string(), "feed_off".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(
            !catchup_rerun_is_redundant(&mixed),
            "{{partial, feed_off}} still re-runs — the partial side may improve"
        );
        let mut one = both.clone();
        one.remove("dhan");
        assert!(!catchup_rerun_is_redundant(&one), "a missing row re-runs");
        assert!(!catchup_rerun_is_redundant(&BTreeMap::new()));
    }

    #[test]
    fn test_scoreboard_trigger_after_auto_stop_warn_threshold() {
        // Round-4 LOW: an accepted trigger at/after 16:15 IST sleeps into
        // the prod 16:30 auto-stop every day — warn loudly at spawn.
        assert!(!scoreboard_trigger_after_auto_stop(56_700), "15:45 default");
        assert!(!scoreboard_trigger_after_auto_stop(58_499), "16:14:59");
        assert!(scoreboard_trigger_after_auto_stop(58_500), "16:15:00");
        assert!(scoreboard_trigger_after_auto_stop(61_200), "17:00");
        assert_eq!(SCOREBOARD_TRIGGER_AUTO_STOP_WARN_SECS, 58_500);
    }

    #[test]
    fn test_parse_existing_daily_coverage_reads_columns_6_through_11() {
        let body = r#"{"dataset":[["dhan","complete",1200,2900,5400,48213,14,361,740,12,270000,"in_memory"],["groww","complete",-1,-1,-1,-1,-1,-1,-1,-1,-1,"sql_backfill"]]}"#;
        let cov = parse_existing_daily_coverage(body).expect("parse");
        assert_eq!(
            cov.get("dhan"),
            Some(&ExistingDailyCoverage {
                unique_win_minutes: 14,
                both_minutes: 361,
                mapped_instruments: 740,
                unmapped_instruments: 12,
                covered_instrument_minutes: 270_000,
                coverage_source: "in_memory".to_string(),
            })
        );
        // Pre-PR-D short rows (no coverage columns) record sentinels + an
        // empty source label, never fabricated values.
        let body = r#"{"dataset":[["dhan","partial",1200,2900,5400,48213]]}"#;
        let cov = parse_existing_daily_coverage(body).expect("parse");
        let d = cov.get("dhan").expect("row");
        assert_eq!(d.unique_win_minutes, SCOREBOARD_UNAVAILABLE_SENTINEL);
        assert_eq!(d.coverage_source, "");
        assert_eq!(parse_existing_daily_coverage("not json"), None);
    }

    #[test]
    fn test_coverage_source_rank_and_fold_existing_coverage_keep_better() {
        assert_eq!(coverage_source_rank("in_memory"), 2);
        assert_eq!(coverage_source_rank("mixed"), 1);
        assert_eq!(coverage_source_rank("sql_backfill"), 0);
        assert_eq!(coverage_source_rank(""), 0, "pre-PR-D rows rank lowest");

        let existing = ExistingDailyCoverage {
            unique_win_minutes: 14,
            both_minutes: 361,
            mapped_instruments: 740,
            unmapped_instruments: 12,
            covered_instrument_minutes: 270_000,
            coverage_source: "in_memory".to_string(),
        };
        // A registry-less rerun (sql_backfill) must fold the measured
        // in_memory row forward — the post-close catch-up erasure class.
        let mut n = FeedDayNumbers::unavailable();
        n.unique_win_minutes = 9; // SQL-derived
        assert!(fold_existing_coverage_keep_better(&mut n, Some(&existing)));
        assert_eq!(n.unique_win_minutes, 14);
        assert_eq!(n.mapped_instruments, 740);
        assert_eq!(n.coverage_source, CoverageSource::InMemory);
        // A same-rank (in_memory) rerun that MEASURED writes its own values.
        let mut n = FeedDayNumbers::unavailable();
        n.coverage_source = CoverageSource::InMemory;
        n.unique_win_minutes = 21;
        n.covered_instrument_minutes = 250_000;
        assert!(!fold_existing_coverage_keep_better(&mut n, Some(&existing)));
        assert_eq!(n.unique_win_minutes, 21);
        // Mixed rerun over an in_memory row still folds forward (rank 1 < 2)
        // and preserves the existing InMemory source label.
        let mut n = FeedDayNumbers::unavailable();
        n.coverage_source = CoverageSource::Mixed;
        assert!(fold_existing_coverage_keep_better(&mut n, Some(&existing)));
        assert_eq!(n.coverage_source, CoverageSource::InMemory);
        // A mixed EXISTING row folds to Mixed.
        let mut mixed_existing = existing.clone();
        mixed_existing.coverage_source = "mixed".to_string();
        let mut n = FeedDayNumbers::unavailable();
        assert!(fold_existing_coverage_keep_better(
            &mut n,
            Some(&mixed_existing)
        ));
        assert_eq!(n.coverage_source, CoverageSource::Mixed);
        // No existing row / a sql_backfill existing row: nothing to keep.
        let mut n = FeedDayNumbers::unavailable();
        assert!(!fold_existing_coverage_keep_better(&mut n, None));
        let mut sql_existing = existing;
        sql_existing.coverage_source = "sql_backfill".to_string();
        let mut n = FeedDayNumbers::unavailable();
        assert!(!fold_existing_coverage_keep_better(
            &mut n,
            Some(&sql_existing)
        ));
    }

    #[test]
    fn test_fold_existing_coverage_keep_better_equal_rank_zero_drain() {
        // PR-D fix round 1 (review HIGH): an evening same-day rerun
        // re-registers today's slots (warm-snapshot boot) but its fresh
        // process folded ZERO bits — equal-rank 'mixed' vs 'mixed' must
        // NOT erase the 15:45 run's measured coverage with zeros (the
        // outcome keep-better's measured-zero precedent, round 5).
        let existing = ExistingDailyCoverage {
            unique_win_minutes: 30,
            both_minutes: 300,
            mapped_instruments: 740,
            unmapped_instruments: 12,
            covered_instrument_minutes: 270_000,
            coverage_source: "mixed".to_string(),
        };
        let mut n = FeedDayNumbers::unavailable();
        n.coverage_source = CoverageSource::Mixed;
        n.covered_instrument_minutes = 0; // registered, zero bits folded
        n.mapped_instruments = 740;
        assert!(fold_existing_coverage_keep_better(&mut n, Some(&existing)));
        assert_eq!(n.covered_instrument_minutes, 270_000);
        assert_eq!(n.unique_win_minutes, 30);
        assert_eq!(n.coverage_source, CoverageSource::Mixed);
        // An equal-rank rerun that DID measure still writes its own values.
        let mut n = FeedDayNumbers::unavailable();
        n.coverage_source = CoverageSource::Mixed;
        n.covered_instrument_minutes = 5;
        assert!(!fold_existing_coverage_keep_better(&mut n, Some(&existing)));
        assert_eq!(n.covered_instrument_minutes, 5);
        // A zero-measured rerun over a ZERO-measured existing row also
        // writes its own values (nothing material to preserve).
        let mut zero_existing = existing;
        zero_existing.covered_instrument_minutes = 0;
        let mut n = FeedDayNumbers::unavailable();
        n.coverage_source = CoverageSource::Mixed;
        n.covered_instrument_minutes = 0;
        assert!(!fold_existing_coverage_keep_better(
            &mut n,
            Some(&zero_existing)
        ));
    }

    // -----------------------------------------------------------------------
    // REST 1m pull digest (Groww REST plan PR-5 — operator Quote 2)
    // -----------------------------------------------------------------------

    fn audit_row(feed: &str, leg: &str, outcome: &str, close_ms: i64, rl: i64) -> RestFetchLiteRow {
        RestFetchLiteRow {
            feed: feed.to_string(),
            leg: leg.to_string(),
            outcome: outcome.to_string(),
            close_to_data_ms: close_ms,
            rate_limited_count: rl,
            error_class: String::new(),
        }
    }

    /// Micros-literal + column-shape regression lock for the two digest
    /// SQL builders (QuestDB 9.3.5 embeds integer TIMESTAMP literals as
    /// MICROSECONDS — a nanos literal silently matches nothing).
    #[test]
    fn test_rest_leg_digest_sql_builders_use_micros_and_expected_columns() {
        let (start_micros, end_micros) = day_bounds_micros(DAY);
        let audit_sql = build_rest_fetch_audit_day_sql(DAY);
        assert!(audit_sql.contains("from rest_fetch_audit"), "{audit_sql}");
        for col in [
            "feed",
            "leg",
            "outcome",
            "close_to_data_ms",
            "rate_limited_count",
        ] {
            assert!(audit_sql.contains(col), "missing {col}: {audit_sql}");
        }
        assert!(
            audit_sql.contains(&format!("ts >= {start_micros}"))
                && audit_sql.contains(&format!("ts < {end_micros}")),
            "micros window literals required: {audit_sql}"
        );
        let fb_sql = build_spot1m_close_latency_day_sql(DAY);
        assert!(fb_sql.contains("from spot_1m_rest"), "{fb_sql}");
        assert!(
            fb_sql.contains("close_to_data_ms >= 0"),
            "the -1 sentinel rows must never reach the distribution: {fb_sql}"
        );
        assert!(
            fb_sql.contains(&format!("ts >= {start_micros}"))
                && fb_sql.contains(&format!("ts < {end_micros}")),
            "micros window literals required: {fb_sql}"
        );
        // Nanos literals must never appear (1000x the micros value).
        let nanos_start = (DAY as i64) * 86_400 * NANOS_PER_SEC;
        assert!(!audit_sql.contains(&nanos_start.to_string()), "{audit_sql}");
        assert!(!fb_sql.contains(&nanos_start.to_string()), "{fb_sql}");
    }

    #[test]
    fn test_parse_rest_fetch_audit_lite_and_latency_rows() {
        let body = r#"{"columns":[],"dataset":[
            ["groww","spot_1m","ok",1042,0],
            ["groww","spot_1m","error",-1,2],
            ["groww","chain_1m","ok",2100,0],
            ["garbage-shape-row"],
            [null,"spot_1m","ok",1,0]
        ]}"#;
        let rows = parse_rest_fetch_audit_lite(body).expect("parsable");
        assert_eq!(rows.len(), 3, "bad-shape rows are skipped: {rows:?}");
        assert_eq!(rows[0], audit_row("groww", "spot_1m", "ok", 1042, 0));
        assert_eq!(rows[1].rate_limited_count, 2);
        assert!(parse_rest_fetch_audit_lite("not json").is_none());
        assert!(parse_rest_fetch_audit_lite(r#"{"no":"dataset"}"#).is_none());

        let fb = parse_feed_close_latency_rows(
            r#"{"columns":[],"dataset":[["dhan",1900],["dhan",61000],["bad"],["groww",120]]}"#,
        )
        .expect("parsable");
        assert_eq!(
            fb,
            vec![
                ("dhan".to_string(), 1900),
                ("dhan".to_string(), 61000),
                ("groww".to_string(), 120)
            ]
        );
        assert!(parse_feed_close_latency_rows("nope").is_none());
    }

    #[test]
    fn test_percentile_nearest_rank_boundaries() {
        assert_eq!(percentile_nearest_rank(&[], 50), -1, "empty = sentinel");
        assert_eq!(percentile_nearest_rank(&[7], 50), 7);
        assert_eq!(percentile_nearest_rank(&[7], 99), 7);
        let v: Vec<i64> = (1..=100).collect();
        assert_eq!(percentile_nearest_rank(&v, 50), 50);
        assert_eq!(percentile_nearest_rank(&v, 99), 99);
        assert_eq!(percentile_nearest_rank(&v, 100), 100);
        // pct > 100 clamps to the max — never an out-of-bounds panic.
        assert_eq!(percentile_nearest_rank(&v, 250), 100);
        let v2 = [10, 20, 30, 40];
        assert_eq!(percentile_nearest_rank(&v2, 50), 20);
        assert_eq!(percentile_nearest_rank(&v2, 99), 40);
    }

    /// The core aggregation semantics: ok-vs-failed-vs-named-gap split,
    /// the ≥60s late-recovery exclusion from the prompt distribution, and
    /// the 429 sum across ALL rows.
    #[test]
    fn test_aggregate_rest_leg_day_outcome_and_late_split() {
        let rows = vec![
            audit_row("groww", "spot_1m", "ok", 1_000, 0),
            audit_row("groww", "spot_1m", "ok", 2_000, 1),
            audit_row("groww", "spot_1m", "ok", 3_000, 0),
            // Late repair (the sweep's honest >60s delay) — counted, NOT
            // in the prompt distribution.
            audit_row("groww", "spot_1m", "ok", 7_200_000, 0),
            audit_row("groww", "spot_1m", "empty", -1, 0),
            audit_row("groww", "spot_1m", "rate_limited", -1, 3),
            audit_row("groww", "spot_1m", "named_gap", -1, 0),
            audit_row("groww", "chain_1m", "ok", 2_100, 0),
        ];
        let out = aggregate_rest_leg_day(&rows, &[]);
        assert_eq!(out.len(), 2);
        let spot = out
            .iter()
            .find(|s| s.leg == "spot_1m")
            .expect("spot summary");
        assert_eq!(spot.feed, "groww");
        assert_eq!(spot.ok_fetches, 4, "late repairs still count ok");
        assert_eq!(spot.failed_fetches, 2, "empty + rate_limited");
        assert_eq!(spot.named_gaps, 1, "gaps are NOT failed fetches");
        assert_eq!(spot.rate_limited_hits, 4, "429s sum across all rows");
        assert_eq!(spot.late_recovered, 1);
        assert_eq!(spot.close_samples, 3, "the late repair is excluded");
        assert_eq!(spot.close_p50_ms, 2_000);
        assert_eq!(spot.close_p99_ms, 3_000);
        assert_eq!(spot.close_max_ms, 3_000);
        let chain = out
            .iter()
            .find(|s| s.leg == "chain_1m")
            .expect("chain summary");
        assert_eq!((chain.ok_fetches, chain.close_p50_ms), (1, 2_100));
        // Empty day → empty vec (the card omits the section honestly).
        assert!(aggregate_rest_leg_day(&[], &[]).is_empty());
    }

    /// An all-pulls-failed key keeps its measured counts with the -1
    /// latency sentinels + a measured-zero sample count — never a
    /// fabricated distribution (Rule 11).
    #[test]
    fn test_aggregate_rest_leg_day_zero_ok_sentinels() {
        let rows = vec![
            audit_row("groww", "chain_1m", "error", -1, 0),
            audit_row("groww", "chain_1m", "no_token", -1, 0),
        ];
        let out = aggregate_rest_leg_day(&rows, &[]);
        let s = &out[0];
        assert_eq!((s.ok_fetches, s.failed_fetches), (0, 2));
        assert_eq!(s.close_samples, 0, "measured zero, not -1");
        assert_eq!(
            (s.close_p50_ms, s.close_p99_ms, s.close_max_ms),
            (-1, -1, -1)
        );
    }

    /// The Dhan spot latency-only fallback: a feed with data-table latency
    /// rows but NO audit rows gets a distribution with -1 COUNT sentinels;
    /// a feed already covered by the audit source is never double-counted.
    #[test]
    fn test_aggregate_rest_leg_day_spot_latency_fallback() {
        let audit = vec![audit_row("groww", "spot_1m", "ok", 1_042, 0)];
        let fallback = vec![
            ("dhan".to_string(), 1_900),
            ("dhan".to_string(), 2_400),
            ("dhan".to_string(), 100_000), // a backfilled repair — late
            ("groww".to_string(), 999),    // audit source wins — ignored
        ];
        let out = aggregate_rest_leg_day(&audit, &fallback);
        assert_eq!(out.len(), 2);
        let dhan = out.iter().find(|s| s.feed == "dhan").expect("dhan spot");
        assert_eq!(dhan.leg, "spot_1m");
        assert_eq!(
            (dhan.ok_fetches, dhan.failed_fetches, dhan.named_gaps),
            (-1, -1, -1),
            "the data table records no outcomes — counts stay sentinels"
        );
        assert_eq!(dhan.rate_limited_hits, -1);
        assert_eq!(dhan.late_recovered, 1);
        assert_eq!(dhan.close_samples, 2);
        assert_eq!(dhan.close_p50_ms, 1_900);
        assert_eq!(dhan.close_max_ms, 2_400);
        let groww = out.iter().find(|s| s.feed == "groww").expect("groww spot");
        assert_eq!(
            groww.close_p50_ms, 1_042,
            "the audit-derived distribution must win over the fallback"
        );
    }

    /// The gauge-label allowlist: bounded static values only — a parsed
    /// slug outside the allowlist is skipped, never a label value.
    #[test]
    fn test_rest_leg_gauge_labels_allowlist() {
        assert_eq!(
            rest_leg_gauge_labels("dhan", "spot_1m"),
            Some(("dhan", "spot_1m"))
        );
        assert_eq!(
            rest_leg_gauge_labels("groww", "chain_1m"),
            Some(("groww", "chain_1m"))
        );
        assert_eq!(
            rest_leg_gauge_labels("groww", "contract_1m"),
            Some(("groww", "contract_1m"))
        );
        assert_eq!(rest_leg_gauge_labels("evil'feed", "spot_1m"), None);
        assert_eq!(rest_leg_gauge_labels("dhan", "weird_leg"), None);
    }

    /// The card-line builder: the four canonical pairs ALWAYS render (the
    /// Quote-2 answer exists even as "not measured yet"), extra measured
    /// pairs (the PR-4 contract leg) append automatically, and wire slugs
    /// never leak into the display names (commandment 2).
    #[test]
    fn test_build_rest_leg_score_lines_canonical_order_and_extras() {
        let summaries = vec![
            RestLegDaySummary {
                feed: "groww".to_string(),
                leg: "spot_1m".to_string(),
                ok_fetches: 1_496,
                failed_fetches: 4,
                named_gaps: 0,
                pre_boot_gaps: 0,
                rate_limited_hits: 0,
                late_recovered: 2,
                close_p50_ms: 1_400,
                close_p99_ms: 3_200,
                close_max_ms: 6_200,
                close_samples: 1_494,
            },
            RestLegDaySummary {
                feed: "groww".to_string(),
                leg: "contract_1m".to_string(),
                ok_fetches: 10,
                failed_fetches: 0,
                named_gaps: 0,
                pre_boot_gaps: 0,
                rate_limited_hits: 0,
                late_recovered: 0,
                close_p50_ms: 900,
                close_p99_ms: 1_500,
                close_max_ms: 1_600,
                close_samples: 10,
            },
        ];
        let lines = build_rest_leg_score_lines(&summaries);
        let names: Vec<(String, String)> = lines
            .iter()
            .map(|l| (l.feed.clone(), l.leg.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("Dhan".to_string(), "spot candles".to_string()),
                ("Dhan".to_string(), "option chain".to_string()),
                ("Groww".to_string(), "spot candles".to_string()),
                ("Groww".to_string(), "option chain".to_string()),
                ("Groww".to_string(), "option contracts".to_string()),
            ]
        );
        // The measured pair carries its numbers; an absent pair carries
        // the honest -1 sentinels everywhere.
        let groww_spot = &lines[2];
        assert_eq!(groww_spot.ok_fetches, 1_496);
        assert_eq!(groww_spot.close_p99_ms, 3_200);
        let dhan_chain = &lines[1];
        assert_eq!(dhan_chain.ok_fetches, -1);
        assert_eq!(dhan_chain.close_samples, -1);
        // Empty summaries still render the four canonical placeholders.
        assert_eq!(build_rest_leg_score_lines(&[]).len(), 4);
    }

    #[test]
    fn test_unmeasured_canonical_rest_pairs_names_only_sentinel_pairs() {
        // F3 (2026-07-15 fix round): the card suppresses unmeasured pairs,
        // so the honest not-measured signal moves to the logs — this pure
        // helper names exactly the canonical pairs with no measured pull
        // source (the §2b always-render contract's log-side leg).
        let summaries = vec![RestLegDaySummary {
            feed: "groww".to_string(),
            leg: "spot_1m".to_string(),
            ok_fetches: 1_496,
            failed_fetches: 4,
            named_gaps: 0,
            pre_boot_gaps: 0,
            rate_limited_hits: 0,
            late_recovered: 2,
            close_p50_ms: 1_400,
            close_p99_ms: 3_200,
            close_max_ms: 6_200,
            close_samples: 1_494,
        }];
        let lines = build_rest_leg_score_lines(&summaries);
        assert_eq!(
            unmeasured_canonical_rest_pairs(&lines),
            vec![
                ("Dhan".to_string(), "spot candles".to_string()),
                ("Dhan".to_string(), "option chain".to_string()),
                ("Groww".to_string(), "option chain".to_string()),
            ]
        );
        // Every canonical pair measured → nothing to log.
        let all = ["spot_1m", "chain_1m"];
        let full: Vec<RestLegDaySummary> = ["dhan", "groww"]
            .iter()
            .flat_map(|f| {
                all.iter().map(|l| RestLegDaySummary {
                    feed: (*f).to_string(),
                    leg: (*l).to_string(),
                    ok_fetches: 5,
                    failed_fetches: 0,
                    named_gaps: 0,
                    pre_boot_gaps: 0,
                    rate_limited_hits: 0,
                    late_recovered: 0,
                    close_p50_ms: 900,
                    close_p99_ms: 1_500,
                    close_max_ms: 1_600,
                    close_samples: 5,
                })
            })
            .collect();
        let lines = build_rest_leg_score_lines(&full);
        assert!(unmeasured_canonical_rest_pairs(&lines).is_empty());
        // Empty build → all four pairs named.
        assert_eq!(
            unmeasured_canonical_rest_pairs(&build_rest_leg_score_lines(&[])).len(),
            4
        );
    }

    #[test]
    fn test_latency_only_pair_is_measured_not_unmeasured() {
        // G6 (fix round 2): the documented spot_1m_rest latency-only
        // fallback (counts -1, latency measured) must classify MEASURED —
        // logging it "not measured" was a false statement during exactly
        // the forensics-writer-outage window the fallback exists for. It
        // is named separately as latency-only instead.
        let summaries = vec![RestLegDaySummary {
            feed: "dhan".to_string(),
            leg: "spot_1m".to_string(),
            ok_fetches: -1,
            failed_fetches: -1,
            named_gaps: -1,
            pre_boot_gaps: -1,
            rate_limited_hits: -1,
            late_recovered: -1,
            close_p50_ms: 1_100,
            close_p99_ms: 1_800,
            close_max_ms: 5_000,
            close_samples: 372,
        }];
        let lines = build_rest_leg_score_lines(&summaries);
        let unmeasured = unmeasured_canonical_rest_pairs(&lines);
        assert!(
            !unmeasured.contains(&("Dhan".to_string(), "spot candles".to_string())),
            "a latency-only pair must never be logged 'not measured': {unmeasured:?}"
        );
        // The other three canonical pairs stay honestly unmeasured.
        assert_eq!(unmeasured.len(), 3);
        // ...and the latency-only classifier names exactly this pair.
        assert_eq!(
            latency_only_canonical_rest_pairs(&lines),
            vec![("Dhan".to_string(), "spot candles".to_string())]
        );
        // A counts-measured pair is never latency-only.
        let counted = vec![RestLegDaySummary {
            feed: "dhan".to_string(),
            leg: "spot_1m".to_string(),
            ok_fetches: 735,
            failed_fetches: 0,
            named_gaps: 0,
            pre_boot_gaps: 0,
            rate_limited_hits: 0,
            late_recovered: 0,
            close_p50_ms: 1_100,
            close_p99_ms: 1_800,
            close_max_ms: 5_000,
            close_samples: 372,
        }];
        let lines = build_rest_leg_score_lines(&counted);
        assert!(latency_only_canonical_rest_pairs(&lines).is_empty());
    }

    // MERGE RESOLVED 2026-07-15: the FEED-GAP-01 dangling-close sweep
    // tests died here with their production fns (the gap-episode
    // machinery retired with the Groww live feed, main #1581); the G6
    // measurement-gap tests above are KEPT (their fns + the
    // aggregation-path log call survive the REST-only runtime).
}
