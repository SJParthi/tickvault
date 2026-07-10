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
//!    then POLLING until this boot's own connect row is visible): a dying
//!    process writes NO disconnect row, so the BOOTING process is its own
//!    correlation evidence. For each `(feed, ws_type, connection_index)`
//!    whose last PRE-boot `ws_event_audit` row was an "up" kind IN SESSION
//!    (the DEATH-window gate) and whose first POST-boot `connected` row
//!    exists, synthesize ONE `process_death` episode at the deterministic
//!    post-boot connected ts (DEDUP-idempotent across repeated boots),
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

use std::collections::{BTreeMap, HashSet};
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
#[must_use]
pub fn build_episode_day_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select feed, episode_kind, blame, market_hours \
         from feed_episode_audit where ts >= {start} and ts < {end}"
    )
}

/// The day's `detector='boot_reconciled'` process-death rows ONLY. The
/// daily run merges these into its IN-MEMORY step-2 tallies instead of
/// reading back its own just-flushed rows (hostile review 2026-07-10:
/// ILP-HTTP ACK = committed-to-WAL, NOT visible-to-SELECT — a same-run
/// read-back can silently miss every episode just written; the
/// boot-reconciled rows were written minutes-to-hours earlier and are long
/// visible). Micros literals.
#[must_use]
pub fn build_boot_reconciled_episode_day_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select feed, episode_kind, blame, market_hours \
         from feed_episode_audit where ts >= {start} and ts < {end} \
         and detector = 'boot_reconciled'"
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
        EPISODE_KIND_PROCESS_DEATH => t.restarts += 1,
        _ => {}
    }
    if headline {
        match blame {
            "broker" => t.blame_broker += 1,
            "ours" => t.blame_ours += 1,
            _ => t.blame_indeterminate += 1,
        }
    }
}

/// Sum every field of `add` into `base` (per feed). Pure — used to merge
/// the boot-reconciled process-death read-back into the in-memory tallies.
pub fn merge_episode_tallies(
    base: &mut BTreeMap<String, EpisodeTally>,
    add: &BTreeMap<String, EpisodeTally>,
) {
    for (feed, a) in add {
        let t = base.entry(feed.clone()).or_default();
        t.disconnects_market += a.disconnects_market;
        t.disconnects_off_hours += a.disconnects_off_hours;
        t.stalls += a.stalls;
        t.restarts += a.restarts;
        t.blame_broker += a.blame_broker;
        t.blame_ours += a.blame_ours;
        t.blame_indeterminate += a.blame_indeterminate;
    }
}

/// Aggregate the [`build_episode_day_sql`] /
/// [`build_boot_reconciled_episode_day_sql`] response per feed. Pure.
#[must_use]
pub fn aggregate_episode_rows(body: &str) -> Option<BTreeMap<String, EpisodeTally>> {
    let rows = parse_dataset(body)?;
    let mut out: BTreeMap<String, EpisodeTally> = BTreeMap::new();
    for row in rows {
        let cols = match row.as_array() {
            Some(c) if c.len() >= 4 => c,
            _ => continue,
        };
        let feed = cols[0].as_str().unwrap_or("").to_string();
        let kind = cols[1].as_str().unwrap_or("");
        let blame = cols[2].as_str().unwrap_or("");
        let market_hours = cols[3].as_bool().unwrap_or(false);
        fold_episode_into_tally(out.entry(feed).or_default(), kind, blame, market_hours);
    }
    Some(out)
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

/// Scan the errors.jsonl directory (blocking file I/O — callers wrap in
/// `spawn_blocking`). Missing dir / unreadable files degrade to
/// `scan_complete = false` evidence — fail-soft, never a panic. O(≤48h of
/// retained hourly files), cold path, once per run.
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
        match std::fs::File::open(&path) {
            Ok(f) => {
                for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Some(ev) = parse_errors_jsonl_line(&line) {
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
    /// Deterministic ts = this boot's first post-boot `connected` row ts.
    pub ts_ist_nanos: i64,
    pub feed: String,
    pub ws_type: String,
    pub connection_index: i64,
    /// Gap between the last pre-boot "up" row and the post-boot connect.
    pub down_secs: i64,
    /// The death window was in market hours (derived from the PRIOR "up"
    /// row — the death happened at/after that instant).
    pub market_hours: bool,
}

/// `true` when `secs_of_day(ts_ist_nanos)` falls in the market-hours
/// window. Pure.
fn nanos_in_market_hours(ts_ist_nanos: i64) -> bool {
    let secs_of_day = ts_ist_nanos.div_euclid(NANOS_PER_SEC).rem_euclid(86_400);
    // APPROVED: rem_euclid(86_400) is always in [0, 86_400) — fits u32.
    u32::try_from(secs_of_day).is_ok_and(is_in_market_hours_secs)
}

/// `true` when at least one post-boot `connected` row exists — the polling
/// gate for the reconciler (this boot's own connect can land minutes after
/// the first query, e.g. behind the 300s WS-GAP-08 429 cooldown). Pure.
#[must_use]
pub fn has_post_boot_connect(rows: &[WsAuditEventLite], boot_ts_ist_nanos: i64) -> bool {
    rows.iter()
        .any(|ev| ev.ts_ist_nanos >= boot_ts_ist_nanos && ev.event_kind == "connected")
}

/// Pure process-death detection over the day's audit rows.
///
/// For each `(feed, ws_type, connection_index)`: the last row BEFORE
/// `boot_ts_ist_nanos` must be an "up" kind (`connected` / `reconnected` /
/// `sleep_resumed` — the connection was live when the process died), and a
/// first `connected` row AT/AFTER boot must exist (this boot's connect —
/// the deterministic episode ts; Groww's by-design double-`connected` per
/// episode collapses to the EARLIEST).
///
/// Gate (hostile review 2026-07-10): synthesis keys on the DEATH window —
/// the prior "up" row must be in-session (`market_hours` column, or its ts
/// inside [09:00, 15:30) IST) — NOT on the boot instant. A 15:20 crash
/// restarted at 15:35 is still a death; the scheduled 16:30 auto-stop →
/// 08:30 start cycle stays excluded because the query is day-scoped (the
/// prior day's rows never appear).
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
        } else if ev.event_kind == "connected" {
            // The EARLIEST post-boot connected row (Groww double-connect
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
        let was_up = matches!(
            prior.event_kind.as_str(),
            "connected" | "reconnected" | "sleep_resumed"
        );
        if !was_up {
            continue;
        }
        // Death-window gate: the connection was up IN SESSION when the
        // process died (prior row's own market_hours flag, or its ts inside
        // the window — belt and braces against a mis-stamped column).
        let prior_in_session = prior.market_hours || nanos_in_market_hours(prior.ts_ist_nanos);
        if !prior_in_session {
            continue;
        }
        out.push(SynthesizedProcessDeath {
            ts_ist_nanos: connect_ts,
            feed,
            ws_type,
            connection_index,
            down_secs: connect_ts
                .saturating_sub(prior.ts_ist_nanos)
                .saturating_div(NANOS_PER_SEC),
            market_hours: prior_in_session,
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
/// attempts are safe by construction. Returns the number of synthesized
/// episodes. Fail-soft everywhere; never blocks boot.
// TEST-EXEMPT: orchestration over the unit-tested pure parts (synthesize_process_death_episodes / has_post_boot_connect / classify_build_sha_changed / parsers); a direct test needs live QuestDB + SSM.
pub async fn reconcile_process_death_episodes(
    questdb: &QuestDbConfig,
    target_ist_day: u64,
    trading_date_ist_nanos: i64,
    boot_ts_ist_nanos: i64,
) -> usize {
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
        return 0;
    };
    let sql = build_ws_events_day_sql(target_ist_day);
    let mut rows: Option<Vec<WsAuditEventLite>> = None;
    for attempt in 1..=PROCESS_DEATH_RECONCILE_MAX_ATTEMPTS {
        let parsed = match exec_query(&client, questdb, &sql).await {
            Some(body) => parse_ws_events(&body),
            None => None,
        };
        match parsed {
            // This boot's connect is visible — the pairing input is ready.
            Some(r) if has_post_boot_connect(&r, boot_ts_ist_nanos) => {
                rows = Some(r);
                break;
            }
            // Readable but no post-boot connect YET (429 cooldown / stagger
            // / audit-flush latency — or the feed is disabled/off-hours and
            // will never connect). Keep the latest view; retry.
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
             synthesized this boot"
        );
        return 0;
    };
    if !has_post_boot_connect(&rows, boot_ts_ist_nanos) {
        info!(
            "feed_scoreboard: no post-boot connect row appeared inside the \
             reconcile poll window — nothing synthesized (feed disabled / \
             off-hours boot, or the connect is still pending; a later re-run \
             with TICKVAULT_SCOREBOARD_NOW re-attempts nothing here — the \
             15:45 aggregation still counts the coverage hole honestly)"
        );
        return 0;
    }
    let deaths = synthesize_process_death_episodes(&rows, boot_ts_ist_nanos);
    if deaths.is_empty() {
        info!("feed_scoreboard: process-death reconciler found no gaps (clean boot)");
        return 0;
    }
    // Deploy-vs-crash sub-reason (control-plane SSM read; fail-soft).
    let deployed_sha = fetch_deployed_binary_sha().await;
    let sha_changed = classify_build_sha_changed(
        tickvault_common::build_info::BUILD_GIT_SHA,
        deployed_sha.as_deref(),
    );
    let mut writer = FeedEpisodeAuditWriter::new(questdb);
    let mut appended = 0_usize;
    for d in &deaths {
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
        let row = FeedEpisodeAuditRow {
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
            down_secs: d.down_secs,
            // Death-window flag from the prior "up" row — the death
            // happened at/after that instant, not at the boot instant.
            market_hours: d.market_hours,
            evidence: format!(
                "prior state up; gap {}s before this boot's first connect",
                d.down_secs
            ),
            run_partial: false,
        };
        match writer.append_row(&row) {
            Ok(()) => appended += 1,
            Err(err) => error!(
                code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                stage = "reconcile_append",
                ?err,
                "SCOREBOARD-01: process-death episode append failed"
            ),
        }
    }
    if let Err(err) = writer.flush() {
        error!(
            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
            stage = "reconcile_flush",
            ?err,
            "SCOREBOARD-01: process-death episode flush failed (QuestDB down?)"
        );
    }
    metrics::counter!("tv_feed_scoreboard_process_death_synthesized_total")
        .increment(appended as u64);
    info!(
        synthesized = appended,
        "feed_scoreboard: process-death reconciler synthesized episodes \
         (blame ours; deterministic ts — repeat boots UPSERT in place)"
    );
    appended
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
                    if ev.event_kind == "reconnected" {
                        *recon.entry(ev.feed.clone()).or_default() += 1;
                    }
                    if let Some(episode) =
                        classify_ws_event_to_episode(ev, &corr, trading_date_ist_nanos)
                    {
                        fold_episode_into_tally(
                            mem.entry(episode.feed.clone()).or_default(),
                            episode.episode_kind,
                            episode.blame.as_str(),
                            episode.market_hours,
                        );
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
    //    with the boot-reconciled process-death rows read back from
    //    feed_episode_audit (written at boot + minutes — long visible, so
    //    the read-back is safe for THOSE rows). The full-table read-back is
    //    ONLY the fallback when the ws read itself failed (rows an earlier
    //    same-day run wrote are better than nothing; that path is already
    //    stamped partial).
    let tallies: Option<BTreeMap<String, EpisodeTally>> = match mem_tallies {
        Some(mut mem) => {
            let boot_sql = build_boot_reconciled_episode_day_sql(target_ist_day);
            match exec_query(&client, questdb, &boot_sql).await {
                Some(body) => match aggregate_episode_rows(&body) {
                    Some(boot_rows) => merge_episode_tallies(&mut mem, &boot_rows),
                    None => {
                        error!(
                            code = ErrorCode::Scoreboard01AggregationDegraded.code_str(),
                            stage = "boot_reconciled_parse",
                            "SCOREBOARD-01: boot-reconciled episode read-back \
                             unparsable — restart counts may under-count"
                        );
                        sources_complete = false;
                    }
                },
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
                    Some(body) => aggregate_episode_rows(&body),
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

    // 7. AUDIT-WS-01 under-count cross-check (self-scrape; a dropped audit
    //    row means today's episode counts are a floor → degraded).
    let mut degraded = false;
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
    // The persisted row is stamped partial for an early forced run too —
    // it covers the day only up to the run time (hostile review
    // 2026-07-10: a pre-15:45 forced run consumes the day's single
    // scheduled run, so its mid-day numbers must never masquerade as a
    // complete end-of-day row).
    let partial_coverage = !sources_complete;
    let row_partial = partial_coverage || forced_early_run;
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
        assert_eq!(deaths[0].down_secs, 3_720);
        assert_eq!(deaths[0].feed, "dhan");
        assert!(deaths[0].market_hours, "death window was in-session");
        // Polling gate helper: the post-boot connect is visible here.
        assert!(has_post_boot_connect(&rows, boot));
        assert!(
            !has_post_boot_connect(&rows[..1], boot),
            "pre-boot rows alone must not satisfy the polling gate"
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
        assert!(deaths[0].market_hours);
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
        assert!(!has_post_boot_connect(&no_connect, day_ts(39_600)));
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
    fn test_aggregate_episode_rows_tallies_per_feed() {
        let body = r#"{"dataset":[
            ["dhan", "disconnect", "broker", true],
            ["dhan", "disconnect", "indeterminate", true],
            ["dhan", "off_hours_disconnect", "indeterminate", false],
            ["dhan", "process_death", "ours", true],
            ["groww", "stall_restart", "broker", true],
            ["groww", "disconnect", "ours", true]
        ]}"#;
        let tallies = aggregate_episode_rows(body).expect("aggregate");
        let d = tallies.get("dhan").copied().expect("dhan tally");
        assert_eq!(d.disconnects_market, 2);
        assert_eq!(d.disconnects_off_hours, 1);
        assert_eq!(d.restarts, 1);
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
    fn test_merge_episode_tallies_sums_boot_reconciled_rows_in() {
        let mut base: BTreeMap<String, EpisodeTally> = BTreeMap::new();
        base.insert(
            "dhan".to_string(),
            EpisodeTally {
                disconnects_market: 2,
                blame_broker: 1,
                blame_indeterminate: 1,
                ..EpisodeTally::default()
            },
        );
        let mut add: BTreeMap<String, EpisodeTally> = BTreeMap::new();
        add.insert(
            "dhan".to_string(),
            EpisodeTally {
                restarts: 1,
                blame_ours: 1,
                ..EpisodeTally::default()
            },
        );
        add.insert(
            "groww".to_string(),
            EpisodeTally {
                restarts: 1,
                blame_ours: 1,
                ..EpisodeTally::default()
            },
        );
        merge_episode_tallies(&mut base, &add);
        let d = base.get("dhan").copied().expect("dhan");
        assert_eq!(d.disconnects_market, 2);
        assert_eq!(d.restarts, 1);
        assert_eq!(d.blame_broker, 1);
        assert_eq!(d.blame_ours, 1);
        assert_eq!(d.blame_indeterminate, 1);
        let g = base
            .get("groww")
            .copied()
            .expect("groww absent in base is created");
        assert_eq!(g.restarts, 1);
        assert_eq!(g.blame_ours, 1);
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
