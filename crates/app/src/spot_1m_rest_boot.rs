//! Per-minute spot 1m REST pipeline — PR-2, the SPOT half (operator grant
//! 2026-07-12; runbook `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! Every trading-day minute close in session — the 09:15 candle closes at
//! 09:16:00 IST; the last (15:29) candle closes at 15:30:00 — this task
//! wakes shortly after the boundary and fetches THAT just-closed minute's
//! official 1m OHLCV for the 4 IDX_I spot indices (NIFTY 13, BANKNIFTY 25,
//! SENSEX 51, INDIA VIX 21 — VIX per the operator scope addition
//! 2026-07-13, relayed via the coordinator session: INDIA VIX joins the
//! spot 1m pull, SPOT ONLY, no option chain) via Dhan
//! `POST /v2/charts/intraday` (interval `"1"`), then
//! persists to the `spot_1m_rest` QuestDB table (DEDUP-idempotent — a
//! re-fetch UPSERTs in place). Cold path ONLY: the WS candle pipeline,
//! tick capture and trading are untouched.
//!
//! ## Just-closed-minute availability (honest probe)
//! Dhan's docs do NOT document how quickly the just-closed minute's candle
//! appears in the intraday response. Each fire therefore carries a bounded
//! in-minute re-poll ladder (`SPOT_1M_REST_RETRY_OFFSETS_MS` — ~0.7s / 1.5s
//! / 3s / 6s after the first attempt) and records the
//! `tv_spot1m_close_to_data_ms` histogram (minute close → successful
//! retrieval) as the live measurement. A minute whose candle never appears
//! is `outcome="empty"` — counted, edge-tracked, never silent (Rule 11).
//!
//! ## 2026-07-13 live-failure hotfix — proven day window + backfill sweep
//! The first live session (2026-07-13) failed EVERY minute with
//! `ok=0/errors=0/empty=3` ("2xx but the minute's candle never appeared"):
//! the original same-date 60-second request window
//! (`fromDate = minute open, toDate = open + 60s`) was answered `2xx`
//! WITHOUT the target candle all session, while the matcher itself was
//! verified correct (same conversion as the live-proven cross-verify).
//! Fixes:
//! 1. **Window**: each fire now sends the ONLY live-proven window shape —
//!    day-granular `fromDate = D 00:00:00, toDate = D+1 00:00:00` (the
//!    exact body the 15:31 cross-verify + prev-day fetchers use daily) —
//!    and filters client-side to the exact minute (over-delivery: a full
//!    session ≈ 375 candles ≈ 20 KB, far under the 2 MiB body cap).
//! 2. **Previous-minute backfill**: the full-day response covers earlier
//!    minutes, so each fire ALSO persists the previous minute when it was
//!    not successfully persisted (per-SID in-memory [`PersistTracker`];
//!    DEDUP UPSERT keys make re-appends idempotent). Edge accounting stays
//!    honest: a fire's verdict is its OWN target minute's fetch+persist —
//!    a minute that lands only via next-fire backfill was still that
//!    fire's failure; the backfilled row's `close_to_data_ms` column
//!    stamps the REAL retrieval delay (> 60 s), while the
//!    `tv_spot1m_close_to_data_ms` histogram keeps sampling ONLY own-fire
//!    retrievals (its defined "just-closed availability" semantics).
//! 3. **Post-session sweep** (M1, review 2026-07-13): the per-minute
//!    backfill looks back exactly ONE minute, so a minute that stays
//!    absent for ≥2 consecutive fires — or a backfill row lost to a
//!    failed flush — stays absent DURING the session; and the 15:30:00
//!    fire is the last, so a vendor-late 15:29 candle has no in-session
//!    repair path at all. The one bounded post-session sweep
//!    (~15:33:30 IST, single fire, [`run_post_session_sweep`]) closes all
//!    three: it re-fetches the day window once per SID and persists every
//!    session minute above the watermark that is still missing. The sweep
//!    fires at 15:33:30 — NOT 15:31 — so its ≤4 requests clear the 15:31
//!    bulk cross-verify's burst window (2026-07-13 live session: 91/776
//!    cross-verify fetches 429'd at 15:31–15:33; see the 429-coordination
//!    follow-up in `rest-1m-pipeline-error-codes.md`).
//!
//! **First-attempt timing (honest):** the boundary sleep is computed on a
//! SECOND-granular clock plus the 300 ms fire delay, so the first attempt
//! lands anywhere from ~0.3 s to ~1.3 s after the minute close (never a
//! sub-second guarantee). Each SID's whole ladder is HARD-bounded by
//! `tokio::time::timeout(SPOT_1M_REST_SID_BUDGET_SECS)` and each request by
//! `SPOT_1M_REST_REQUEST_TIMEOUT_SECS`, so a fire can never overrun the
//! minute; if a boundary IS ever missed (suspend / clock step), the miss is
//! COUNTED (`tv_spot1m_boundary_skipped_total`) + coalesced-logged + fed
//! into the failure edge — never silent (Rule 11).
//!
//! ## Rate budget (Dhan Data-API 5/sec)
//! One fire = 4 concurrent requests (one per index), plus at most 4 ladder
//! re-polls per index spread over ~6 s and staggered by the deterministic
//! per-SID jitter (0/150/300/450 ms) — worst case 4 requests at the fire
//! instant, inside the 5/sec Data-API budget (verified for the 2026-07-13
//! 4-SID arity; re-poll instants never exceed the initial burst).
//!
//! ## INDIA VIX (2026-07-13) — live-probe unknown, per-SID independence
//! Whether Dhan's intraday endpoint serves INDIA VIX 1m candles at all is
//! a LIVE-PROBE UNKNOWN. Two guarantees keep that honest:
//! 1. **Per-SID independence** — each SID rides its OWN budgeted ladder in
//!    its OWN JoinSet task, and the escalation edge's "fully failed" means
//!    ZERO SIDs succeeded ([`minute_fully_failed`]) — a never-serving VIX
//!    can never delay, fail, or edge-count a minute where the other 3
//!    succeeded (unit-pinned below).
//! 2. **Per-SID persistent-empty detector** ([`SidServedTracker`]) — a SID
//!    accumulating [`SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD`] consecutive
//!    empty/failed minutes WHILE ≥1 other SID succeeded in those same
//!    minutes (vendor-not-serving, NOT a global outage) fires ONE
//!    edge-latched `SPOT1M-01 stage="sid_not_served"` page per SID
//!    (re-armed only by that SID's own recovery — one Info ping).
//!
//! ## Volume (index candles)
//! Index candles legitimately carry zero/absent volume — the columnar
//! parser defaults an absent `volume` to `0` and NOTHING in this module
//! treats `volume == 0` as an error: a candle is "served" purely by its
//! presence at the target minute (OHLC), so VIX/index rows with volume 0
//! persist normally.
//!
//! ## Boot wiring (honest answer, module contract)
//! Spawned from `main.rs::spawn_post_market_tasks` — the SAME seam as the
//! rest_canary — which is invoked from BOTH boot paths (the fast
//! crash-recovery arm at main.rs ~2911 AND the slow lane at ~8273,
//! boot-symmetry 2026-06-09), so a mid-session crash restart re-arms this
//! task; the process-global once-guard there prevents duplicates on runtime
//! Dhan cold-start cycles. BOTH call sites are Dhan-gated — correct here,
//! because this fetcher is Dhan-REST-dependent (token).
//!
//! **2026-07-13 update (Phase A, Dhan-live-feed removal):** the old
//! "a Groww-only session runs NO spot-1m REST fetch" limitation is CLOSED —
//! with `dhan_enabled = false` (now the locked default; the live WS lane is
//! retired) the Dhan REST-only stack (`crate::dhan_rest_stack`) spawns this
//! scheduler with its OWN TokenManager, mirroring the
//! spawn_post_market_tasks shape (same params, same config gate, same
//! spot→chain sequencing channel).
//!
//! ## Lifetime
//! The task runs today's remaining minute closes and exits after 15:30 IST
//! (or immediately on a non-trading day) — the AWS box stops at 16:30 IST
//! and cold-boots fresh each trading morning, the same single-day-pass
//! rationale as the rest_canary. A supervised respawn wrapper
//! (classify_join_exit + `tv_spot1m_task_respawn_total{reason}` + bounded
//! backoff) makes sure the scheduler can never die silently mid-session.
//! Panic honesty (the TICK-FLUSH-01 precedent): the release profile sets
//! `panic = "abort"`, so a panicked task aborts the PROCESS in prod — the
//! panic-respawn arm is an unwind-build (dev/test) self-heal path only.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate};
use secrecy::ExposeSecret;
use tracing::{error, info, warn};

use tickvault_common::config::{QuestDbConfig, SpotFetchMode};
use tickvault_common::constants::{
    DHAN_CHARTS_INTRADAY_PATH, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
    SPOT_1M_REST_429_EXTRA_BACKOFF_MS, SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD,
    SPOT_1M_REST_DEGRADE_AFTER_CONSECUTIVE_NO_DATA_MINUTES, SPOT_1M_REST_FIRE_DELAY_MS,
    SPOT_1M_REST_FIRE_STALE_GRACE_SECS, SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST,
    SPOT_1M_REST_INDICES, SPOT_1M_REST_LADDER_JITTER_SLOTS, SPOT_1M_REST_LADDER_JITTER_STEP_MS,
    SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST, SPOT_1M_REST_MAX_BODY_BYTES,
    SPOT_1M_REST_REQUEST_TIMEOUT_SECS, SPOT_1M_REST_RETRY_OFFSETS_MS, SPOT_1M_REST_SID_BUDGET_SECS,
    SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::{capture_rest_error_body, redact_url_params};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::types::SecurityId;
use tickvault_common::url_join::join_api_url;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::disk_health_watcher::classify_join_exit;
use tickvault_storage::rest_fetch_audit_persistence::{
    REST_FETCH_LEG_SPOT_1M, RestFetchAuditRow, RestFetchAuditWriter, RestFetchOutcome,
    ensure_rest_fetch_audit_table,
};
use tickvault_storage::spot_1m_rest_persistence::{
    SPOT_1M_REST_FEED_DHAN, SPOT_1M_REST_SEGMENT_IDX_I, Spot1mRestRow, Spot1mRestWriter,
    ensure_spot_1m_rest_table,
};

use crate::dhan_intraday_parse::{MinuteCandle, intraday_request_body, parse_intraday_1m_candles};

/// Dhan `instrument` enum value for IDX_I index rows.
const SPOT_1M_INSTRUMENT_INDEX: &str = "INDEX";
/// Backoff before the supervisor respawns a dead/failed scheduler run.
const SPOT_1M_RESPAWN_BACKOFF_SECS: u64 = 30;
/// Milliseconds per second / per day (wall-clock latency math).
const MILLIS_PER_SEC: i64 = 1_000;
const MILLIS_PER_DAY: i64 = 86_400_000;
/// Nanoseconds per second (IST-epoch → nanos).
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Everything the scheduler needs, cloneable so the supervisor can respawn
/// the inner run (all fields are `Arc`s or cheap owned copies).
#[derive(Clone)]
pub struct Spot1mRestTaskParams {
    /// Live token handle — re-`load()`ed EVERY fire (the 24h JWT rotates
    /// mid-session; AUTH-GAP-05 re-mints swap it atomically).
    pub token_handle: TokenHandle,
    /// Telegram dispatcher for the edge page + recovery ping.
    pub notifier: Arc<NotificationService>,
    /// Trading calendar (weekday + NSE-holiday aware) — re-checked every
    /// loop iteration (audit-findings Rule 3).
    pub calendar: Arc<TradingCalendar>,
    /// QuestDB target for the `spot_1m_rest` table.
    pub questdb: QuestDbConfig,
    /// Dhan REST v2 base URL (joined via `join_api_url` — never `format!`).
    pub rest_api_base_url: String,
    /// PR-3 sequencing signal (OPTIONAL — plumbed only when the chain leg
    /// is enabled): the boundary seconds-of-day of the minute this task
    /// just finished firing, published at the END of each fire (success or
    /// failure) via `send_replace` (never fails, receivers optional). The
    /// option-chain leg wakes on it so it fires immediately AFTER the spot
    /// fetch; `None` keeps PR-2 behaviour byte-identical.
    pub minute_done_tx: Option<tokio::sync::watch::Sender<Option<u32>>>,
    /// 2026-07-14 serving-delay diagnostics rider: when `true`, two
    /// one-shot LOG-ONLY probe moments (the first session fire after boot
    /// + once at `diagnostics_second_probe_secs_of_day_ist`) each issue 3
    /// bounded extra requests for ONE SID — the cross-verify's byte-exact
    /// day window, the previous-trading-day window, and a same-day-to-now
    /// window — and log the request bodies + response shapes side by side.
    /// Never touches the fetch / persist / edge behaviour.
    pub diagnostics_enabled: bool,
    /// IST seconds-of-day of the second one-shot probe (default 11:00 IST
    /// from config). Inert while `diagnostics_enabled` is false.
    pub diagnostics_second_probe_secs_of_day_ist: u32,
    /// 2026-07-14 architecture optionality (`[spot_1m_rest] fetch_mode`,
    /// pending the ~15:40 IST sweep-discriminator operator ruling):
    /// `PerMinute` = the shipped behaviour; `BatchCatchup` = a sweep-style
    /// catch-up every [`Self::batch_interval_minutes`] instead of
    /// per-minute fires (a thin mode wrapper over the existing sweep
    /// machinery — NOT new fetch logic).
    pub fetch_mode: SpotFetchMode,
    /// Batch catch-up cadence (minutes, validated 1..=60 at boot). Inert
    /// in `PerMinute` mode.
    pub batch_interval_minutes: u32,
}

// ---------------------------------------------------------------------------
// Pure scheduling primitives
// ---------------------------------------------------------------------------

/// The next minute-close fire boundary at-or-after `now_secs_of_day`, on
/// the IST seconds-of-day domain. Boundaries are the exact minute marks
/// `[09:16:00, 15:30:00]` INCLUSIVE (each targets the minute that CLOSED
/// there — 09:16:00 fires for the 09:15 candle; 15:30:00 for 15:29).
/// `None` once today's window is past. Pure.
#[must_use]
pub fn next_minute_close_fire(now_secs_of_day: u32) -> Option<u32> {
    if now_secs_of_day <= SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST {
        return Some(SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST);
    }
    let next_boundary = now_secs_of_day.div_ceil(60).saturating_mul(60);
    (next_boundary <= SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST).then_some(next_boundary)
}

/// The next boundary STRICTLY AFTER the last fired one (2026-07-12
/// hostile-review H1 fix): a fire that completes within its own boundary
/// second must never re-select the SAME boundary — `last_fired` advances
/// the horizon to `last_fired + 1` so instant-completing (or
/// instant-failing) fires can't duplicate fetches, double-count edge
/// "minutes", or double-sample the latency histogram. Pure.
#[must_use]
pub fn next_fire_after(now_secs_of_day: u32, last_fired: Option<u32>) -> Option<u32> {
    let horizon = match last_fired {
        Some(lf) => now_secs_of_day.max(lf.saturating_add(1)),
        None => now_secs_of_day,
    };
    next_minute_close_fire(horizon)
}

/// How many fire boundaries fell inside `(last_boundary, now]` — i.e. were
/// MISSED while a long fire / suspend held the loop past them (2026-07-12
/// hostile-review H2 fix). `last_boundary` must itself be a boundary
/// (multiple of 60); the count clamps at the session's last boundary. Each
/// missed boundary is a minute we will NEVER fetch this session — the
/// caller counts it loudly and feeds it into the failure edge. Pure.
#[must_use]
pub fn count_missed_boundaries(last_boundary: u32, now_secs_of_day: u32) -> u32 {
    let hi = now_secs_of_day.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST);
    if hi <= last_boundary {
        return 0;
    }
    (hi / 60).saturating_sub(last_boundary / 60)
}

/// `true` when a wake at `woke_at_secs_of_day` is fresh enough to fetch the
/// minute that closed at `fire_secs_of_day` (suspend / clock-step defense —
/// the rest_canary `probe_is_fresh` precedent). A midnight-wrap wake
/// (seconds-of-day below the boundary) is stale. Pure.
#[must_use]
pub fn fire_is_fresh(fire_secs_of_day: u32, woke_at_secs_of_day: u32) -> bool {
    woke_at_secs_of_day >= fire_secs_of_day
        && woke_at_secs_of_day - fire_secs_of_day <= SPOT_1M_REST_FIRE_STALE_GRACE_SECS
}

/// `true` once today's fire window is over (or today is not a trading day)
/// — the supervisor's "legitimate clean completion" test. Pure.
#[must_use]
pub fn spot_1m_day_is_over(now_secs_of_day: u32, is_trading_day: bool) -> bool {
    !is_trading_day || now_secs_of_day > SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST
}

/// The `/v2/charts/intraday` request body for ONE index — the PROVEN
/// day-granular window (`fromDate = D 00:00:00`, `toDate = D+1 00:00:00`),
/// the EXACT shape the 15:31 cross-verify + prev-day fetchers use live
/// daily (delegates to the shared [`intraday_request_body`] builder).
///
/// 2026-07-13 live-failure hotfix: the original same-date
/// `[minute open, open + 60s]` window was answered `2xx` WITHOUT the
/// target candle for EVERY session minute (SPOT1M-01 all day, `empty=3`
/// per fire) — that window shape was never live-proven. The consumer
/// filters the full-day response client-side to the exact minute;
/// over-delivery is ~375 candles ≈ 20 KB, far under the 2 MiB body cap.
/// Pure.
#[must_use]
pub fn spot_1m_day_request_body(security_id: &str, trading_date: NaiveDate) -> serde_json::Value {
    let next_day = trading_date.succ_opt().unwrap_or(trading_date);
    intraday_request_body(
        security_id,
        SPOT_1M_REST_SEGMENT_IDX_I,
        SPOT_1M_INSTRUMENT_INDEX,
        trading_date,
        next_day,
    )
}

/// IST-wall-clock-as-epoch nanoseconds for a minute open on `trading_date`
/// — the same representation `candles_1m.ts` and the cross-verify use, so
/// `spot_1m_rest.ts` joins exactly against the live candle tables. Pure.
#[must_use]
pub fn minute_open_ist_nanos(trading_date: NaiveDate, minute_open_secs_of_day: u32) -> i64 {
    let day_start_secs = trading_date
        .and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(0);
    day_start_secs
        .saturating_add(i64::from(minute_open_secs_of_day))
        .saturating_mul(NANOS_PER_SEC)
}

/// Select the candle whose IST-minute bucket equals the target from a
/// parsed columnar response (the response may over-deliver — filter
/// client-side to the exact minute). Pure.
#[must_use]
pub fn select_minute_candle(
    candles: &[MinuteCandle],
    target_minute_ist_nanos: i64,
) -> Option<MinuteCandle> {
    candles
        .iter()
        .copied()
        .find(|c| c.minute_ts_ist_nanos == target_minute_ist_nanos)
}

/// Parse a Dhan intraday columnar body and pick the target minute's candle
/// PLUS (when requested) the previous-minute backfill candle from the SAME
/// full-day body, PLUS the body's diagnostic shape stats
/// (`(rows, newest candle minute nanos)` — 2026-07-14 serving-delay
/// instrumentation; `None` for a zero-candle parse). Malformed / short /
/// length-mismatched bodies parse to an empty set (the reused panic-free
/// columnar parser) and therefore yield `(None, None, None)`. Pure.
#[must_use]
pub fn parse_intraday_columnar_for_minutes(
    body: &str,
    target_minute_ist_nanos: i64,
    backfill_minute_ist_nanos: Option<i64>,
) -> (
    Option<MinuteCandle>,
    Option<MinuteCandle>,
    Option<(usize, i64)>,
) {
    let candles = parse_intraday_1m_candles(body);
    (
        select_minute_candle(&candles, target_minute_ist_nanos),
        backfill_minute_ist_nanos.and_then(|b| select_minute_candle(&candles, b)),
        body_stats(&candles),
    )
}

// ---------------------------------------------------------------------------
// 2026-07-14 serving-delay diagnostics — empty-classification split
// ---------------------------------------------------------------------------

/// Diagnostic classification of a 2xx-but-no-target-candle ladder verdict
/// (2026-07-14 serving-delay instrumentation). The pre-split `empty`
/// outcome collapsed two VERY different vendor states; the split is the
/// per-minute discriminator between "Dhan has NO same-day data at all"
/// and "Dhan serves same-day data with a LAG". Edge / backfill / persist
/// semantics are unchanged — both classes still count as an empty minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyClass {
    /// Every 2xx body of the ladder parsed to ZERO candles for the day —
    /// the vendor is returning an empty day (`outcome="empty_no_rows"`).
    NoRows,
    /// Candles present but none at/after the target minute — the vendor
    /// is serving the day with a measurable delay
    /// (`outcome="empty_stale"`).
    Stale {
        /// Candle count in the freshest 2xx body of the ladder.
        rows_in_response: usize,
        /// The freshest body's newest candle (IST minute-open nanos).
        last_candle_ist_nanos: i64,
        /// The vendor SERVING LAG: `target minute open − last candle
        /// minute open`, in whole seconds, clamped ≥ 0 (a body carrying
        /// candles BEYOND the target while the exact target is missing is
        /// a pathological gap — recorded as lag 0, never negative).
        serving_lag_secs: i64,
    },
}

/// The vendor serving lag in whole seconds for a stale body: target minute
/// open − newest candle minute open, clamped ≥ 0. Pure.
#[must_use]
pub fn serving_lag_secs(target_minute_ist_nanos: i64, last_candle_ist_nanos: i64) -> i64 {
    target_minute_ist_nanos
        .saturating_sub(last_candle_ist_nanos)
        .max(0)
        / NANOS_PER_SEC
}

/// Classify an empty ladder verdict from the freshest 2xx body's shape.
/// `None` stats (no 2xx body ever parsed any rows — including the
/// zero-candle parse of a malformed body) → [`EmptyClass::NoRows`]. Pure.
#[must_use]
pub fn classify_empty_body(
    freshest_body: Option<(usize, i64)>,
    target_minute_ist_nanos: i64,
) -> EmptyClass {
    match freshest_body {
        Some((rows_in_response, last_candle_ist_nanos)) if rows_in_response > 0 => {
            EmptyClass::Stale {
                rows_in_response,
                last_candle_ist_nanos,
                serving_lag_secs: serving_lag_secs(target_minute_ist_nanos, last_candle_ist_nanos),
            }
        }
        _ => EmptyClass::NoRows,
    }
}

/// The freshest 2xx body's `(rows, newest candle minute nanos)` — `None`
/// for a zero-candle parse (the caller keeps the LAST non-empty view so a
/// transiently-empty later rung never launders an earlier stale view into
/// `NoRows`... it can't: a later NON-empty body simply replaces the
/// stats, and a later EMPTY body leaves them untouched). Pure.
#[must_use]
pub fn body_stats(candles: &[MinuteCandle]) -> Option<(usize, i64)> {
    candles
        .iter()
        .map(|c| c.minute_ts_ist_nanos)
        .max()
        .map(|last| (candles.len(), last))
}

/// Per-fire aggregate of the empty-class diagnostics for the coalesced
/// minute log (2026-07-14 serving-delay instrumentation). Log-only — the
/// edge / persist / backfill semantics never read it. Pure state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EmptyDiagnostics {
    /// SIDs whose ladder ended `empty_no_rows` this fire.
    pub no_rows: usize,
    /// SIDs whose ladder ended `empty_stale` this fire.
    pub stale: usize,
    /// Max candle count across the stale SIDs' freshest bodies.
    pub max_rows_in_response: usize,
    /// Max vendor serving lag across the stale SIDs (whole seconds).
    pub max_serving_lag_secs: i64,
    /// Newest candle seen across the stale SIDs (IST minute-open nanos).
    pub latest_candle_ist_nanos: Option<i64>,
}

impl EmptyDiagnostics {
    /// Fold one SID's empty classification into the fire's aggregate.
    pub fn record(&mut self, class: EmptyClass) {
        match class {
            EmptyClass::NoRows => self.no_rows = self.no_rows.saturating_add(1),
            EmptyClass::Stale {
                rows_in_response,
                last_candle_ist_nanos,
                serving_lag_secs,
            } => {
                self.stale = self.stale.saturating_add(1);
                self.max_rows_in_response = self.max_rows_in_response.max(rows_in_response);
                self.max_serving_lag_secs = self.max_serving_lag_secs.max(serving_lag_secs);
                self.latest_candle_ist_nanos = Some(
                    self.latest_candle_ist_nanos
                        .map_or(last_candle_ist_nanos, |cur| cur.max(last_candle_ist_nanos)),
                );
            }
        }
    }
}

/// IST 12-hour label for an IST minute-open nanosecond instant on the
/// given trading day (`"10:38 AM"`) — commandment-9 wording for the
/// coalesced diagnostics fields. Out-of-day instants clamp into the day.
/// Pure.
#[must_use]
pub fn ist_nanos_minute_label(minute_ist_nanos: i64, trading_date_nanos: i64) -> String {
    let secs_of_day = (minute_ist_nanos.saturating_sub(trading_date_nanos) / NANOS_PER_SEC)
        .clamp(0, i64::from(SECONDS_PER_DAY) - 1);
    // APPROVED: clamped into [0, SECONDS_PER_DAY) — the cast is safe.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    format_minute_ist_12h(secs_of_day as u32)
}

/// Nanoseconds per minute (backfill arithmetic).
const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SEC;

/// The previous minute to BACKFILL on this fire, or `None` when no backfill
/// is due: the previous minute must be inside today's session (at or after
/// the 09:15 open) and must not already be persisted for this SID. One-minute
/// lookback only — older gaps stay absent (re-fetchable manually; DEDUP
/// makes any re-append idempotent). Pure.
#[must_use]
pub fn backfill_minute_nanos(
    last_persisted: Option<i64>,
    target_minute_nanos: i64,
    session_first_minute_nanos: i64,
) -> Option<i64> {
    let prev = target_minute_nanos.saturating_sub(NANOS_PER_MINUTE);
    (prev >= session_first_minute_nanos && last_persisted.is_none_or(|l| l < prev)).then_some(prev)
}

/// Post-session sweep instant, IST seconds-of-day: 15:33:30 — after the
/// last (15:30:00) fire, once the whole session is final. The single
/// bounded sweep re-fetches the day window ONCE per SID and backfills
/// every session minute above the persisted watermark that is still
/// missing (M1, review 2026-07-13: the 15:30:00 fire is the LAST, so a
/// vendor-late 15:29 candle had no repair path).
///
/// 429-coordination (2026-07-13, same-day follow-up): moved from 15:31:00
/// to 15:33:30 so the sweep's ≤4 requests land AFTER the 15:31 bulk
/// cross-verify's observed 429 burst window (live session 2026-07-13:
/// 91/776 cross-verify fetches failed HTTP 429 between 15:31 and 15:33).
/// The const-assert below pins the sweep strictly clear of that window.
const SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST: u32 = 15 * 3600 + 33 * 60 + 30;

// The sweep must clear the cross-verify burst: at least 150 s after the
// 15:31:00 cross-verify trigger (burst observed through 15:33), and still
// comfortably before the 16:30 IST box stop.
const _: () = assert!(
    SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST
        >= crate::cross_verify_1m_boot::CROSS_VERIFY_TRIGGER_SECS_OF_DAY_IST + 150,
    "post-session sweep must clear the 15:31-15:33 cross-verify burst window"
);
const _: () = assert!(
    SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST < 16 * 3600 + 30 * 60,
    "post-session sweep must fire before the 16:30 IST box stop"
);

/// All session minutes STILL MISSING above the persisted watermark at
/// sweep time: `(watermark, session_last]` step one minute (the whole
/// session when nothing was ever persisted). Bounded to ≤375 minutes by
/// construction. Pure.
#[must_use]
pub fn sweep_missing_minutes(
    last_persisted: Option<i64>,
    session_first_minute_nanos: i64,
    session_last_minute_nanos: i64,
) -> Vec<i64> {
    let start = match last_persisted {
        Some(w) => w
            .saturating_add(NANOS_PER_MINUTE)
            .max(session_first_minute_nanos),
        None => session_first_minute_nanos,
    };
    let mut out = Vec::new();
    let mut m = start;
    while m <= session_last_minute_nanos {
        out.push(m);
        m = m.saturating_add(NANOS_PER_MINUTE);
    }
    out
}

/// Per-SID latest successfully PERSISTED minute (append + flush confirmed).
/// Drives the previous-minute backfill sweep; commits are max-merge so a
/// double persist of the same minute (DEDUP-idempotent server-side) never
/// regresses the watermark. In-memory only — a restart re-fills via the
/// backfill sweep's one-minute lookback.
#[derive(Debug, Default)]
pub struct PersistTracker {
    committed: HashMap<SecurityId, i64>,
}

impl PersistTracker {
    /// The latest persisted minute (IST nanos) for this SID, if any.
    #[must_use]
    pub fn last_persisted(&self, security_id: SecurityId) -> Option<i64> {
        self.committed.get(&security_id).copied()
    }

    /// Commit a persisted minute — max-merge (idempotent; never regresses).
    pub fn commit(&mut self, security_id: SecurityId, minute_nanos: i64) {
        let entry = self.committed.entry(security_id).or_insert(minute_nanos);
        if *entry < minute_nanos {
            *entry = minute_nanos;
        }
    }
}

/// Sleep DELTAS (ms) between ladder attempts, derived from the constant
/// offsets-from-first-attempt so the schedule stays a single source of
/// truth. Pure.
#[must_use]
pub fn retry_sleep_deltas_ms() -> [u64; 4] {
    let o = SPOT_1M_REST_RETRY_OFFSETS_MS;
    [
        o[0],
        o[1].saturating_sub(o[0]),
        o[2].saturating_sub(o[1]),
        o[3].saturating_sub(o[2]),
    ]
}

/// Deterministic per-SID ladder jitter (ms) for the SID at `slot` in the
/// pinned [`SPOT_1M_REST_INDICES`] array: `(slot % slots) × step` → 0 /
/// 150 / 300 / 450 ms for the 4 spot SIDs. The slot (a SID's fixed position in
/// the const array) is used instead of `sid % 3` because the pinned SIDs
/// 13/25/51 give `1/1/0` under `% 3` — two of three would still re-poll in
/// lockstep. NO randomness — deterministic + testable (429-coordination
/// follow-up 2026-07-13). Pure.
#[must_use]
pub fn ladder_jitter_ms(slot: usize) -> u64 {
    (slot as u64 % SPOT_1M_REST_LADDER_JITTER_SLOTS) * SPOT_1M_REST_LADDER_JITTER_STEP_MS
}

/// The sleep (ms) before one ladder re-poll: the schedule delta, plus the
/// per-SID jitter on the FIRST re-poll only (shifting the whole schedule —
/// later deltas are relative, so the shift carries through), plus the
/// bounded [`SPOT_1M_REST_429_EXTRA_BACKOFF_MS`] when the PREVIOUS attempt
/// was rate-limited (HTTP 429) so the next poll never lands straight back
/// inside Dhan's rate-limit window. Pure — worst-case totals are
/// const-asserted inside the hard per-SID budget in `constants.rs`.
#[must_use]
pub fn ladder_sleep_ms(
    base_delta_ms: u64,
    is_first_repoll: bool,
    jitter_ms: u64,
    prev_rate_limited: bool,
) -> u64 {
    base_delta_ms
        .saturating_add(if is_first_repoll { jitter_ms } else { 0 })
        .saturating_add(if prev_rate_limited {
            SPOT_1M_REST_429_EXTRA_BACKOFF_MS
        } else {
            0
        })
}

// ---------------------------------------------------------------------------
// 2026-07-14 retry shaping (operator pacing directive) — pure primitives
// ---------------------------------------------------------------------------

/// STALE-WATERMARK CUTOFF (2026-07-14): `true` when ladder attempt N
/// returned a parsed day payload whose last-candle watermark equals the
/// previous attempt's — re-polling cannot outrun a serving delay (the
/// #1524 serving-lag data: lags measured in minutes/hours, not the
/// ladder's 0.7–6s reach), so the ladder STOPS for the minute.
///
/// Encoding: `prev_parsed` is `None` until an attempt returns a parsed
/// 2xx; a parsed payload's watermark is `Some(newest_candle_nanos)` or
/// `None` for a zero-row day payload — two consecutive zero-row payloads
/// therefore ALSO repeat (today's `empty_no_rows` regime). A transport /
/// non-2xx attempt records NO observation (never compared).
///
/// Honest trade-off (stated in the PR body too): on a healthy-but-
/// slow-serving day this stops the ladder after two polls ~0.7s apart,
/// trading the 1.5–6s marginal-appearance window for quota; the
/// per-minute backfill + the 15:33:30 sweep remain the repair paths.
/// Pure.
#[must_use]
pub fn ladder_watermark_repeated(
    prev_parsed: Option<Option<i64>>,
    current_parsed: Option<i64>,
) -> bool {
    prev_parsed == Some(current_parsed)
}

/// ADAPTIVE DEGRADE (2026-07-14): the ladder attempt count for one SID's
/// fire — 1 (single attempt, no re-polls) while degraded, the full
/// schedule otherwise. Pure.
#[must_use]
pub fn ladder_attempt_count(degraded: bool) -> usize {
    if degraded {
        1
    } else {
        SPOT_1M_REST_RETRY_OFFSETS_MS.len() + 1
    }
}

/// What the run loop must do after feeding one fired minute's verdict to
/// the [`LadderDegrade`] tracker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DegradeAction {
    /// No transition.
    None,
    /// RISING edge: enter single-attempt-per-minute mode (one loud log).
    EnterDegraded { consecutive_no_data: u32 },
    /// FALLING edge: a real success re-armed the full ladder (one log).
    ExitDegraded { degraded_minutes: u32 },
}

/// ADAPTIVE DEGRADE tracker: after
/// [`SPOT_1M_REST_DEGRADE_AFTER_CONSECUTIVE_NO_DATA_MINUTES`] consecutive
/// no-data minutes (ZERO SIDs served their own just-closed candle —
/// empty AND transport/429 failures both count, matching today's mixed
/// 0/980 + 429 regime), the ladder drops to a single attempt per minute
/// until ANY success re-arms it. Session-scoped pure state machine (the
/// [`FailureEdge`] envelope — a task respawn restarts the streak; the
/// re-learning window is bounded and loud). Pure — unit-tested without a
/// clock.
#[derive(Debug, Default)]
pub struct LadderDegrade {
    consecutive_no_data: u32,
    degraded: bool,
    degraded_minutes: u32,
}

impl LadderDegrade {
    /// Whether the NEXT fire should run single-attempt.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// Record one fired minute's verdict (`any_sid_served` = at least one
    /// SID retrieved its own target-minute candle this fire).
    pub fn record_minute(&mut self, any_sid_served: bool) -> DegradeAction {
        if any_sid_served {
            let degraded_minutes = self.degraded_minutes;
            let was_degraded = self.degraded;
            self.consecutive_no_data = 0;
            self.degraded = false;
            self.degraded_minutes = 0;
            if was_degraded {
                return DegradeAction::ExitDegraded { degraded_minutes };
            }
            return DegradeAction::None;
        }
        self.consecutive_no_data = self.consecutive_no_data.saturating_add(1);
        if self.degraded {
            self.degraded_minutes = self.degraded_minutes.saturating_add(1);
            return DegradeAction::None;
        }
        if self.consecutive_no_data >= SPOT_1M_REST_DEGRADE_AFTER_CONSECUTIVE_NO_DATA_MINUTES {
            self.degraded = true;
            self.degraded_minutes = 0;
            return DegradeAction::EnterDegraded {
                consecutive_no_data: self.consecutive_no_data,
            };
        }
        DegradeAction::None
    }
}

// ---------------------------------------------------------------------------
// 2026-07-14 fetch-mode flag — batch catch-up scheduling (pure)
// ---------------------------------------------------------------------------

/// The next batch catch-up fire boundary at-or-after `now_secs_of_day`
/// and strictly after `last_fired`, on the K-minute grid anchored at the
/// first per-minute boundary: `FIRST + k × (interval × 60)`, clamped to
/// today's session window. `None` once the grid is past 15:30 IST (the
/// caller then runs the post-session sweep). `interval_minutes` is
/// boot-validated to 1..=60; a hostile 0 is treated as 1. Pure.
#[must_use]
pub fn next_batch_fire_after(
    now_secs_of_day: u32,
    last_fired: Option<u32>,
    interval_minutes: u32,
) -> Option<u32> {
    let step = interval_minutes.max(1).saturating_mul(60);
    let first = SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST;
    let last = SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST;
    let floor = match last_fired {
        Some(f) => now_secs_of_day.max(f.saturating_add(1)),
        None => now_secs_of_day,
    };
    if floor <= first {
        return Some(first);
    }
    // Smallest grid point >= floor.
    let k = (floor - first).div_ceil(step);
    let fire = first.saturating_add(k.saturating_mul(step));
    (fire <= last).then_some(fire)
}

/// Batch-cycle verdict for the [`FailureEdge`]: a cycle is FULLY FAILED
/// when the persist leg failed, or when minutes were due and NONE landed
/// (a fetch outage or an all-empty vendor regime — honest, per audit
/// Rule 11: an idle cycle with nothing due is OK, an empty-handed cycle
/// is not). Pure.
#[must_use]
pub fn batch_cycle_fully_failed(missing_before: u64, swept: u64, persist_failed: bool) -> bool {
    persist_failed || (missing_before > 0 && swept == 0)
}

/// IST 12-hour label for a seconds-of-day instant (Telegram commandment 9
/// — `"10:42 AM"`, never `"1042"` or ISO). Pure.
#[must_use]
pub fn format_minute_ist_12h(secs_of_day: u32) -> String {
    let h24 = (secs_of_day / 3600) % 24;
    let minute = (secs_of_day % 3600) / 60;
    let (h12, ampm) = match h24 {
        0 => (12, "AM"),
        1..=11 => (h24, "AM"),
        12 => (12, "PM"),
        _ => (h24 - 12, "PM"),
    };
    format!("{h12}:{minute:02} {ampm}")
}

// ---------------------------------------------------------------------------
// Pure edge tracker (audit Rule 4 — edge-triggered escalation)
// ---------------------------------------------------------------------------

/// What the caller must do after recording a minute's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeAction {
    /// Nothing to page (below the edge, or already paged this episode).
    None,
    /// RISING edge: page ONCE (High) — consecutive fully-failed minutes
    /// reached the threshold.
    Page { consecutive: u32 },
    /// FALLING edge: a successful minute ended a PAGED episode — one Info
    /// recovery ping.
    Recover { failed_minutes: u32 },
}

/// Consecutive fully-failed-minute tracker. Pages once per episode at
/// [`SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD`]; re-arms only after a
/// successful minute. Pure state machine — unit-tested without a clock.
#[derive(Debug, Default)]
pub struct FailureEdge {
    consecutive_failed: u32,
    paged: bool,
}

impl FailureEdge {
    /// Record one fired minute's verdict (`fully_failed` = no SID
    /// succeeded) and return the edge action.
    pub fn record_minute(&mut self, fully_failed: bool) -> EdgeAction {
        if fully_failed {
            self.consecutive_failed = self.consecutive_failed.saturating_add(1);
            if !self.paged
                && self.consecutive_failed >= SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD
            {
                self.paged = true;
                return EdgeAction::Page {
                    consecutive: self.consecutive_failed,
                };
            }
            EdgeAction::None
        } else {
            let failed_minutes = self.consecutive_failed;
            let was_paged = self.paged;
            self.consecutive_failed = 0;
            self.paged = false;
            if was_paged {
                EdgeAction::Recover { failed_minutes }
            } else {
                EdgeAction::None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure per-SID not-served detector (2026-07-13 — the INDIA VIX live-probe
// companion; operator scope addition 2026-07-13, relayed via the
// coordinator session)
// ---------------------------------------------------------------------------

/// What the caller must do for ONE SID after recording a minute's per-SID
/// served verdicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidEdgeAction {
    /// Nothing to page for this SID this minute.
    None,
    /// RISING edge: this SID reached
    /// [`SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD`] consecutive counted
    /// not-served minutes (each with ≥1 sibling success) — page ONCE
    /// (High), latched until this SID's own recovery.
    Page { consecutive: u32 },
    /// FALLING edge: a paged SID was served again — one Info ping; the
    /// latch re-arms.
    Recover { not_served_minutes: u32 },
}

/// Per-SID persistent-empty state: consecutive COUNTED not-served minutes
/// + the page latch.
#[derive(Debug, Default)]
struct SidServedState {
    consecutive_not_served: u32,
    paged: bool,
}

/// Per-SID "is the vendor serving this index?" tracker. Distinguishes
/// vendor-not-serving-ONE-index from a global outage:
///
/// | This minute, this SID | ≥1 OTHER SID served? | Effect on this SID |
/// |---|---|---|
/// | served (own-minute candle retrieved) | — | streak reset; `Recover` if paged |
/// | not served | yes | streak +1; `Page` once at the threshold |
/// | not served | no (global outage) | HOLD — neither counts nor resets |
///
/// The global-outage HOLD keeps the two signals disjoint: a full outage is
/// the [`FailureEdge`]'s page, and a mid-streak global blip can neither
/// inflate nor launder a genuine vendor-not-serving streak. Pure state
/// machine — unit-tested without a clock. State is per scheduler run
/// (session-scoped, same envelope as [`FailureEdge`] — a task respawn
/// restarts the streak).
#[derive(Debug, Default)]
pub struct SidServedTracker {
    per_sid: HashMap<SecurityId, SidServedState>,
}

impl SidServedTracker {
    /// Record one fired minute's per-SID served verdicts (`served` = the
    /// SID's OWN target-minute candle was retrieved this fire) and return
    /// one action per input SID, index-aligned with `verdicts`.
    pub fn record_minute(
        &mut self,
        verdicts: &[(SecurityId, bool)],
    ) -> Vec<(SecurityId, SidEdgeAction)> {
        let any_served = verdicts.iter().any(|&(_, served)| served);
        verdicts
            .iter()
            .map(|&(sid, served)| {
                let state = self.per_sid.entry(sid).or_default();
                let action = if served {
                    let not_served_minutes = state.consecutive_not_served;
                    let was_paged = state.paged;
                    state.consecutive_not_served = 0;
                    state.paged = false;
                    if was_paged {
                        SidEdgeAction::Recover { not_served_minutes }
                    } else {
                        SidEdgeAction::None
                    }
                } else if any_served {
                    state.consecutive_not_served = state.consecutive_not_served.saturating_add(1);
                    if !state.paged
                        && state.consecutive_not_served >= SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD
                    {
                        state.paged = true;
                        SidEdgeAction::Page {
                            consecutive: state.consecutive_not_served,
                        }
                    } else {
                        SidEdgeAction::None
                    }
                } else {
                    // Global-outage minute (no SID served): HOLD.
                    SidEdgeAction::None
                };
                (sid, action)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 2026-07-14 serving-delay diagnostics — one-shot side-by-side probes
// ---------------------------------------------------------------------------
//
// Config-gated (`[spot_1m_rest] diagnostics`), LOG-ONLY, bounded: two probe
// moments per day (the first session fire after boot + once at the
// configurable second instant, default 11:00 IST), each issuing up to 3
// extra requests for ONE SID spaced [`PROBE_REQUEST_SPACING_MS`] apart —
// ≤6 requests/day, trivially inside the Data-API 5/sec budget. The probes
// NEVER touch the persist / edge / backfill legs; their whole output is
// ONE structured side-by-side log line per probe moment. Shapes:
//   a. `crossverify_day_window` — the 15:31 cross-verify's byte-exact
//      day-granular request (== [`spot_1m_day_request_body`]; the two
//      builders share [`intraday_request_body`] — byte-equality is
//      unit-pinned below), issued AT the probe instant: does the shape
//      that provably works at 15:31 also work now?
//   b. `prev_trading_day_window` — the previous trading day's full window:
//      proves SETTLED-data serving is healthy right now.
//   c. `same_day_to_now_window` — same day but `toDate = now` instead of
//      D+1 00:00:00: does the toDate shape change same-day serving?
// Together with the per-minute empty-class split these discriminate
// "vendor serves same-day intraday with a DELAY" from "our request shape
// is wrong".

/// Sleep between the probe's sequential requests (budget spacing).
const PROBE_REQUEST_SPACING_MS: u64 = 300;
/// Minimum seconds remaining before the next minute boundary for a probe
/// to start: 3 requests × the 5 s client timeout + 2 × spacing ≈ 15.6 s
/// worst case, so 20 s of room guarantees the probe can never eat the next
/// boundary (and if it somehow did, the H2 missed-boundary accounting
/// still counts it — the probe runs BEFORE that accounting).
const PROBE_ROOM_SECS: u32 = 20;

/// Which one-shot probe moment fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeSlot {
    /// The first session fire after boot.
    FirstFire,
    /// The configurable second instant (default 11:00 IST).
    SecondScheduled,
}

/// One-shot latches for the two probe moments (session-scoped; a task
/// respawn re-arms them — worst case a few extra bounded probes, log-only).
#[derive(Debug, Default)]
pub struct ProbeState {
    pub first_done: bool,
    pub second_done: bool,
}

/// Which probe (if any) is due after the fire at `fire_secs_of_day`.
/// `None` while diagnostics are off or both one-shots already ran. Pure.
#[must_use]
pub fn should_run_probe(
    diagnostics_enabled: bool,
    state: &ProbeState,
    fire_secs_of_day: u32,
    second_probe_secs_of_day: u32,
) -> Option<ProbeSlot> {
    if !diagnostics_enabled {
        return None;
    }
    if !state.first_done {
        return Some(ProbeSlot::FirstFire);
    }
    if !state.second_done && fire_secs_of_day >= second_probe_secs_of_day {
        return Some(ProbeSlot::SecondScheduled);
    }
    None
}

/// `true` when the wall clock leaves at least [`PROBE_ROOM_SECS`] before
/// the next minute boundary — a due probe with no room DEFERS to the next
/// fire (the latch stays un-set), never risks eating a fetch minute. Pure.
#[must_use]
pub fn probe_has_room(now_secs_of_day: u32) -> bool {
    let next_boundary = (now_secs_of_day / 60).saturating_add(1).saturating_mul(60);
    next_boundary.saturating_sub(now_secs_of_day) >= PROBE_ROOM_SECS
}

/// The most recent trading day STRICTLY BEFORE `today` (bounded 14-day
/// walk — longer than any NSE holiday cluster). Predicate-injected so the
/// calendar never has to be mocked. Pure. `None` only on a pathological
/// 14-straight-closed-days calendar (the probe then simply skips shape b).
#[must_use]
pub fn previous_trading_day(
    today: NaiveDate,
    is_trading_day: impl Fn(NaiveDate) -> bool,
) -> Option<NaiveDate> {
    let mut d = today.pred_opt()?;
    for _ in 0..14 {
        if is_trading_day(d) {
            return Some(d);
        }
        d = d.pred_opt()?;
    }
    None
}

/// The `same_day_to_now_window` probe body: identical to the proven day
/// window EXCEPT `toDate` = the current IST wall-clock instant (instead of
/// D+1 00:00:00). Pure.
#[must_use]
pub fn probe_to_now_body(
    security_id: &str,
    trading_date: NaiveDate,
    now_secs_of_day: u32,
) -> serde_json::Value {
    let clamped = now_secs_of_day.min(SECONDS_PER_DAY - 1);
    let (h, m, s) = (clamped / 3600, (clamped % 3600) / 60, clamped % 60);
    serde_json::json!({
        "securityId": security_id,
        "exchangeSegment": SPOT_1M_REST_SEGMENT_IDX_I,
        "instrument": SPOT_1M_INSTRUMENT_INDEX,
        "interval": "1",
        "oi": false,
        "fromDate": trading_date.format("%Y-%m-%d 00:00:00").to_string(),
        "toDate": format!("{} {h:02}:{m:02}:{s:02}", trading_date.format("%Y-%m-%d")),
    })
}

/// One probe response's shape summary. `serving_lag_secs = -1` is the
/// honest "no candles, nothing to measure" sentinel. Pure construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeBodySummary {
    pub rows: usize,
    pub first_candle_ist_nanos: Option<i64>,
    pub last_candle_ist_nanos: Option<i64>,
    pub target_present: bool,
    pub serving_lag_secs: i64,
}

/// Summarize a parsed probe body against the shape's target minute. Pure.
#[must_use]
pub fn summarize_probe_candles(
    candles: &[MinuteCandle],
    target_minute_ist_nanos: i64,
) -> ProbeBodySummary {
    let first = candles.iter().map(|c| c.minute_ts_ist_nanos).min();
    let last = candles.iter().map(|c| c.minute_ts_ist_nanos).max();
    ProbeBodySummary {
        rows: candles.len(),
        first_candle_ist_nanos: first,
        last_candle_ist_nanos: last,
        target_present: select_minute_candle(candles, target_minute_ist_nanos).is_some(),
        serving_lag_secs: last.map_or(-1, |l| serving_lag_secs(target_minute_ist_nanos, l)),
    }
}

/// Run one probe moment: up to 3 sequential bounded requests, ONE
/// structured side-by-side log line. Returns `true` when the probe
/// actually issued requests (the caller latches the one-shot); a
/// no-token moment defers. LOG-ONLY — never touches writer/tracker/edge.
// TEST-EXEMPT: live-deps async runner — the gating (should_run_probe / probe_has_room), body builders (spot_1m_day_request_body byte-equality / probe_to_now_body), previous_trading_day walk and summarize_probe_candles are pure fns unit-tested below; the HTTP leg reuses the tested spot_1m_fetch_once.
async fn run_serving_delay_probe(
    params: &Spot1mRestTaskParams,
    client: &reqwest::Client,
    url: &str,
    slot: ProbeSlot,
    fire_secs_of_day: u32,
) -> bool {
    let jwt: Option<secrecy::SecretString> = {
        let guard = params.token_handle.load();
        guard
            .as_ref()
            .as_ref()
            .map(|state| state.access_token().clone())
    };
    let Some(jwt) = jwt else {
        warn!(
            slot = ?slot,
            "spot_1m_rest diagnostics: no access token at probe time — \
             deferring the one-shot probe to the next minute"
        );
        return false;
    };
    let trading_date = today_ist();
    let (probe_sid, probe_symbol) = SPOT_1M_REST_INDICES[0];
    let sid_str = probe_sid.to_string();
    let target_minute_open_secs = fire_secs_of_day.saturating_sub(60);
    let today_target_nanos = minute_open_ist_nanos(trading_date, target_minute_open_secs);
    let today_base_nanos = minute_open_ist_nanos(trading_date, 0);

    // (name, body, shape's own day base for labels, shape's target minute)
    let mut shapes: Vec<(&'static str, serde_json::Value, i64, i64)> = vec![(
        "crossverify_day_window",
        spot_1m_day_request_body(&sid_str, trading_date),
        today_base_nanos,
        today_target_nanos,
    )];
    if let Some(prev) = previous_trading_day(trading_date, |d| params.calendar.is_trading_day(d)) {
        // Settled-data shape: the target is the PREVIOUS day's last
        // session minute (15:29 open) — "did the whole settled day serve?"
        shapes.push((
            "prev_trading_day_window",
            spot_1m_day_request_body(&sid_str, prev),
            minute_open_ist_nanos(prev, 0),
            minute_open_ist_nanos(prev, SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST - 60),
        ));
    }
    shapes.push((
        "same_day_to_now_window",
        probe_to_now_body(&sid_str, trading_date, ist_secs_of_day_now()),
        today_base_nanos,
        today_target_nanos,
    ));

    let mut summaries: Vec<serde_json::Value> = Vec::with_capacity(shapes.len());
    for (i, (name, body, base_nanos, target_nanos)) in shapes.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(PROBE_REQUEST_SPACING_MS)).await;
        }
        let entry = match spot_1m_fetch_once(client, url, jwt.expose_secret(), body).await {
            Ok(fetched) => {
                let candles = parse_intraday_1m_candles(&fetched.text);
                let s = summarize_probe_candles(&candles, *target_nanos);
                serde_json::json!({
                    "probe": name,
                    "request": body.to_string(),
                    "status": "2xx",
                    // 2026-07-14 raw-body discriminator (diagnostics-gated
                    // by construction — probes only run under
                    // `[spot_1m_rest] diagnostics_enabled`): bounded
                    // 600-char secret-redacted body sample + shape stats.
                    "content_type": fetched.content_type,
                    "body_bytes": fetched.text.len(),
                    "body_sample": tickvault_common::sanitize::capture_rest_raw_body_sample(
                        &fetched.text,
                    ),
                    "rows": s.rows,
                    "first_candle_ist": s
                        .first_candle_ist_nanos
                        .map(|n| ist_nanos_minute_label(n, *base_nanos)),
                    "last_candle_ist": s
                        .last_candle_ist_nanos
                        .map(|n| ist_nanos_minute_label(n, *base_nanos)),
                    "target_present": s.target_present,
                    "serving_lag_secs": s.serving_lag_secs,
                })
            }
            Err(failure) => {
                if failure.rate_limited {
                    metrics::counter!("tv_spot1m_rate_limited_total").increment(1);
                }
                // `failure.msg` is already status + token-redacted URL +
                // ≤300-char secret-redacted body (spot_1m_fetch_once).
                serde_json::json!({
                    "probe": name,
                    "request": body.to_string(),
                    "status": failure.msg,
                })
            }
        };
        summaries.push(entry);
    }
    // ONE structured line: every shape's request body + response shape,
    // side by side — tomorrow's session reads this single line.
    info!(
        slot = ?slot,
        probe_sid,
        probe_symbol,
        target_minute = %format_minute_ist_12h(target_minute_open_secs),
        probes = %serde_json::Value::Array(summaries),
        "spot_1m_rest diagnostics: one-shot serving-delay probe — \
         cross-verify byte-exact day window vs previous-trading-day window \
         vs same-day-to-now window (log-only; bounded; never touches persist)"
    );
    true
}

// ---------------------------------------------------------------------------
// Wall-clock helpers (IST)
// ---------------------------------------------------------------------------

/// IST seconds-of-day from the wall clock (the rest_canary helper).
fn ist_secs_of_day_now() -> u32 {
    let now_ist = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // rem_euclid of a positive modulus is < SECONDS_PER_DAY; the cast is safe.
    now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32
}

/// IST milliseconds-of-day from the wall clock (close→data latency math).
fn ist_millis_of_day_now() -> i64 {
    let now_ist_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS) * MILLIS_PER_SEC);
    now_ist_ms.rem_euclid(MILLIS_PER_DAY)
}

/// IST calendar date for "now" (the orphan-watchdog helper).
fn today_ist() -> NaiveDate {
    let utc = DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0).unwrap_or_default();
    (utc + ChronoDuration::seconds(i64::from(IST_UTC_OFFSET_SECONDS))).date_naive()
}

/// Retrieval wall-clock instant as IST nanoseconds (`Utc::now()` source ⇒
/// ADD the IST offset per `data-integrity.md`).
fn fetched_at_ist_nanos_now() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS).saturating_mul(NANOS_PER_SEC))
}

// ---------------------------------------------------------------------------
// Fetch ladder
// ---------------------------------------------------------------------------

/// One index's per-minute fetch verdict after the bounded ladder. Every
/// arm additionally carries the PREVIOUS-minute backfill candle when the
/// backfill was due and any 2xx body of the ladder contained it — a fire
/// whose OWN minute failed can still repair the previous minute (the
/// vendor-lateness recovery path).
#[derive(Clone, Debug, PartialEq)]
enum SidFetchOutcome {
    /// The target minute's candle was retrieved (`close_to_data_ms` =
    /// minute close → retrieval wall-clock latency).
    Found {
        candle: MinuteCandle,
        close_to_data_ms: i64,
        backfill_candle: Option<MinuteCandle>,
    },
    /// Every attempt got a parseable 2xx but the target minute never
    /// appeared — counted `outcome="empty_no_rows"` / `outcome="empty_stale"`
    /// per the [`EmptyClass`] split (2026-07-14 serving-delay
    /// instrumentation), included in the failure edge either way.
    Empty {
        class: EmptyClass,
        backfill_candle: Option<MinuteCandle>,
    },
    /// The last attempt (transport / non-2xx) failure, bounded + redacted.
    Failed {
        reason: String,
        backfill_candle: Option<MinuteCandle>,
    },
}

/// One attempt's typed failure — `rate_limited` is derived from the REAL
/// `StatusCode` (429), never a substring scan of the message (2026-07-12
/// review LOW).
#[derive(Clone, Debug, PartialEq)]
struct FetchFailure {
    rate_limited: bool,
    msg: String,
}

/// Per-ladder forensics for the Dhan `rest_fetch_audit` row (GAP-11
/// review HIGH, 2026-07-14): the REAL rung count + 429 count + whether
/// the TERMINAL failure was an HTTP 429 — so a Dhan 429 storm can never
/// again read 0 on the scoreboard digest while
/// `tv_spot1m_rate_limited_total` climbs. `final_http_status` /
/// `fetch_latency_ms` REMAIN the storage crate's 0/-1 named sentinels
/// (the Dhan [`FetchFailure`] carries no status/latency fields — that
/// residual is the remaining flagged follow-up).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DhanLadderForensics {
    /// Requests actually sent by the ladder (0 = the budget-overrun arm —
    /// the timed-out ladder's partial state is dropped with its future,
    /// the Groww `budget_exceeded` honesty note).
    attempts: u32,
    /// How many of those attempts were HTTP 429 (derived from the REAL
    /// `StatusCode`, never a substring scan).
    rate_limited_count: u32,
    /// The FINAL attempt was an HTTP 429 — drives the terminal
    /// [`RestFetchOutcome::RateLimited`] classification (mirrors the
    /// Groww `error_class == "rate_limited"` rule: the LAST attempt's
    /// status decides).
    terminal_rate_limited: bool,
}

/// GAP-11 review HIGH (2026-07-14): terminal-failure classification for
/// the Dhan audit row — mirrors the Groww `audit_outcome_for` rule
/// (`RateLimited` keys on the LAST attempt being an HTTP 429). Pure.
fn dhan_failed_audit_class(forensics: &DhanLadderForensics) -> (RestFetchOutcome, &'static str) {
    if forensics.terminal_rate_limited {
        (RestFetchOutcome::RateLimited, "rate_limited")
    } else {
        (RestFetchOutcome::Error, "error")
    }
}

/// A 2xx response body + its `Content-Type` header value (2026-07-14
/// raw-body discriminator: the header names WHAT Dhan is serving when the
/// body carries zero candles — JSON envelope vs HTML shell vs empty).
struct FetchedBody {
    text: String,
    content_type: String,
}

// ---------------------------------------------------------------------------
// 2026-07-14 once-per-day raw-body sample (the account-condition vs
// envelope-drift discriminator + Dhan-support evidence). For ≥14 days BOTH
// /v2/charts/intraday and /v2/charts/historical returned 2xx with zero
// parseable candles for ALL SIDs while option-chain + WS worked — and no
// 2xx body was ever logged anywhere. On the FIRST empty_no_rows/empty_stale
// classification of each IST day, ONE structured line captures a bounded
// (600-char, secret-redacted) sample of the last parsed 2xx body + its
// total byte length + Content-Type. Edge-latched: exactly one line per
// day per process, unconditional (not diagnostics-gated — it is one log
// line per day).
// ---------------------------------------------------------------------------

/// Process-global latch: the IST day key (nanos / day) whose raw-body
/// sample has already been captured. 0 = never.
static RAW_BODY_CAPTURE_DAY_KEY: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

/// Nanoseconds per day — the day-key divisor for the once-per-day latch.
const NANOS_PER_DAY_I64: i64 = 86_400 * 1_000_000_000;

/// Pure: the once-per-day latch key for a minute inside the trading day.
#[must_use]
pub fn raw_body_capture_day_key(minute_ist_nanos: i64) -> i64 {
    minute_ist_nanos.div_euclid(NANOS_PER_DAY_I64)
}

/// Pure: whether a capture is still due for `day_key` given the latched
/// value (`0` = never captured).
#[must_use]
pub fn raw_body_capture_due(latched_day_key: i64, day_key: i64) -> bool {
    latched_day_key != day_key
}

/// Claim today's capture slot. `true` exactly once per day key per
/// process (CAS — the 4 concurrent per-SID ladders can never double-log).
fn try_claim_raw_body_capture(day_key: i64) -> bool {
    use std::sync::atomic::Ordering;
    let prev = RAW_BODY_CAPTURE_DAY_KEY.load(Ordering::Relaxed);
    raw_body_capture_due(prev, day_key)
        && RAW_BODY_CAPTURE_DAY_KEY
            .compare_exchange(prev, day_key, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// `true` when a DECLARED `Content-Length` fits the body cap (an absent
/// declaration passes — the streamed accumulator below still enforces the
/// cap). Pure (2026-07-12 security-review M — unbounded body read).
#[must_use]
pub fn declared_len_within_cap(declared_len: Option<u64>, cap_bytes: usize) -> bool {
    declared_len.is_none_or(|len| len <= cap_bytes as u64)
}

/// `true` when accumulating `chunk_len` more bytes onto `buffered_len`
/// stays within the body cap. Pure.
#[must_use]
pub fn accumulation_within_cap(buffered_len: usize, chunk_len: usize, cap_bytes: usize) -> bool {
    buffered_len.saturating_add(chunk_len) <= cap_bytes
}

/// Read a response body with the [`SPOT_1M_REST_MAX_BODY_BYTES`] cap
/// enforced BOTH on the declared `Content-Length` and on the streamed
/// accumulation (the csv_downloader §18 body-cap pattern) — a
/// misbehaving/hostile server can never buffer unbounded bytes here.
async fn read_body_capped(mut resp: reqwest::Response) -> Result<String, String> {
    if !declared_len_within_cap(resp.content_length(), SPOT_1M_REST_MAX_BODY_BYTES) {
        return Err(format!(
            "body too large: declared {} bytes > cap {SPOT_1M_REST_MAX_BODY_BYTES}",
            resp.content_length().unwrap_or_default()
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("read: {e}"))? {
        if !accumulation_within_cap(buf.len(), chunk.len(), SPOT_1M_REST_MAX_BODY_BYTES) {
            return Err(format!(
                "body exceeded cap {SPOT_1M_REST_MAX_BODY_BYTES} bytes mid-stream"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| "body not valid UTF-8".to_string())
}

/// One intraday REST round-trip → the raw 2xx body text (parsed by the
/// caller via [`parse_intraday_columnar_for_minute`] — a malformed body
/// parses to no candles and rides the ladder like an empty one). `Err`
/// carries status + token-redacted URL + ≤300-char secret-redacted body
/// (the DHAN-REST-400 capture discipline). Bodies (success AND error) are
/// read through the streamed cap.
async fn spot_1m_fetch_once(
    client: &reqwest::Client,
    url: &str,
    jwt: &str,
    body: &serde_json::Value,
) -> Result<FetchedBody, FetchFailure> {
    // 2026-07-14 operator pacing directive: EVERY spot-1m Data-API request
    // (per-minute fires, ladder re-polls, the 15:33:30 sweep, the #1524
    // diagnostic probes — they all funnel through this fn) waits for a
    // permit from the shared process-wide limiter; overflow spills into
    // the next second(s), never drops.
    crate::dhan_data_api_limiter::shared_dhan_data_api_limiter()
        .acquire()
        .await;
    let resp = client
        .post(url)
        .header("access-token", jwt)
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| FetchFailure {
            rate_limited: false,
            msg: format!("send: {}", redact_url_params(&e.to_string())),
        })?;
    let status = resp.status();
    if !status.is_success() {
        let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        if rate_limited {
            // Feed the self-tuner from the REAL StatusCode (never a
            // substring scan) — enough of these inside the rolling window
            // steps the shared limiter down to the 2 rps floor.
            crate::dhan_data_api_limiter::shared_dhan_data_api_limiter().record_429();
        }
        let error_body = read_body_capped(resp).await.unwrap_or_default();
        return Err(FetchFailure {
            rate_limited,
            msg: format!(
                "http {status} url={} body={}",
                redact_url_params(url),
                capture_rest_error_body(&error_body)
            ),
        });
    }
    // Content-Type BEFORE the body read consumes the response (2026-07-14
    // raw-body discriminator — the header value rides the once-per-day
    // sample line + the diagnostics probe entries).
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let text = read_body_capped(resp).await.map_err(|msg| FetchFailure {
        rate_limited: false,
        msg,
    })?;
    Ok(FetchedBody { text, content_type })
}

/// Bounded in-minute re-poll ladder for ONE index: first attempt at the
/// fire instant, then re-polls at [`SPOT_1M_REST_RETRY_OFFSETS_MS`] (the
/// whole schedule shifted by the deterministic per-SID `jitter_ms` so the
/// 3 SIDs never re-poll in lockstep) until the target minute's candle
/// appears — after the last offset the minute is `Empty`/`Failed`, never
/// an unbounded in-minute retry. DH-904/429 counts via the REAL
/// `StatusCode` AND adds the bounded extra backoff before the NEXT rung
/// ([`ladder_sleep_ms`]) — same rung count, never an extra retry
/// (429-coordination follow-up 2026-07-13). The WHOLE ladder is
/// additionally hard-bounded by [`SPOT_1M_REST_SID_BUDGET_SECS`] in
/// [`fetch_minute_bounded`] so no stall combination can overrun the
/// minute (2026-07-12 H2 fix).
#[allow(clippy::too_many_arguments)] // APPROVED: private ladder — a struct would be pure ceremony
async fn fetch_minute_with_ladder(
    client: &reqwest::Client,
    url: &str,
    jwt: &secrecy::SecretString,
    body: &serde_json::Value,
    target_minute_ist_nanos: i64,
    backfill_minute_ist_nanos: Option<i64>,
    minute_close_ms_of_day: i64,
    jitter_ms: u64,
    max_attempts: usize,
) -> (SidFetchOutcome, DhanLadderForensics) {
    let deltas = retry_sleep_deltas_ms();
    // 2026-07-14 adaptive degrade: 1 while degraded (no re-polls), the
    // full schedule otherwise — clamped so a hostile value can never
    // extend the ladder.
    let attempts = max_attempts.clamp(1, deltas.len() + 1);
    let mut last_error: Option<String> = None;
    let mut prev_rate_limited = false;
    // GAP-11 review HIGH (2026-07-14): real attempt/429 facts for the
    // forensics row (previously fabricated as attempts=1 / rate=0).
    let mut forensics = DhanLadderForensics::default();
    // Sticky: the FIRST 2xx body carrying the due backfill minute wins —
    // preserved across rungs and across a failing own-minute verdict.
    let mut backfill_found: Option<MinuteCandle> = None;
    // 2026-07-14 serving-delay instrumentation: the freshest NON-empty 2xx
    // body's `(rows, newest candle minute)` — classifies an Empty verdict
    // into `empty_no_rows` vs `empty_stale` + measures the serving lag.
    let mut freshest_body: Option<(usize, i64)> = None;
    // 2026-07-14 stale-watermark cutoff: the previous PARSED attempt's
    // last-candle watermark (`Some(None)` = a parsed zero-row payload).
    let mut prev_parsed_watermark: Option<Option<i64>> = None;
    // 2026-07-14 raw-body discriminator: the last target-less 2xx body's
    // bounded redacted sample + Content-Type + total byte length. Stashed
    // ONLY while today's once-per-day capture is still due (zero work on
    // every later minute of the day).
    let capture_day_key = raw_body_capture_day_key(target_minute_ist_nanos);
    let mut empty_body_sample: Option<(String, String, usize)> = None;
    for attempt in 0..attempts {
        if attempt > 0 {
            let sleep_ms = ladder_sleep_ms(
                deltas[attempt - 1],
                attempt == 1,
                jitter_ms,
                prev_rate_limited,
            );
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        }
        let started = std::time::Instant::now();
        let result = spot_1m_fetch_once(client, url, jwt.expose_secret(), body).await;
        metrics::histogram!("tv_spot1m_fetch_duration_ms")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        forensics.attempts = forensics.attempts.saturating_add(1);
        match result {
            Ok(fetched) => {
                let body_text = fetched.text;
                let (target, backfill, stats) = parse_intraday_columnar_for_minutes(
                    &body_text,
                    target_minute_ist_nanos,
                    backfill_minute_ist_nanos,
                );
                if stats.is_some() {
                    freshest_body = stats;
                }
                if backfill_found.is_none() {
                    backfill_found = backfill;
                }
                if let Some(candle) = target {
                    let close_to_data_ms =
                        (ist_millis_of_day_now() - minute_close_ms_of_day).max(0);
                    return (
                        SidFetchOutcome::Found {
                            candle,
                            close_to_data_ms,
                            backfill_candle: backfill_found,
                        },
                        forensics,
                    );
                }
                // 2xx without the target minute — stash the bounded
                // redacted sample for the once-per-day raw-body capture
                // (only while today's slot is unclaimed; cold path).
                if raw_body_capture_due(
                    RAW_BODY_CAPTURE_DAY_KEY.load(std::sync::atomic::Ordering::Relaxed),
                    capture_day_key,
                ) {
                    empty_body_sample = Some((
                        tickvault_common::sanitize::capture_rest_raw_body_sample(&body_text),
                        fetched.content_type,
                        body_text.len(),
                    ));
                }
                // The seal may not have landed yet; the next ladder rung
                // re-polls UNLESS the watermark provably did not move
                // between two polls
                // (stale-watermark cutoff, 2026-07-14 — re-polling cannot
                // outrun a serving delay; the saved rungs were today's
                // wasted-429 fuel).
                last_error = None;
                prev_rate_limited = false;
                forensics.terminal_rate_limited = false;
                let current_watermark = stats.map(|(_, newest)| newest);
                if ladder_watermark_repeated(prev_parsed_watermark, current_watermark) {
                    metrics::counter!("tv_spot1m_ladder_watermark_cutoff_total").increment(1);
                    break;
                }
                prev_parsed_watermark = Some(current_watermark);
            }
            Err(failure) => {
                if failure.rate_limited {
                    // DH-904 class: counted; the NEXT rung waits the extra
                    // bounded backoff; NEVER retried past the ladder.
                    metrics::counter!("tv_spot1m_rate_limited_total").increment(1);
                    forensics.rate_limited_count = forensics.rate_limited_count.saturating_add(1);
                }
                prev_rate_limited = failure.rate_limited;
                forensics.terminal_rate_limited = failure.rate_limited;
                last_error = Some(failure.msg);
            }
        }
    }
    let outcome = match last_error {
        Some(reason) => SidFetchOutcome::Failed {
            reason,
            backfill_candle: backfill_found,
        },
        None => {
            let class = classify_empty_body(freshest_body, target_minute_ist_nanos);
            // 2026-07-14 once-per-day raw-body capture: the FIRST
            // empty_no_rows/empty_stale classification of the IST day logs
            // ONE bounded structured line — the account-condition vs
            // envelope-drift discriminator (Dhan-support evidence). CAS
            // claim: the concurrent per-SID ladders can never double-log.
            if let Some((sample, content_type, body_bytes)) = empty_body_sample
                && try_claim_raw_body_capture(capture_day_key)
            {
                error!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage = "raw_body_sample",
                    class = ?class,
                    body_bytes,
                    content_type = %content_type,
                    body_sample = %sample,
                    "spot_1m_rest: FIRST empty classification of the day — bounded \
                     600-char secret-redacted sample of the 2xx charts body (the \
                     account-condition vs envelope-drift discriminator; once per day)"
                );
            }
            SidFetchOutcome::Empty {
                class,
                backfill_candle: backfill_found,
            }
        }
    };
    (outcome, forensics)
}

/// The ladder wrapped in the HARD per-SID wall-clock budget
/// ([`SPOT_1M_REST_SID_BUDGET_SECS`]): a budget overrun is that SID's
/// failure for the minute — the fire can never overrun the next boundary
/// (the const-asserts in `constants.rs` pin the budget < 60 s including
/// the per-request timeout).
#[allow(clippy::too_many_arguments)] // APPROVED: private budget wrapper over the ladder — same rationale
async fn fetch_minute_bounded(
    client: &reqwest::Client,
    url: &str,
    jwt: &secrecy::SecretString,
    body: &serde_json::Value,
    target_minute_ist_nanos: i64,
    backfill_minute_ist_nanos: Option<i64>,
    minute_close_ms_of_day: i64,
    jitter_ms: u64,
    max_attempts: usize,
) -> (SidFetchOutcome, DhanLadderForensics) {
    match tokio::time::timeout(
        Duration::from_secs(SPOT_1M_REST_SID_BUDGET_SECS),
        fetch_minute_with_ladder(
            client,
            url,
            jwt,
            body,
            target_minute_ist_nanos,
            backfill_minute_ist_nanos,
            minute_close_ms_of_day,
            jitter_ms,
            max_attempts,
        ),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            metrics::counter!("tv_spot1m_sid_budget_exceeded_total").increment(1);
            // Honest sentinels (the Groww budget-overrun note): the
            // timed-out ladder's partial forensics are dropped with its
            // future — attempts reads 0, never a fabricated count.
            (
                SidFetchOutcome::Failed {
                    reason: format!(
                        "ladder budget exceeded ({SPOT_1M_REST_SID_BUDGET_SECS}s) — peer stalling"
                    ),
                    backfill_candle: None,
                },
                DhanLadderForensics::default(),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduler run + supervisor
// ---------------------------------------------------------------------------

/// Run today's remaining minute-close fires, then return. Never panics;
/// every fault path logs (coded) + counts and continues to the next minute.
// TEST-EXEMPT: live-deps async runner — every scheduling / parsing / edge decision is a pure fn unit-tested below; the HTTP leg mirrors the tested cross_verify/prev_day pattern; wiring pinned by crates/app/tests/spot_1m_rest_wiring_guard.rs.
pub async fn run_spot_1m_rest(params: Spot1mRestTaskParams) {
    // Idempotent DDL first (CREATE → ADD COLUMN self-heal → DEDUP ENABLE);
    // failures degrade loudly inside (SPOT1M-02) and never block the run.
    ensure_spot_1m_rest_table(&params.questdb).await;
    // Gap-11 forensics: the shared rest_fetch_audit table (idempotent —
    // the Groww leg also ensures it; a second call is harmless).
    ensure_rest_fetch_audit_table(&params.questdb).await;

    if !params.calendar.is_trading_day_today() {
        info!("spot_1m_rest: non-trading day — skipping all minute fires");
        return;
    }
    let url = join_api_url(&params.rest_api_base_url, DHAN_CHARTS_INTRADAY_PATH);
    // ONE long-lived client for the whole session (per-minute rebuild is
    // the exact TLS/resolver churn HTTP-CLIENT-01 §0 condemns). A build
    // failure degrades loudly and returns — the supervisor retries after
    // its bounded backoff; NEVER a `Client::new()` panic fallback.
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(SPOT_1M_REST_REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "client_build",
                ?err,
                "SPOT1M-01: HTTP client build failed — per-minute spot fetch \
                 degraded; supervisor will retry after backoff"
            );
            metrics::counter!("tv_spot1m_fetch_total", "outcome" => "error").increment(1);
            return;
        }
    };
    let mut writer = Spot1mRestWriter::new(&params.questdb);
    // Gap-11 forensics: best-effort per-fetch audit rows (never on the
    // fetch/verdict/edge path).
    let mut audit_writer = RestFetchAuditWriter::new(&params.questdb);
    // 2026-07-13 backfill sweep: per-SID latest PERSISTED minute — drives
    // the previous-minute repair on every fire (and the batch catch-up
    // ceiling in batch mode).
    let mut tracker = PersistTracker::default();

    // 2026-07-14 fetch-mode flag (`[spot_1m_rest] fetch_mode`): batch
    // catch-up replaces the per-minute fires with a sweep-style cycle
    // every `batch_interval_minutes` — a thin mode wrapper over the SAME
    // sweep machinery the 15:33:30 post-session sweep uses. Default stays
    // per_minute pending the ~15:40 IST sweep-discriminator ruling.
    if params.fetch_mode == SpotFetchMode::BatchCatchup {
        let session_complete =
            run_batch_catchup_loop(&params, &client, &url, &mut writer, &mut tracker).await;
        if session_complete {
            run_post_session_sweep(
                &client,
                &url,
                &params,
                &mut writer,
                &mut audit_writer,
                &mut tracker,
            )
            .await;
        }
        return;
    }

    let mut edge = FailureEdge::default();
    // 2026-07-13 VIX companion: per-SID vendor-not-serving detector.
    let mut sid_tracker = SidServedTracker::default();
    // 2026-07-14 adaptive degrade: consecutive no-data minutes drop the
    // ladder to a single attempt per minute until ANY success re-arms it.
    let mut degrade = LadderDegrade::default();
    // H1 (2026-07-12): the last boundary actually HANDLED (fired or
    // skipped-stale) — the next fire is always STRICTLY after it, so a
    // fast-completing fire can never re-fire the same boundary second.
    let mut last_fired: Option<u32> = None;
    // 2026-07-14 serving-delay diagnostics: one-shot probe latches.
    let mut probe_state = ProbeState::default();
    info!(
        indices = SPOT_1M_REST_INDICES.len(),
        "spot_1m_rest: per-minute fetch loop armed (fires each minute close \
         09:16:00–15:30:00 IST, ~0.3–1.3s after the boundary; every request \
         paced by the shared Dhan Data-API limiter)"
    );

    loop {
        // Audit Rule 3: re-read the wall clock + trading-day verdict EVERY
        // iteration (a suspend can cross midnight and stale the verdict).
        if !params.calendar.is_trading_day_today() {
            // 2026-07-14: loud + coded (was a bare info!) — a mid-session
            // calendar flip silently stopping a capture leg must be
            // greppable in errors.jsonl. Log-sink-only, NO Telegram (a
            // calendar flip is not broker failure); a suspend that
            // crossed IST midnight is a legitimate cause.
            metrics::counter!("tv_spot1m_trading_day_flip_exit_total").increment(1);
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "trading_day_flip_exit",
                "SPOT1M-01: the trading-day verdict flipped mid-session — \
                 exiting today's spot fire loop (a suspend that crossed \
                 IST midnight is a legitimate cause; remaining minutes \
                 stay absent, re-fetchable via backfill)"
            );
            return;
        }
        // Groww Item-7 precedent (GAP-11 review MEDIUM 1): the trading
        // date as of THIS iteration's start — a suspend across IST
        // midnight makes the post-wake `today_ist()` a DIFFERENT day, and
        // stamping pre-suspend session seconds onto it would file the
        // boundary-skip forensics rows under the wrong trading date.
        let iter_date = today_ist();
        let now = ist_secs_of_day_now();
        let Some(fire) = next_fire_after(now, last_fired) else {
            info!(
                "spot_1m_rest: past 15:30 IST — today's minute fires complete; \
                 running the one bounded post-session sweep"
            );
            run_post_session_sweep(
                &client,
                &url,
                &params,
                &mut writer,
                &mut audit_writer,
                &mut tracker,
            )
            .await;
            return;
        };
        let sleep_ms =
            u64::from(fire.saturating_sub(now)).saturating_mul(1_000) + SPOT_1M_REST_FIRE_DELAY_MS;
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;

        // Staleness gate: a suspend / clock step can wake us far past the
        // boundary (or on the next day). Skip + recompute, never fetch a
        // long-gone minute as if it just closed. Every boundary that
        // elapsed while asleep is COUNTED as missed (H2 — never silent).
        let woke = ist_secs_of_day_now();
        if !fire_is_fresh(fire, woke) {
            warn!(
                fire_secs = fire,
                woke_at_secs = woke,
                "spot_1m_rest: woke too far past the minute boundary \
                 (suspend/clock step?) — skipping this minute"
            );
            // Boundaries in (fire-60, woke) are gone (a midnight-wrap wake
            // counts 0 — the trading-day gate exits next iteration).
            let missed = count_missed_boundaries(fire.saturating_sub(60), woke.saturating_sub(1));
            record_skipped_boundaries(
                &params,
                &mut edge,
                &mut audit_writer,
                missed,
                fire,
                iter_date,
            );
            // Advance past everything that already elapsed so the next
            // iteration never re-selects a long-gone boundary.
            if woke > fire {
                last_fired = Some((woke.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
            }
            continue;
        }

        let ok_count = fire_one_minute(
            &params,
            &client,
            &url,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut sid_tracker,
            &mut tracker,
            fire,
            degrade.is_degraded(),
        )
        .await;
        // 2026-07-14 adaptive degrade: edge-triggered transitions only
        // (one loud log per direction, never per minute).
        match degrade.record_minute(ok_count > 0) {
            DegradeAction::EnterDegraded {
                consecutive_no_data,
            } => {
                metrics::gauge!("tv_spot1m_ladder_degraded").set(1.0);
                warn!(
                    consecutive_no_data,
                    "spot_1m_rest: adaptive degrade ENTERED — consecutive \
                     no-data minutes; dropping to a single attempt per \
                     minute (no ladder re-polls) until ANY success re-arms \
                     the full ladder (2026-07-14 retry shaping — re-polling \
                     an all-empty vendor regime was today's wasted-429 fuel)"
                );
            }
            DegradeAction::ExitDegraded { degraded_minutes } => {
                metrics::gauge!("tv_spot1m_ladder_degraded").set(0.0);
                info!(
                    degraded_minutes,
                    "spot_1m_rest: adaptive degrade EXITED — a SID served \
                     again; full ladder re-armed"
                );
            }
            DegradeAction::None => {}
        }
        // PR-3 sequencing: tell the option-chain leg this minute's spot
        // fire is DONE (success or failure — the chain must never block on
        // a failing spot leg). `send_replace` never errors.
        if let Some(tx) = &params.minute_done_tx {
            tx.send_replace(Some(fire));
        }
        // 2026-07-14 serving-delay diagnostics: one-shot log-only probes,
        // AFTER the chain sequencing signal (never delays the chain leg)
        // and BEFORE the H2 overrun accounting (a pathological probe
        // overrun is still counted as a missed boundary, never silent).
        // A due probe with no room / no token DEFERS to the next fire.
        if let Some(slot) = should_run_probe(
            params.diagnostics_enabled,
            &probe_state,
            fire,
            params.diagnostics_second_probe_secs_of_day_ist,
        ) && probe_has_room(ist_secs_of_day_now())
            && run_serving_delay_probe(&params, &client, &url, slot, fire).await
        {
            match slot {
                ProbeSlot::FirstFire => probe_state.first_done = true,
                ProbeSlot::SecondScheduled => probe_state.second_done = true,
            }
        }
        last_fired = Some(fire);
        // H2 overrun accounting: boundaries that fully elapsed DURING the
        // fire can never be fetched — count them loudly + feed the edge.
        let after = ist_secs_of_day_now();
        let missed = count_missed_boundaries(fire, after.saturating_sub(1));
        record_skipped_boundaries(
            &params,
            &mut edge,
            &mut audit_writer,
            missed,
            fire + 60,
            iter_date,
        );
        if missed > 0 {
            last_fired = Some((after.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
        }
    }
}

/// Loud accounting for minute boundaries that elapsed UNFETCHED (fire
/// overrun / suspend / clock step): counter + ONE coalesced coded log +
/// each missed minute feeds the failure edge so a sustained-overrun outage
/// still reaches the SPOT1M-01 escalation page (2026-07-12 H2 fix) + one
/// `outcome=skipped` forensics row per (missed minute, SID) so the hole is
/// queryable (GAP-11 review MEDIUM 1, 2026-07-14 — the Groww spot / Dhan
/// chain shape; never silent, Rule 11). `first_missed_boundary_secs` = the
/// FIRST boundary in the missed run (each targets the minute opening 60 s
/// before it).
fn record_skipped_boundaries(
    params: &Spot1mRestTaskParams,
    edge: &mut FailureEdge,
    audit_writer: &mut RestFetchAuditWriter,
    skipped: u32,
    first_missed_boundary_secs: u32,
    iter_date: NaiveDate,
) {
    if skipped == 0 {
        return;
    }
    metrics::counter!("tv_spot1m_boundary_skipped_total").increment(u64::from(skipped));
    let around = format_minute_ist_12h(first_missed_boundary_secs);
    error!(
        code = ErrorCode::Spot1m01FetchDegraded.code_str(),
        stage = "boundary_skipped",
        skipped,
        around = %around,
        "SPOT1M-01: minute boundaries elapsed unfetched (fire overrun / \
         suspend) — those minutes stay absent (re-fetchable via backfill)"
    );
    // Groww Item-7 midnight-cross guard: a wake that crossed IST midnight
    // would stamp the missed PRE-SUSPEND session seconds onto the
    // POST-WAKE date — wrong trading date on every row. The counter + the
    // coalesced log above already fired; skip the (mis-dated) forensics
    // rows AND the per-boundary edge accounting. DELIBERATE edge-accounting
    // consequence (review MEDIUM 3 class): the trading day is over for this
    // run — an escalation page for yesterday's tail would be noise.
    let trading_date = today_ist();
    if trading_date != iter_date {
        warn!(
            %iter_date,
            %trading_date,
            "spot_1m_rest: wake crossed IST midnight — skipping the \
             boundary-skip forensics rows (they would carry the wrong \
             trading date); the day is over for this run"
        );
        return;
    }
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
    for i in 0..skipped {
        let boundary = first_missed_boundary_secs.saturating_add(i.saturating_mul(60));
        let minute_open_secs = boundary.saturating_sub(60);
        let target_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
        for (security_id, symbol) in SPOT_1M_REST_INDICES {
            audit_append_best_effort(
                audit_writer,
                &build_dhan_fetch_audit_row(
                    target_nanos,
                    trading_date_nanos,
                    security_id,
                    symbol,
                    0,
                    0,
                    RestFetchOutcome::Skipped,
                    -1,
                    "boundary_skipped",
                ),
            );
        }
        if let EdgeAction::Page { consecutive } = edge.record_minute(true) {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "escalation",
                consecutive,
                minute = %around,
                "SPOT1M-01: per-minute spot fetch fully failed for consecutive \
                 minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::Spot1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: around.clone(),
                });
        }
    }
    audit_flush_best_effort(audit_writer);
}

/// Build one `spot_1m_rest` row from a parsed candle. The `close_to_data_ms`
/// stamp is the caller's HONEST retrieval delay (own-fire latency, or the
/// > 60 s real delay for a backfilled minute).
fn build_spot_1m_row(
    candle: &MinuteCandle,
    security_id: SecurityId,
    symbol: &'static str,
    trading_date_nanos: i64,
    close_to_data_ms: i64,
) -> Spot1mRestRow {
    Spot1mRestRow {
        ts_ist_nanos: candle.minute_ts_ist_nanos,
        trading_date_ist_nanos: trading_date_nanos,
        // Unreachable for the pinned 13/25/51 set; a hypothetical overflow
        // is LOUD + a visible sentinel, never a silent sid=0 (review LOW).
        security_id: i64::try_from(security_id).unwrap_or_else(|_| {
            error!(
                code = ErrorCode::Spot1m02PersistFailed.code_str(),
                stage = "sid_overflow",
                security_id,
                "SPOT1M-02: security_id exceeds i64 — row \
                 stamped with the i64::MAX sentinel"
            );
            i64::MAX
        }),
        symbol,
        open: candle.open,
        high: candle.high,
        low: candle.low,
        close: candle.close,
        volume: candle.volume,
        close_to_data_ms,
        fetched_at_ist_nanos: fetched_at_ist_nanos_now(),
    }
}

/// Best-effort symbol lookup for a staged `(security_id, minute)` pair —
/// the pinned [`SPOT_1M_REST_INDICES`] set is 4 entries (cold path).
fn spot_1m_symbol_for_sid(security_id: SecurityId) -> &'static str {
    SPOT_1M_REST_INDICES
        .into_iter()
        .find(|(sid, _)| *sid == security_id)
        .map_or("unknown", |(_, symbol)| symbol)
}

/// Pure Dhan-side `rest_fetch_audit` row builder — the mirror of the Groww
/// `build_fetch_audit_row`. Since the GAP-11 review HIGH fix (2026-07-14)
/// `attempts` and `rate_limited_count` are the ladder's REAL counts
/// ([`DhanLadderForensics`]); `final_http_status` / `fetch_latency_ms`
/// REMAIN the storage crate's documented sentinels (`0` = no captured
/// status, `-1` = not measured — the Dhan [`FetchFailure`] carries no
/// status/latency fields, the remaining flagged follow-up).
///
/// DEDUP note (review LOW, 2026-07-14): `error_class` is NOT in the audit
/// DEDUP key `(ts, trading_date_ist, feed, leg, security_id,
/// exchange_segment, outcome)` — two rows sharing that key with DIFFERENT
/// error_class values (e.g. a fire's `persist_failed` NamedGap then the
/// sweep's `named_gap` for the same minute) UPSERT in place, so the
/// LAST-written error_class wins for that key. The outcome-level truth is
/// unaffected (`outcome` IS in-key — transition rows with distinct
/// outcomes both survive).
#[allow(clippy::too_many_arguments)] // APPROVED: private forensics builder — a struct would be pure ceremony
fn build_dhan_fetch_audit_row(
    target_minute_ist_nanos: i64,
    trading_date_nanos: i64,
    security_id: SecurityId,
    symbol: &'static str,
    attempts: i64,
    rate_limited_count: i64,
    outcome: RestFetchOutcome,
    close_to_data_ms: i64,
    error_class: &'static str,
) -> RestFetchAuditRow {
    RestFetchAuditRow {
        close_to_persist_ms: -1,
        ts_ist_nanos: target_minute_ist_nanos,
        trading_date_ist_nanos: trading_date_nanos,
        feed: SPOT_1M_REST_FEED_DHAN,
        leg: REST_FETCH_LEG_SPOT_1M,
        // Unreachable overflow for the pinned SID set — a visible
        // sentinel, never a silent sid=0 (the build_spot_1m_row precedent).
        security_id: i64::try_from(security_id).unwrap_or(i64::MAX),
        exchange_segment: SPOT_1M_REST_SEGMENT_IDX_I,
        symbol,
        attempts,
        final_http_status: 0,
        fetch_latency_ms: -1,
        close_to_data_ms,
        rate_limited_count,
        outcome,
        error_class,
    }
}

/// Best-effort forensics append: a failure logs (coded) + counts and
/// RETURNS — the fetch loop, the verdict and the failure edge are never
/// affected by the forensics leg. Dhan emit sites stay field-less on the
/// SPOT1M codes per the rule-file convention (grep-split by
/// `feed="groww"`).
fn audit_append_best_effort(audit_writer: &mut RestFetchAuditWriter, row: &RestFetchAuditRow) {
    if let Err(err) = audit_writer.append_row(row) {
        metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_append")
            .increment(1);
        error!(
            code = ErrorCode::Spot1m02PersistFailed.code_str(),
            stage = "audit_append",
            ?err,
            "SPOT1M-02: rest_fetch_audit row append failed (forensics only — \
             the fetch loop is unaffected)"
        );
    }
}

/// Best-effort forensics flush (same never-affects-the-loop contract).
fn audit_flush_best_effort(audit_writer: &mut RestFetchAuditWriter) {
    if let Err(err) = audit_writer.flush() {
        metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_flush")
            .increment(1);
        error!(
            code = ErrorCode::Spot1m02PersistFailed.code_str(),
            stage = "audit_flush",
            ?err,
            "SPOT1M-02: rest_fetch_audit ILP flush failed — pending forensics \
             rows discarded (best-effort; the fetch loop is unaffected)"
        );
    }
}

/// GAP-11 persist stamping: minute CLOSE (row minute open + 60 s) → the
/// data-table ILP flush-ACK instant, in ms of the IST day. Clamped at 0
/// (clock jitter can never fabricate a negative latency). The row's
/// `ts_ist_nanos` is its minute-OPEN in IST nanos — the same math the
/// sweep's `close_to_data_ms` uses. Pure.
///
/// IST-midnight wrap (review LOW, 2026-07-14): a `now_ms_of_day` that
/// wrapped past midnight reads BELOW the close instant and the stamp
/// clamps to 0 — unreachable on the prod schedule (last fire 15:30,
/// sweep ~15:33:30, box auto-stops 16:30 IST); documented, not special-cased.
#[must_use]
pub fn close_to_persist_ms_for(
    minute_ts_ist_nanos: i64,
    trading_date_nanos: i64,
    now_ms_of_day: i64,
) -> i64 {
    let close_ms_of_day =
        ((minute_ts_ist_nanos.saturating_sub(trading_date_nanos)) / NANOS_PER_SEC + 60)
            .saturating_mul(MILLIS_PER_SEC);
    (now_ms_of_day - close_ms_of_day).max(0)
}

/// GAP-11 hold-then-stamp: `ok`-class forensics rows are HELD until the
/// DATA-table ILP flush ACK. On flush Ok every held row is stamped with
/// its real `close_to_persist_ms` and returned for the best-effort audit
/// append; on flush Err the held rows are DISCARDED — a false `ok` row
/// must never land (the `flush_failed` named-gap rows emitted from the
/// staged set are the truth for those minutes; `outcome` is in the audit
/// DEDUP key, so an ok row would otherwise survive ALONGSIDE them and
/// lie about the ok path). Pure — the hold/stamp/discard decision in one
/// unit-testable place.
#[must_use]
pub fn stamp_held_ok_rows(
    mut held: Vec<RestFetchAuditRow>,
    flush_ok: bool,
    trading_date_nanos: i64,
    now_ms_of_day: i64,
) -> Vec<RestFetchAuditRow> {
    if !flush_ok {
        return Vec::new();
    }
    for row in &mut held {
        row.close_to_persist_ms =
            close_to_persist_ms_for(row.ts_ist_nanos, trading_date_nanos, now_ms_of_day);
    }
    held
}

/// One minute-close fire: 4 concurrent ladder fetches (each carrying the
/// full-day proven window) → persist the target minute AND the
/// previous-minute backfill when due → counters → edge accounting.
/// Failures are coalesced to ONE coded log per fire. Edge honesty: the
/// verdict is the OWN target minute's fetch + persist — a backfill hit
/// never counts as this fire's `ok` (a minute that lands only via
/// next-fire backfill was still that fire's failure).
#[allow(clippy::too_many_arguments)] // APPROVED: private fire sink over the run loop's owned state — a struct would be pure ceremony
async fn fire_one_minute(
    params: &Spot1mRestTaskParams,
    client: &reqwest::Client,
    url: &str,
    writer: &mut Spot1mRestWriter,
    audit_writer: &mut RestFetchAuditWriter,
    edge: &mut FailureEdge,
    sid_tracker: &mut SidServedTracker,
    tracker: &mut PersistTracker,
    fire_secs_of_day: u32,
    degraded: bool,
) -> usize {
    let minute_open_secs = fire_secs_of_day.saturating_sub(60);
    let minute_label = format_minute_ist_12h(minute_open_secs);
    let trading_date = today_ist();
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);

    // Zeroize-on-drop JWT copy, re-loaded EVERY fire (the JWT rotates
    // mid-session); never logged.
    let jwt: Option<secrecy::SecretString> = {
        let guard = params.token_handle.load();
        guard
            .as_ref()
            .as_ref()
            .map(|state| state.access_token().clone())
    };

    let mut ok_count: usize = 0;
    let mut empty_count: usize = 0;
    let mut error_count: usize = 0;
    // 2026-07-14 serving-delay instrumentation: log-only per-fire aggregate
    // of the empty-class split (never read by the edge / persist legs).
    let mut empty_diag = EmptyDiagnostics::default();
    // M1 (2026-07-12): a minute is fully-OK for the edge ONLY when the
    // fetch succeeded AND append+flush confirmed — a day-long QuestDB
    // outage must eventually page via the SAME Spot1mFetchDegraded path.
    let mut persist_failed = false;
    let mut sample_failure: Option<String> = None;
    // Minutes appended this fire, committed to the tracker ONLY after the
    // flush confirms (a failed flush discards the buffer — never a false
    // watermark advance). The ladder forensics ride along so a flush
    // failure's named-gap rows keep the REAL attempt/429 facts (GAP-11
    // review HIGH, 2026-07-14).
    let mut staged: Vec<(SecurityId, i64, DhanLadderForensics)> = Vec::new();
    // GAP-11 persist stamping: `ok` forensics rows are HELD here until the
    // data flush ACK, then stamped with the real close_to_persist_ms (a
    // failed flush discards them — the flush_failed rows are the truth).
    let mut held_ok_rows: Vec<RestFetchAuditRow> = Vec::new();
    // 2026-07-13 VIX companion: per-SID served verdicts for THIS minute
    // (served = the SID's OWN target-minute candle was retrieved). A
    // join-failed task leaves its SID unrecorded (HOLD); the no-token arm
    // records nothing (a global miss — the detector deliberately never
    // counts it).
    let mut sid_verdicts: Vec<(SecurityId, &'static str, bool)> = Vec::new();

    if let Some(jwt) = jwt {
        let target_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
        let session_first_nanos =
            minute_open_ist_nanos(trading_date, SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST - 60);
        let minute_close_ms = i64::from(fire_secs_of_day).saturating_mul(MILLIS_PER_SEC);

        let mut join_set = tokio::task::JoinSet::new();
        for (slot, (security_id, symbol)) in SPOT_1M_REST_INDICES.into_iter().enumerate() {
            let client = client.clone();
            let url = url.to_string();
            let jwt = jwt.clone();
            // 2026-07-13 hotfix: the PROVEN full-day window (cross-verify /
            // prev-day shape) — the consumer filters to the exact minute.
            let body = spot_1m_day_request_body(&security_id.to_string(), trading_date);
            let backfill_nanos = backfill_minute_nanos(
                tracker.last_persisted(security_id),
                target_nanos,
                session_first_nanos,
            );
            // 429-coordination (2026-07-13): deterministic per-SID schedule
            // shift — KEPT under the 2026-07-14 shared limiter as a
            // harmless schedule de-sync (the limiter is now the actual
            // pacing authority; removing the jitter would churn the
            // const-asserts for zero behaviour gain).
            let jitter_ms = ladder_jitter_ms(slot);
            // 2026-07-14 adaptive degrade: single attempt per minute in
            // the no-data regime, full ladder otherwise.
            let max_attempts = ladder_attempt_count(degraded);
            join_set.spawn(async move {
                let (outcome, forensics) = fetch_minute_bounded(
                    &client,
                    &url,
                    &jwt,
                    &body,
                    target_nanos,
                    backfill_nanos,
                    minute_close_ms,
                    jitter_ms,
                    max_attempts,
                )
                .await;
                (security_id, symbol, outcome, forensics)
            });
        }
        while let Some(joined) = join_set.join_next().await {
            let Ok((security_id, symbol, outcome, forensics)) = joined else {
                error_count = error_count.saturating_add(1);
                metrics::counter!("tv_spot1m_fetch_total", "outcome" => "error").increment(1);
                if sample_failure.is_none() {
                    sample_failure = Some("fetch task join failed".to_string());
                }
                continue;
            };
            // Previous-minute backfill (any outcome arm): persist with the
            // HONEST real retrieval delay (> 60 s by construction). Never
            // counted as this fire's `ok` and never sampled into the
            // own-fire `tv_spot1m_close_to_data_ms` histogram.
            // Gap-11 forensics: the non-ok verdict/class for this
            // (minute, SID), captured by the match arms below (the ok /
            // persist verdict is only knowable after the append).
            let mut audit_outcome = RestFetchOutcome::Error;
            let mut audit_error_class: &'static str = "error";
            let (own_outcome, backfill_candle) = match outcome {
                SidFetchOutcome::Found {
                    candle,
                    close_to_data_ms,
                    backfill_candle,
                } => (Some((candle, close_to_data_ms)), backfill_candle),
                SidFetchOutcome::Empty {
                    class,
                    backfill_candle,
                } => {
                    empty_count = empty_count.saturating_add(1);
                    empty_diag.record(class);
                    audit_outcome = RestFetchOutcome::Empty;
                    match class {
                        // 2026-07-14 split: static label values only.
                        EmptyClass::NoRows => {
                            audit_error_class = "empty_no_rows";
                            metrics::counter!("tv_spot1m_fetch_total", "outcome" => "empty_no_rows")
                                .increment(1);
                            if sample_failure.is_none() {
                                sample_failure = Some(format!(
                                    "sid {security_id}: 2xx with ZERO candles for the \
                                     whole day (empty_no_rows)"
                                ));
                            }
                        }
                        EmptyClass::Stale {
                            rows_in_response,
                            last_candle_ist_nanos,
                            serving_lag_secs,
                        } => {
                            audit_error_class = "empty_stale";
                            metrics::counter!("tv_spot1m_fetch_total", "outcome" => "empty_stale")
                                .increment(1);
                            // The vendor SERVING LAG, measured every stale
                            // minute — the per-minute discriminator for the
                            // "Dhan serves same-day candles with a delay"
                            // hypothesis. Dedicated 1s→6h buckets bound via
                            // Matcher::Full in observability.rs.
                            #[allow(clippy::cast_precision_loss)]
                            // APPROVED: histogram sample only
                            metrics::histogram!("tv_spot1m_serving_lag_ms")
                                .record(serving_lag_secs.saturating_mul(1_000) as f64);
                            if sample_failure.is_none() {
                                let last_label = ist_nanos_minute_label(
                                    last_candle_ist_nanos,
                                    trading_date_nanos,
                                );
                                sample_failure = Some(format!(
                                    "sid {security_id}: 2xx STALE — rows={rows_in_response}, \
                                     last_candle={last_label} IST, \
                                     serving_lag={serving_lag_secs}s (empty_stale)"
                                ));
                            }
                        }
                    }
                    (None, backfill_candle)
                }
                SidFetchOutcome::Failed {
                    reason,
                    backfill_candle,
                } => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_spot1m_fetch_total", "outcome" => "error").increment(1);
                    // GAP-11 review HIGH (2026-07-14): a terminal-429
                    // ladder classifies `rate_limited`, never a generic
                    // `error` — the scoreboard digest sums these rows'
                    // rate-limit facts (the Groww classification rule).
                    (audit_outcome, audit_error_class) = dhan_failed_audit_class(&forensics);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!("sid {security_id}: {reason}"));
                    }
                    (None, backfill_candle)
                }
            };
            sid_verdicts.push((security_id, symbol, own_outcome.is_some()));
            // Gap-11 forensics: non-ok verdicts (empty_no_rows /
            // empty_stale / error) emit their audit row here — one per
            // (minute, SID). The ok verdict emits after its append below.
            if own_outcome.is_none() {
                audit_append_best_effort(
                    audit_writer,
                    &build_dhan_fetch_audit_row(
                        target_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        i64::from(forensics.attempts),
                        i64::from(forensics.rate_limited_count),
                        audit_outcome,
                        -1,
                        audit_error_class,
                    ),
                );
            }
            if let Some((candle, close_to_data_ms)) = own_outcome {
                ok_count = ok_count.saturating_add(1);
                metrics::counter!("tv_spot1m_fetch_total", "outcome" => "ok").increment(1);
                #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
                metrics::histogram!("tv_spot1m_close_to_data_ms").record(close_to_data_ms as f64);
                let row = build_spot_1m_row(
                    &candle,
                    security_id,
                    symbol,
                    trading_date_nanos,
                    close_to_data_ms,
                );
                if let Err(err) = writer.append_row(&row) {
                    persist_failed = true;
                    metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "append")
                        .increment(1);
                    error!(
                        code = ErrorCode::Spot1m02PersistFailed.code_str(),
                        stage = "append",
                        security_id,
                        ?err,
                        "SPOT1M-02: spot_1m_rest row append failed"
                    );
                    if sample_failure.is_none() {
                        sample_failure = Some(format!("persist append failed: {err:#}"));
                    }
                    // Gap-11 forensics: fetched-but-lost — never dressed
                    // as vendor absence (round-2 LOW precedent).
                    audit_append_best_effort(
                        audit_writer,
                        &build_dhan_fetch_audit_row(
                            candle.minute_ts_ist_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            i64::from(forensics.attempts),
                            i64::from(forensics.rate_limited_count),
                            RestFetchOutcome::NamedGap,
                            -1,
                            "persist_failed",
                        ),
                    );
                } else {
                    // GAP-11: HOLD the ok row until the data flush ACK —
                    // close_to_persist_ms is stamped post-flush, and a
                    // failed flush discards it (never a false ok row).
                    held_ok_rows.push(build_dhan_fetch_audit_row(
                        candle.minute_ts_ist_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        i64::from(forensics.attempts),
                        i64::from(forensics.rate_limited_count),
                        RestFetchOutcome::Ok,
                        close_to_data_ms,
                        "none",
                    ));
                    staged.push((security_id, candle.minute_ts_ist_nanos, forensics));
                }
            }
            if let Some(backfill) = backfill_candle {
                // Honest delay: the backfilled minute closed 60 s before
                // this fire's target minute close.
                let backfill_close_to_data_ms =
                    (ist_millis_of_day_now() - (minute_close_ms - 60 * MILLIS_PER_SEC)).max(0);
                let row = build_spot_1m_row(
                    &backfill,
                    security_id,
                    symbol,
                    trading_date_nanos,
                    backfill_close_to_data_ms,
                );
                if let Err(err) = writer.append_row(&row) {
                    persist_failed = true;
                    metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "append")
                        .increment(1);
                    error!(
                        code = ErrorCode::Spot1m02PersistFailed.code_str(),
                        stage = "append",
                        security_id,
                        ?err,
                        "SPOT1M-02: spot_1m_rest BACKFILL row append failed"
                    );
                    audit_append_best_effort(
                        audit_writer,
                        &build_dhan_fetch_audit_row(
                            backfill.minute_ts_ist_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            i64::from(forensics.attempts),
                            i64::from(forensics.rate_limited_count),
                            RestFetchOutcome::NamedGap,
                            -1,
                            "persist_failed",
                        ),
                    );
                } else {
                    metrics::counter!("tv_spot1m_backfilled_total").increment(1);
                    info!(
                        security_id,
                        symbol,
                        backfill_close_to_data_ms,
                        "spot_1m_rest: previous minute backfilled from this \
                         fire's full-day response (DEDUP-idempotent)"
                    );
                    // Gap-11 forensics: late recovery — `ok` keyed on the
                    // BACKFILLED minute with the honest > 60 s delay.
                    // GAP-11: held until the data flush ACK like the
                    // own-minute ok row above.
                    held_ok_rows.push(build_dhan_fetch_audit_row(
                        backfill.minute_ts_ist_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        i64::from(forensics.attempts),
                        i64::from(forensics.rate_limited_count),
                        RestFetchOutcome::Ok,
                        backfill_close_to_data_ms,
                        "none",
                    ));
                    staged.push((security_id, backfill.minute_ts_ist_nanos, forensics));
                }
            }
        }
        let flush_result = writer.flush();
        if let Err(err) = &flush_result {
            persist_failed = true;
            metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "flush").increment(1);
            error!(
                code = ErrorCode::Spot1m02PersistFailed.code_str(),
                stage = "flush",
                ?err,
                "SPOT1M-02: spot_1m_rest ILP flush failed — pending rows \
                 discarded (poisoned-buffer defense; minutes stay absent and \
                 re-fetchable via DEDUP-idempotent backfill)"
            );
            if sample_failure.is_none() {
                sample_failure = Some(format!("persist flush failed: {err:#}"));
            }
            // Gap-11 forensics: the staged-but-lost minutes become
            // flush_failed rows — identity captured from `staged` BEFORE
            // the commit arm would consume it (spec caveat). The earlier
            // `ok` rows survive alongside (outcome is in the DEDUP key);
            // the staged ladder forensics keep the real 429 facts.
            for (security_id, minute_nanos, forensics) in &staged {
                audit_append_best_effort(
                    audit_writer,
                    &build_dhan_fetch_audit_row(
                        *minute_nanos,
                        trading_date_nanos,
                        *security_id,
                        spot_1m_symbol_for_sid(*security_id),
                        i64::from(forensics.attempts),
                        i64::from(forensics.rate_limited_count),
                        RestFetchOutcome::NamedGap,
                        -1,
                        "flush_failed",
                    ),
                );
            }
        } else {
            // Flush confirmed — advance the per-SID persisted watermark.
            for (security_id, minute_nanos, _forensics) in staged {
                tracker.commit(security_id, minute_nanos);
            }
        }
        // GAP-11: the ok rows land ONLY after (and stamped with) the data
        // flush ACK — discarded on a failed flush (the flush_failed rows
        // above are the truth for those minutes).
        for row in stamp_held_ok_rows(
            held_ok_rows,
            flush_result.is_ok(),
            trading_date_nanos,
            ist_millis_of_day_now(),
        ) {
            audit_append_best_effort(audit_writer, &row);
        }
    } else {
        // No token at fire time — REST cannot succeed; the whole minute is
        // a full miss (counted per SID for honest rate math).
        error_count = SPOT_1M_REST_INDICES.len();
        sample_failure = Some("no access token available at fire time".to_string());
        let target_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
        for (security_id, symbol) in SPOT_1M_REST_INDICES {
            metrics::counter!("tv_spot1m_fetch_total", "outcome" => "error").increment(1);
            // Gap-11 forensics: no request ran — 0 attempts / -1 sentinels.
            audit_append_best_effort(
                audit_writer,
                &build_dhan_fetch_audit_row(
                    target_nanos,
                    trading_date_nanos,
                    security_id,
                    symbol,
                    0,
                    0,
                    RestFetchOutcome::NoToken,
                    -1,
                    "no_token",
                ),
            );
        }
    }
    // Gap-11 forensics: flush ONCE per fire (best-effort).
    audit_flush_best_effort(audit_writer);

    record_minute_verdict(
        params,
        edge,
        &minute_label,
        ok_count,
        error_count,
        empty_count,
        persist_failed,
        sample_failure.as_deref(),
        &empty_diag,
        trading_date_nanos,
    );
    record_sid_served_verdicts(params, sid_tracker, &sid_verdicts, &minute_label);
    // 2026-07-14 adaptive degrade: the caller feeds this into the
    // LadderDegrade tracker (any SID serving its own candle re-arms the
    // full ladder).
    ok_count
}

/// M1 (2026-07-12): the edge's "fully failed" verdict for one fired
/// minute — no SID succeeded, OR the persist leg (append/flush) failed.
/// A fetched-but-never-persisted minute is NOT ok. Pure.
#[must_use]
pub fn minute_fully_failed(ok_count: usize, persist_failed: bool) -> bool {
    ok_count == 0 || persist_failed
}

/// One sweep cycle's outcome (shared by the post-session sweep and the
/// 2026-07-14 batch catch-up mode).
#[derive(Clone, Copy, Debug, Default)]
struct SweepCycleStats {
    /// Session minutes above the watermarks that were DUE at cycle start.
    missing_before: u64,
    /// Minutes found + staged + flush-confirmed this cycle.
    swept: u64,
    /// Minutes still absent after the cycle.
    still_missing: u64,
    /// Any append/flush failure this cycle (poisoned-buffer discard).
    persist_failed: bool,
}

/// The SHARED per-SID sweep body (extracted 2026-07-14 so the batch
/// catch-up mode is a thin wrapper, not new fetch logic): for every
/// pinned SID with session minutes still missing above its persisted
/// watermark (bounded to `up_to_last_minute`), fetch the proven day
/// window ONCE — through the shared Dhan Data-API limiter, inside
/// `spot_1m_fetch_once` — and persist every missing minute found.
/// Watermarks advance ONLY after the flush confirms (poisoned-buffer
/// defense; DEDUP-idempotent re-appends).
///
/// Gap-11 forensics (2026-07-14): when `audit` is `Some` (the TERMINAL
/// 15:33:30 post-session sweep) every still-missing minute lands a NAMED
/// gap row (`named_gap` / `persist_failed` / `flush_failed`) and every
/// recovered minute an `ok` row HELD until the data flush ACK then
/// stamped with its real `close_to_persist_ms`. Mid-session batch
/// catch-up cycles pass `None` — a minute missing at one cycle may be
/// recovered by the next, so stamping a terminal `named_gap` there would
/// be a false verdict (the per-minute fires' own rows + the terminal
/// sweep remain the forensics truth for a batch-mode day).
#[allow(clippy::too_many_arguments)] // APPROVED: private shared sweep body — a struct would be pure ceremony
// TEST-EXEMPT: live-deps async body — the missing-minute selection (sweep_missing_minutes), row/window builders and cycle verdict (batch_cycle_fully_failed) are pure fns unit-tested below; the HTTP+persist legs reuse the tested fire_one_minute pattern.
async fn sweep_sids_above_watermark(
    client: &reqwest::Client,
    url: &str,
    writer: &mut Spot1mRestWriter,
    tracker: &mut PersistTracker,
    jwt: &secrecy::SecretString,
    trading_date: NaiveDate,
    trading_date_nanos: i64,
    session_first: i64,
    up_to_last_minute: i64,
    stage: &'static str,
    mut audit: Option<&mut RestFetchAuditWriter>,
) -> SweepCycleStats {
    let mut stats = SweepCycleStats::default();
    let mut staged: Vec<(SecurityId, i64)> = Vec::new();
    // GAP-11 persist stamping: swept `ok` forensics rows are HELD until
    // the data flush ACK (stamped then; discarded on a failed flush).
    let mut held_ok_rows: Vec<RestFetchAuditRow> = Vec::new();
    for (security_id, symbol) in SPOT_1M_REST_INDICES {
        let missing = sweep_missing_minutes(
            tracker.last_persisted(security_id),
            session_first,
            up_to_last_minute,
        );
        if missing.is_empty() {
            continue;
        }
        stats.missing_before = stats.missing_before.saturating_add(missing.len() as u64);
        let body = spot_1m_day_request_body(&security_id.to_string(), trading_date);
        let candles = match spot_1m_fetch_once(client, url, jwt.expose_secret(), &body).await {
            Ok(fetched) => parse_intraday_1m_candles(&fetched.text),
            Err(failure) => {
                if failure.rate_limited {
                    metrics::counter!("tv_spot1m_rate_limited_total").increment(1);
                }
                error!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage,
                    security_id,
                    reason = %failure.msg,
                    "SPOT1M-01: sweep fetch failed for this SID"
                );
                stats.still_missing = stats.still_missing.saturating_add(missing.len() as u64);
                // Gap-11 forensics: the whole-SID sweep fetch failed —
                // every still-missing minute is a NAMED gap, never silent
                // (a 429'd sweep fetch keeps its real rate-limit fact —
                // GAP-11 review HIGH, 2026-07-14).
                if let Some(w) = audit.as_deref_mut() {
                    for minute_nanos in &missing {
                        audit_append_best_effort(
                            w,
                            &build_dhan_fetch_audit_row(
                                *minute_nanos,
                                trading_date_nanos,
                                security_id,
                                symbol,
                                1,
                                i64::from(failure.rate_limited),
                                RestFetchOutcome::NamedGap,
                                -1,
                                "named_gap",
                            ),
                        );
                    }
                }
                continue;
            }
        };
        let mut found_for_sid: u64 = 0;
        for minute_nanos in &missing {
            let Some(candle) = select_minute_candle(&candles, *minute_nanos) else {
                stats.still_missing = stats.still_missing.saturating_add(1);
                // Gap-11 forensics: still absent after the sweep — a
                // NAMED gap row for this exact (minute, SID).
                if let Some(w) = audit.as_deref_mut() {
                    audit_append_best_effort(
                        w,
                        &build_dhan_fetch_audit_row(
                            *minute_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            0,
                            RestFetchOutcome::NamedGap,
                            -1,
                            "named_gap",
                        ),
                    );
                }
                continue;
            };
            // Honest real retrieval delay for the swept minute.
            let close_ms_of_day = ((minute_nanos - trading_date_nanos) / NANOS_PER_SEC + 60)
                .saturating_mul(MILLIS_PER_SEC);
            let close_to_data_ms = (ist_millis_of_day_now() - close_ms_of_day).max(0);
            let row = build_spot_1m_row(
                &candle,
                security_id,
                symbol,
                trading_date_nanos,
                close_to_data_ms,
            );
            if let Err(err) = writer.append_row(&row) {
                stats.persist_failed = true;
                metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "append")
                    .increment(1);
                error!(
                    code = ErrorCode::Spot1m02PersistFailed.code_str(),
                    stage = "append",
                    security_id,
                    ?err,
                    "SPOT1M-02: sweep row append failed"
                );
                stats.still_missing = stats.still_missing.saturating_add(1);
                if let Some(w) = audit.as_deref_mut() {
                    audit_append_best_effort(
                        w,
                        &build_dhan_fetch_audit_row(
                            *minute_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            0,
                            RestFetchOutcome::NamedGap,
                            -1,
                            "persist_failed",
                        ),
                    );
                }
            } else {
                found_for_sid = found_for_sid.saturating_add(1);
                // Gap-11 forensics: sweep recovery — `ok` with the honest
                // real (big) close_to_data_ms. HELD until the data flush
                // ACK (GAP-11 persist stamping).
                if audit.is_some() {
                    held_ok_rows.push(build_dhan_fetch_audit_row(
                        *minute_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        0,
                        RestFetchOutcome::Ok,
                        close_to_data_ms,
                        "none",
                    ));
                }
                staged.push((security_id, *minute_nanos));
            }
        }
        stats.swept = stats.swept.saturating_add(found_for_sid);
    }
    let flush_result = writer.flush();
    if let Err(err) = &flush_result {
        stats.persist_failed = true;
        metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "flush").increment(1);
        error!(
            code = ErrorCode::Spot1m02PersistFailed.code_str(),
            stage = "flush",
            ?err,
            "SPOT1M-02: sweep ILP flush failed — pending swept rows \
             discarded (poisoned-buffer defense)"
        );
        stats.still_missing = stats.still_missing.saturating_add(stats.swept);
        stats.swept = 0;
        // Gap-11 forensics: the staged swept minutes become flush_failed
        // rows — captured from `staged` BEFORE the commit arm consumes it.
        if let Some(w) = audit.as_deref_mut() {
            for (security_id, minute_nanos) in &staged {
                audit_append_best_effort(
                    w,
                    &build_dhan_fetch_audit_row(
                        *minute_nanos,
                        trading_date_nanos,
                        *security_id,
                        spot_1m_symbol_for_sid(*security_id),
                        1,
                        0,
                        RestFetchOutcome::NamedGap,
                        -1,
                        "flush_failed",
                    ),
                );
            }
        }
    } else {
        for (security_id, minute_nanos) in staged {
            tracker.commit(security_id, minute_nanos);
        }
    }
    if let Some(w) = audit {
        // GAP-11: swept ok rows land ONLY after (and stamped with) the data
        // flush ACK — discarded on a failed flush (the flush_failed rows
        // above are the truth for those minutes).
        for row in stamp_held_ok_rows(
            held_ok_rows,
            flush_result.is_ok(),
            trading_date_nanos,
            ist_millis_of_day_now(),
        ) {
            audit_append_best_effort(w, &row);
        }
        // Gap-11 forensics: one best-effort flush for the sweep's audit rows.
        audit_flush_best_effort(w);
    }
    stats
}

/// 2026-07-14 batch catch-up mode (`fetch_mode = "batch_catchup"`): every
/// `batch_interval_minutes` in session, run ONE sweep cycle — one
/// day-window fetch per SID (through the shared Dhan Data-API limiter)
/// persisting every session minute above the per-SID watermark. Returns
/// `true` when today's grid completed (the caller then runs the
/// 15:33:30 post-session sweep for the tail), `false` on a trading-day
/// flip mid-loop. A thin mode wrapper over [`sweep_sids_above_watermark`]
/// — NOT new fetch logic.
// TEST-EXEMPT: live-deps async runner — the grid scheduling (next_batch_fire_after) and cycle verdict (batch_cycle_fully_failed) are pure fns unit-tested below; the fetch/persist body is the shared sweep helper.
async fn run_batch_catchup_loop(
    params: &Spot1mRestTaskParams,
    client: &reqwest::Client,
    url: &str,
    writer: &mut Spot1mRestWriter,
    tracker: &mut PersistTracker,
) -> bool {
    let mut edge = FailureEdge::default();
    let mut last_fired: Option<u32> = None;
    info!(
        interval_minutes = params.batch_interval_minutes,
        "spot_1m_rest: BATCH CATCH-UP mode armed (one day-window fetch per \
         SID per cycle through the shared Dhan Data-API limiter; per-minute \
         fires disabled by config)"
    );
    loop {
        // Audit Rule 3: re-read the wall clock + trading-day verdict every
        // iteration.
        if !params.calendar.is_trading_day_today() {
            // 2026-07-14: loud + coded (was a bare info!) — same class as
            // the per-minute loop's flip exit; this names the BATCH loop.
            metrics::counter!("tv_spot1m_trading_day_flip_exit_total").increment(1);
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "trading_day_flip_exit",
                "SPOT1M-01: the trading-day verdict flipped mid-session — \
                 exiting the batch catch-up loop (a suspend that crossed \
                 IST midnight is a legitimate cause; remaining cycles \
                 stay absent, re-fetchable via backfill)"
            );
            return false;
        }
        let now = ist_secs_of_day_now();
        let Some(fire) = next_batch_fire_after(now, last_fired, params.batch_interval_minutes)
        else {
            info!(
                "spot_1m_rest: batch grid complete — handing over to the \
                 post-session sweep"
            );
            return true;
        };
        let sleep_ms =
            u64::from(fire.saturating_sub(now)).saturating_mul(1_000) + SPOT_1M_REST_FIRE_DELAY_MS;
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        last_fired = Some(fire);

        // A late wake is FINE in batch mode — the cycle's ceiling is
        // recomputed from the wall clock (catching up is the mode's whole
        // point); a midnight wrap exits via the trading-day gate above.
        let woke = ist_secs_of_day_now().min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST);
        let last_closed_minute_open_secs = woke.saturating_sub(60);
        let trading_date = today_ist();
        let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
        let session_first =
            minute_open_ist_nanos(trading_date, SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST - 60);
        let up_to = minute_open_ist_nanos(trading_date, last_closed_minute_open_secs);

        let jwt: Option<secrecy::SecretString> = {
            let guard = params.token_handle.load();
            guard
                .as_ref()
                .as_ref()
                .map(|state| state.access_token().clone())
        };
        let Some(jwt) = jwt else {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "batch_cycle_failed",
                "SPOT1M-01: no access token at batch catch-up time — this \
                 cycle is a full miss (next cycle retries)"
            );
            metrics::counter!("tv_spot1m_batch_cycles_total", "outcome" => "error").increment(1);
            record_batch_cycle_verdict(params, &mut edge, fire, 0, 0, true);
            continue;
        };
        let stats = sweep_sids_above_watermark(
            client,
            url,
            writer,
            tracker,
            &jwt,
            trading_date,
            trading_date_nanos,
            session_first,
            up_to,
            "batch_cycle_failed",
            // Gap-11: NO forensics mid-session — a batch cycle's missing
            // minute may be recovered by the next cycle; only the terminal
            // post-session sweep stamps named-gap/ok rows (see the helper
            // doc note).
            None,
        )
        .await;
        let fully_failed =
            batch_cycle_fully_failed(stats.missing_before, stats.swept, stats.persist_failed);
        let outcome = if fully_failed { "failed" } else { "ok" };
        metrics::counter!("tv_spot1m_batch_cycles_total", "outcome" => outcome).increment(1);
        info!(
            cycle_ist = %format_minute_ist_12h(fire),
            missing_before = stats.missing_before,
            swept = stats.swept,
            still_missing = stats.still_missing,
            persist_failed = stats.persist_failed,
            "spot_1m_rest: batch catch-up cycle complete"
        );
        record_batch_cycle_verdict(
            params,
            &mut edge,
            fire,
            stats.missing_before,
            stats.swept,
            stats.persist_failed,
        );
        // PR-3 sequencing parity: the chain leg's own 2.5s fallback timer
        // fires it per minute regardless; publishing per cycle keeps the
        // watch channel warm when both halves are enabled.
        if let Some(tx) = &params.minute_done_tx {
            tx.send_replace(Some(fire));
        }
    }
}

/// Batch-mode edge accounting: feeds the SAME [`FailureEdge`] +
/// SPOT1M-01 escalation/recovery events the per-minute mode uses (3
/// consecutive fully-failed CYCLES page once; any good cycle recovers).
fn record_batch_cycle_verdict(
    params: &Spot1mRestTaskParams,
    edge: &mut FailureEdge,
    fire_secs_of_day: u32,
    missing_before: u64,
    swept: u64,
    persist_failed: bool,
) {
    let minute_label = format_minute_ist_12h(fire_secs_of_day);
    let fully_failed = batch_cycle_fully_failed(missing_before, swept, persist_failed);
    match edge.record_minute(fully_failed) {
        EdgeAction::Page { consecutive } => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "escalation",
                consecutive,
                minute = %minute_label,
                "SPOT1M-01: batch catch-up cycles fully failed consecutively \
                 — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::Spot1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label,
                });
        }
        EdgeAction::Recover { failed_minutes } => {
            info!(
                failed_cycles = failed_minutes,
                minute = %minute_label,
                "spot_1m_rest: batch catch-up recovered after a paged episode"
            );
            params
                .notifier
                .notify(NotificationEvent::Spot1mFetchRecovered {
                    minute_ist: minute_label,
                    failed_minutes,
                });
        }
        EdgeAction::None => {}
    }
}

/// ONE bounded post-session repair sweep (~15:33:30 IST, single fire —
/// M1, review 2026-07-13; moved off 15:31 the same day so it clears the
/// bulk cross-verify's observed 15:31–15:33 429 burst window): once the
/// session is final, re-fetch the proven
/// day window ONCE per SID that still has session minutes above its
/// persisted watermark (the 15:29 candle after a vendor-late seal, a
/// flush-failed backfill row, any tail gap the per-minute one-minute
/// lookback could not reach) and persist every one found. Bounded: ≤4
/// requests total (one per pinned SID since the 2026-07-13 VIX addition),
/// ≤375 minutes/SID, DEDUP-idempotent re-appends; loud
/// counters (`tv_spot1m_sweep_backfilled_total` /
/// `tv_spot1m_sweep_still_missing_total`) + one coalesced coded log; the
/// swept rows' `close_to_data_ms` column stamps the honest real delay.
/// The membership filter is O(missing × candles) ≤ 375×375 once per day —
/// cold path, flagged honestly.
// TEST-EXEMPT: live-deps async runner — the missing-minute selection (sweep_missing_minutes) and row/window builders are pure fns unit-tested below; the HTTP+persist legs reuse the tested fire_one_minute pattern.
async fn run_post_session_sweep(
    client: &reqwest::Client,
    url: &str,
    params: &Spot1mRestTaskParams,
    writer: &mut Spot1mRestWriter,
    audit_writer: &mut RestFetchAuditWriter,
    tracker: &mut PersistTracker,
) {
    // Wait for the sweep instant (a run reaching here right at 15:30:00
    // sleeps ~60s; a late boot past 15:31 fires immediately).
    let now = ist_secs_of_day_now();
    if now < SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST {
        tokio::time::sleep(Duration::from_secs(u64::from(
            SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST - now,
        )))
        .await;
    }
    // Same-day defense: a suspend across midnight (seconds-of-day wrapped
    // below the close) or a day flip means the session data is no longer
    // "today's" — skip rather than stamp the wrong trading date.
    let woke = ist_secs_of_day_now();
    if woke < SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST || !params.calendar.is_trading_day_today() {
        warn!(
            woke_at_secs = woke,
            "spot_1m_rest: post-session sweep woke outside today's session \
             (midnight wrap / non-trading day) — skipping"
        );
        return;
    }
    let trading_date = today_ist();
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
    let session_first =
        minute_open_ist_nanos(trading_date, SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST - 60);
    let session_last =
        minute_open_ist_nanos(trading_date, SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST - 60);
    let jwt: Option<secrecy::SecretString> = {
        let guard = params.token_handle.load();
        guard
            .as_ref()
            .as_ref()
            .map(|state| state.access_token().clone())
    };
    let Some(jwt) = jwt else {
        error!(
            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
            stage = "sweep_failed",
            "SPOT1M-01: no access token at post-session sweep time — \
             missing minutes stay absent (DEDUP-idempotent manual re-run \
             remains possible)"
        );
        return;
    };

    // Gap-11 forensics ride the SHARED sweep body (`Some(audit_writer)`):
    // named_gap / persist_failed / flush_failed rows + held-then-stamped
    // `ok` rows land exactly as the pre-refactor inline sweep emitted them.
    let stats = sweep_sids_above_watermark(
        client,
        url,
        writer,
        tracker,
        &jwt,
        trading_date,
        trading_date_nanos,
        session_first,
        session_last,
        "sweep_failed",
        Some(audit_writer),
    )
    .await;
    let (swept, still_missing, persist_failed) =
        (stats.swept, stats.still_missing, stats.persist_failed);
    metrics::counter!("tv_spot1m_sweep_backfilled_total").increment(swept);
    metrics::counter!("tv_spot1m_sweep_still_missing_total").increment(still_missing);
    if still_missing > 0 || persist_failed {
        error!(
            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
            stage = "sweep_incomplete",
            swept,
            still_missing,
            persist_failed,
            "SPOT1M-01: post-session sweep left session minutes absent — \
             the day's table stays short (DEDUP-idempotent manual re-run \
             remains possible)"
        );
    } else {
        info!(
            swept,
            "spot_1m_rest: post-session sweep complete — every session \
             minute above the watermark is persisted"
        );
    }
}

/// 2026-07-13 VIX companion: feed one fired minute's per-SID served
/// verdicts into the [`SidServedTracker`] and emit the edge-latched
/// per-SID page / recovery ping + the per-counted-minute counter
/// (`tv_spot1m_sid_not_served_total{symbol}` — 4 static label values, the
/// pinned index symbols). Counting semantics live in the tracker doc.
fn record_sid_served_verdicts(
    params: &Spot1mRestTaskParams,
    sid_tracker: &mut SidServedTracker,
    verdicts: &[(SecurityId, &'static str, bool)],
    minute_label: &str,
) {
    if verdicts.is_empty() {
        return;
    }
    let any_served = verdicts.iter().any(|&(_, _, served)| served);
    let pairs: Vec<(SecurityId, bool)> = verdicts
        .iter()
        .map(|&(sid, _, served)| (sid, served))
        .collect();
    let actions = sid_tracker.record_minute(&pairs);
    for (&(security_id, symbol, served), &(_, action)) in verdicts.iter().zip(actions.iter()) {
        if !served && any_served {
            // One counted vendor-not-serving minute for this SID (a
            // global-outage minute is deliberately NOT counted here).
            metrics::counter!("tv_spot1m_sid_not_served_total", "symbol" => symbol).increment(1);
        }
        match action {
            SidEdgeAction::Page { consecutive } => {
                error!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage = "sid_not_served",
                    sid = security_id,
                    symbol,
                    consecutive,
                    minute = minute_label,
                    "SPOT1M-01: the vendor is not returning 1-minute candles \
                     for this index while the other indices succeed — paging \
                     once per SID (edge-latched; re-armed on this SID's \
                     recovery)"
                );
                params
                    .notifier
                    .notify(NotificationEvent::Spot1mSidNotServed {
                        symbol: symbol.to_string(),
                        consecutive_minutes: consecutive,
                    });
            }
            SidEdgeAction::Recover { not_served_minutes } => {
                info!(
                    sid = security_id,
                    symbol,
                    not_served_minutes,
                    minute = minute_label,
                    "spot_1m_rest: this index is being served again after a \
                     paged not-served episode"
                );
                params
                    .notifier
                    .notify(NotificationEvent::Spot1mSidServedRecovered {
                        symbol: symbol.to_string(),
                        not_served_minutes,
                    });
            }
            SidEdgeAction::None => {}
        }
    }
}

/// Coalesced per-minute verdict: ONE coded log per fired minute with any
/// failure, plus the edge-triggered escalation page / recovery ping.
#[allow(clippy::too_many_arguments)] // APPROVED: private verdict sink — a struct would be pure ceremony
fn record_minute_verdict(
    params: &Spot1mRestTaskParams,
    edge: &mut FailureEdge,
    minute_label: &str,
    ok_count: usize,
    error_count: usize,
    empty_count: usize,
    persist_failed: bool,
    sample_failure: Option<&str>,
    empty_diag: &EmptyDiagnostics,
    trading_date_nanos: i64,
) {
    let fully_failed = minute_fully_failed(ok_count, persist_failed);
    let action = edge.record_minute(fully_failed);
    match action {
        EdgeAction::Page { consecutive } => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "escalation",
                consecutive,
                minute = minute_label,
                sample = sample_failure.unwrap_or("none captured"),
                "SPOT1M-01: per-minute spot fetch fully failed for consecutive \
                 minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::Spot1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label.to_string(),
                });
        }
        EdgeAction::Recover { failed_minutes } => {
            info!(
                failed_minutes,
                minute = minute_label,
                "spot_1m_rest: per-minute fetch recovered after a paged episode"
            );
            params
                .notifier
                .notify(NotificationEvent::Spot1mFetchRecovered {
                    minute_ist: minute_label.to_string(),
                    failed_minutes,
                });
        }
        EdgeAction::None => {
            if error_count > 0 || empty_count > 0 || persist_failed {
                // 2026-07-14 serving-delay instrumentation: the empty-class
                // split fields ride the SAME coalesced line — no new log
                // cadence, just honest accounting of WHICH empty state
                // each minute saw.
                let last_candle_ist = empty_diag.latest_candle_ist_nanos.map_or_else(
                    || "n/a".to_string(),
                    |n| ist_nanos_minute_label(n, trading_date_nanos),
                );
                // Coalesced ONCE per fire (never per retry); log-sink-only —
                // sub-edge failures never page (the escalation arm does).
                error!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage = "minute_failed",
                    minute = minute_label,
                    ok = ok_count,
                    errors = error_count,
                    empty = empty_count,
                    empty_no_rows = empty_diag.no_rows,
                    empty_stale = empty_diag.stale,
                    rows_in_response = empty_diag.max_rows_in_response,
                    last_candle_ist = %last_candle_ist,
                    max_serving_lag_secs = empty_diag.max_serving_lag_secs,
                    persist_failed,
                    sample = sample_failure.unwrap_or("none captured"),
                    "SPOT1M-01: per-minute spot fetch degraded for this minute"
                );
            }
        }
    }
}

/// Spawn the supervised per-minute scheduler. The supervisor respawns a
/// dead/failed run after a bounded backoff so the scheduler can never die
/// silently mid-session, and exits cleanly once today's window is over
/// (non-trading day / past 15:30 IST) or on graceful-shutdown cancel.
// TEST-EXEMPT: tokio supervisor wiring over the unit-tested pure decisions (spot_1m_day_is_over / classify_join_exit); spawn site pinned by crates/app/tests/spot_1m_rest_wiring_guard.rs.
pub fn spawn_supervised_spot_1m_rest(params: Spot1mRestTaskParams) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let inner = tokio::spawn(run_spot_1m_rest(params.clone()));
            let result = inner.await;
            let reason = classify_join_exit(&result);
            let day_over = spot_1m_day_is_over(
                ist_secs_of_day_now(),
                params.calendar.is_trading_day_today(),
            );
            match &result {
                Ok(()) if day_over => {
                    info!("spot_1m_rest: day complete — supervisor exiting");
                    return;
                }
                Err(join_err) if join_err.is_cancelled() => {
                    // Graceful shutdown teardown — not an abort.
                    return;
                }
                _ => {}
            }
            metrics::counter!("tv_spot1m_task_respawn_total", "reason" => reason).increment(1);
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "task_respawn",
                reason,
                "SPOT1M-01: per-minute spot fetch task died mid-window — \
                 respawning after backoff"
            );
            tokio::time::sleep(Duration::from_secs(SPOT_1M_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: u32 = SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST; // 09:16:00
    const LAST: u32 = SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST; // 15:30:00

    // ---- next_minute_close_fire -------------------------------------------

    #[test]
    fn test_next_minute_close_fire_before_window_selects_0916() {
        assert_eq!(next_minute_close_fire(0), Some(FIRST));
        assert_eq!(next_minute_close_fire(8 * 3600 + 30 * 60), Some(FIRST));
        // 09:15:59 — still the first boundary.
        assert_eq!(next_minute_close_fire(FIRST - 1), Some(FIRST));
    }

    #[test]
    fn test_next_fire_exactly_on_boundary_fires_now() {
        assert_eq!(next_minute_close_fire(FIRST), Some(FIRST));
        assert_eq!(next_minute_close_fire(10 * 3600), Some(10 * 3600));
        assert_eq!(next_minute_close_fire(LAST), Some(LAST));
    }

    #[test]
    fn test_next_fire_mid_minute_rounds_up_to_next_boundary() {
        // 10:42:17 → 10:43:00.
        let now = 10 * 3600 + 42 * 60 + 17;
        assert_eq!(next_minute_close_fire(now), Some(10 * 3600 + 43 * 60));
        // 15:29:01 → the final 15:30:00 boundary (the 15:29 candle).
        assert_eq!(next_minute_close_fire(15 * 3600 + 29 * 60 + 1), Some(LAST));
    }

    #[test]
    fn test_next_fire_past_window_returns_none() {
        assert_eq!(next_minute_close_fire(LAST + 1), None);
        assert_eq!(next_minute_close_fire(16 * 3600), None);
        assert_eq!(next_minute_close_fire(23 * 3600 + 59 * 60 + 59), None);
    }

    /// The full boundary walk covers exactly 375 fires — one per session
    /// minute (09:15..=15:29 candle opens).
    #[test]
    fn test_fire_walk_covers_exactly_375_minutes() {
        let mut fires = 0u32;
        let mut now = 0u32;
        while let Some(fire) = next_minute_close_fire(now) {
            fires += 1;
            now = fire + 1;
        }
        assert_eq!(fires, 375, "one fire per session minute");
    }

    // ---- next_fire_after (H1 — same-second duplicate re-fire) ---------------

    /// A fully-successful fire completing WITHIN its boundary second must
    /// select the NEXT boundary, never the same one again (2026-07-12 H1).
    #[test]
    fn test_next_fire_after_fast_completion_same_second_never_refires() {
        let fire = 10 * 3600; // 10:00:00 boundary just fired
        // Post-fire wall clock still reads the boundary second.
        assert_eq!(next_fire_after(fire, Some(fire)), Some(fire + 60));
        // Sub-second later (clock now fire+1): still the next boundary.
        assert_eq!(next_fire_after(fire + 1, Some(fire)), Some(fire + 60));
    }

    /// An instant-failing fire loop (no token — completes in µs) must
    /// advance one boundary per fire, never re-fire the same boundary and
    /// never inflate the edge with duplicate "minutes".
    #[test]
    fn test_next_fire_after_instant_fail_loop_advances_one_boundary_per_fire() {
        let mut last_fired: Option<u32> = None;
        let now = 10 * 3600; // wall clock frozen at 10:00:00 (instant fires)
        let mut fired = Vec::new();
        for _ in 0..3 {
            let fire = next_fire_after(now, last_fired).expect("in window");
            fired.push(fire);
            last_fired = Some(fire);
        }
        assert_eq!(
            fired,
            vec![10 * 3600, 10 * 3600 + 60, 10 * 3600 + 120],
            "each instant fire advances exactly one boundary"
        );
    }

    /// Normal pacing (fire completes in ~1 s, next wake mid-minute) is
    /// unchanged by the H1 horizon: the next boundary is selected.
    #[test]
    fn test_next_fire_after_normal_pacing_selects_next_boundary() {
        let fire = 10 * 3600;
        // 10:00:01 after firing 10:00:00 → 10:01:00.
        assert_eq!(next_fire_after(fire + 1, Some(fire)), Some(fire + 60));
        // First iteration of the day (no last_fired): unchanged semantics.
        assert_eq!(next_fire_after(FIRST - 100, None), Some(FIRST));
        // The LAST boundary fired → None (day complete), never a re-fire.
        assert_eq!(next_fire_after(LAST, Some(LAST)), None);
    }

    // ---- count_missed_boundaries (H2 — overrun accounting) ------------------

    #[test]
    fn test_count_missed_boundaries_zero_within_same_minute() {
        let fire = 10 * 3600;
        // Fire completed 5 s into its own minute: nothing missed.
        assert_eq!(count_missed_boundaries(fire, fire + 5), 0);
        assert_eq!(count_missed_boundaries(fire, fire), 0);
        // `now - 1` convention: an exact next-boundary wall clock is NOT
        // missed (it fires now) — the caller passes now-1.
        assert_eq!(count_missed_boundaries(fire, fire + 59), 0);
    }

    #[test]
    fn test_count_missed_boundaries_counts_each_overrun_minute() {
        let fire = 10 * 3600;
        // A 61 s fire: wall clock now fire+61 → caller passes fire+60 →
        // boundary fire+60 was passed (< now) → 1 missed.
        assert_eq!(count_missed_boundaries(fire, fire + 60), 1);
        // An 81 s ladder-class overrun: still 1 (fire+60 missed; fire+120
        // not yet reached).
        assert_eq!(count_missed_boundaries(fire, fire + 80), 1);
        // A 130 s stall: 2 boundaries gone.
        assert_eq!(count_missed_boundaries(fire, fire + 130), 2);
    }

    #[test]
    fn test_count_missed_boundaries_clamps_at_session_last() {
        // Overrun past 15:30: only boundaries up to LAST count.
        assert_eq!(count_missed_boundaries(LAST - 60, LAST + 3600), 1);
        assert_eq!(count_missed_boundaries(LAST, LAST + 3600), 0);
    }

    // ---- minute_fully_failed (M1 — persist confirmation) --------------------

    #[test]
    fn test_minute_fully_failed_requires_fetch_and_persist_ok() {
        // Fetch ok + persist ok → not failed.
        assert!(!minute_fully_failed(3, false));
        assert!(!minute_fully_failed(1, false));
        // No SID fetched → failed regardless of persist.
        assert!(minute_fully_failed(0, false));
        // Fetched but persist (append/flush) failed → STILL failed for the
        // edge: a day-long QuestDB outage must page (M1).
        assert!(minute_fully_failed(3, true));
        assert!(minute_fully_failed(0, true));
    }

    // ---- 2026-07-14 stale-watermark cutoff ----------------------------------

    #[test]
    fn test_ladder_watermark_repeated_stops_on_equal() {
        // Two parsed payloads with the SAME newest-candle watermark →
        // stop (re-polling cannot outrun a serving delay).
        assert!(ladder_watermark_repeated(Some(Some(1_000)), Some(1_000)));
    }

    #[test]
    fn test_ladder_watermark_zero_rows_twice_repeats() {
        // Two consecutive parsed ZERO-ROW day payloads also repeat —
        // today's empty_no_rows regime is cut at attempt 2.
        assert!(ladder_watermark_repeated(Some(None), None));
    }

    #[test]
    fn test_ladder_watermark_advancing_does_not_stop() {
        // The vendor served a NEWER candle between polls — keep laddering.
        assert!(!ladder_watermark_repeated(Some(Some(1_000)), Some(1_060)));
        // Rows appeared where there were none — keep laddering.
        assert!(!ladder_watermark_repeated(Some(None), Some(1_000)));
    }

    #[test]
    fn test_ladder_watermark_first_observation_never_stops() {
        // No previous PARSED attempt (first attempt, or every prior
        // attempt failed transport/non-2xx) → never a cutoff.
        assert!(!ladder_watermark_repeated(None, Some(1_000)));
        assert!(!ladder_watermark_repeated(None, None));
    }

    // ---- 2026-07-14 adaptive degrade ----------------------------------------

    #[test]
    fn test_degrade_enters_after_five_no_data_minutes() {
        let mut d = LadderDegrade::default();
        let threshold = SPOT_1M_REST_DEGRADE_AFTER_CONSECUTIVE_NO_DATA_MINUTES;
        for _ in 0..threshold - 1 {
            assert_eq!(d.record_minute(false), DegradeAction::None);
            assert!(!d.is_degraded());
        }
        assert_eq!(
            d.record_minute(false),
            DegradeAction::EnterDegraded {
                consecutive_no_data: threshold
            },
            "the threshold-th consecutive no-data minute enters degrade"
        );
        assert!(d.is_degraded());
        // Further no-data minutes never re-log the transition (edge).
        assert_eq!(d.record_minute(false), DegradeAction::None);
        assert!(d.is_degraded());
    }

    #[test]
    fn test_degrade_any_success_rearms() {
        let mut d = LadderDegrade::default();
        for _ in 0..SPOT_1M_REST_DEGRADE_AFTER_CONSECUTIVE_NO_DATA_MINUTES {
            d.record_minute(false);
        }
        assert!(d.is_degraded());
        d.record_minute(false);
        d.record_minute(false);
        assert_eq!(
            d.record_minute(true),
            DegradeAction::ExitDegraded {
                degraded_minutes: 2
            },
            "ANY success exits degrade (one falling-edge log)"
        );
        assert!(!d.is_degraded());
        // A pre-threshold success is a plain None (no phantom exit log).
        assert_eq!(d.record_minute(false), DegradeAction::None);
        assert_eq!(d.record_minute(true), DegradeAction::None);
    }

    #[test]
    fn test_degrade_attempt_count_single() {
        assert_eq!(
            ladder_attempt_count(true),
            1,
            "degraded = single attempt, no ladder re-polls"
        );
        assert_eq!(
            ladder_attempt_count(false),
            SPOT_1M_REST_RETRY_OFFSETS_MS.len() + 1,
            "healthy = the full ladder schedule"
        );
    }

    // ---- 2026-07-14 batch catch-up scheduling --------------------------------

    #[test]
    fn test_next_batch_fire_grid_alignment() {
        // Before the window → the first per-minute boundary.
        assert_eq!(next_batch_fire_after(FIRST - 100, None, 5), Some(FIRST));
        // Mid-session → the next 5-minute grid point after the floor.
        let fired = FIRST;
        let next = next_batch_fire_after(FIRST + 1, Some(fired), 5);
        assert_eq!(next, Some(FIRST + 5 * 60), "grid stays FIRST + k×300s");
        // A slow cycle that overran one grid point skips to the NEXT one
        // (never a double-fire of the same boundary).
        let next = next_batch_fire_after(FIRST + 5 * 60 + 30, Some(FIRST + 5 * 60), 5);
        assert_eq!(next, Some(FIRST + 10 * 60));
        // A hostile 0 interval is treated as 1 minute (validate() is the
        // real gate).
        assert_eq!(
            next_batch_fire_after(FIRST + 1, Some(FIRST), 0),
            Some(FIRST + 60)
        );
    }

    #[test]
    fn test_next_batch_fire_past_window_none() {
        assert_eq!(
            next_batch_fire_after(LAST + 1, Some(LAST), 5),
            None,
            "past 15:30 IST the batch grid is over (sweep takes over)"
        );
        // The grid never emits a point past the session close.
        let last_grid = next_batch_fire_after(LAST - 30, Some(LAST - 5 * 60), 5);
        assert!(last_grid.is_none_or(|f| f <= LAST));
    }

    #[test]
    fn test_batch_cycle_fully_failed_semantics() {
        // Minutes were due, none landed → failed (fetch outage OR the
        // all-empty vendor regime — honest either way).
        assert!(batch_cycle_fully_failed(10, 0, false));
        // Persist failure is always a failed cycle (M1 discipline).
        assert!(batch_cycle_fully_failed(10, 10, true));
        assert!(batch_cycle_fully_failed(0, 0, true));
        // Nothing due (idle cycle) → OK.
        assert!(!batch_cycle_fully_failed(0, 0, false));
        // Partial progress → OK (the remainder is next cycle's work).
        assert!(!batch_cycle_fully_failed(10, 3, false));
    }

    // ---- body cap (security M — unbounded read) ------------------------------

    #[test]
    fn test_declared_len_within_cap_rejects_oversize_content_length() {
        let cap = SPOT_1M_REST_MAX_BODY_BYTES;
        // Declared Content-Length beyond the cap → rejected up front.
        assert!(!declared_len_within_cap(Some(cap as u64 + 1), cap));
        assert!(declared_len_within_cap(Some(cap as u64), cap));
        assert!(declared_len_within_cap(Some(0), cap));
        // Absent declaration passes the pre-check (streamed cap enforces).
        assert!(declared_len_within_cap(None, cap));
    }

    #[test]
    fn test_accumulation_within_cap_rejects_streamed_overrun() {
        let cap = SPOT_1M_REST_MAX_BODY_BYTES;
        // Streamed accumulation: the chunk that would cross the cap is
        // rejected — no unbounded buffering.
        assert!(accumulation_within_cap(0, cap, cap));
        assert!(!accumulation_within_cap(1, cap, cap));
        assert!(!accumulation_within_cap(cap, 1, cap));
        // Overflow-safe (saturating add, never wraps to a small value).
        assert!(!accumulation_within_cap(usize::MAX, usize::MAX, cap));
    }

    // ---- fire_is_fresh -----------------------------------------------------

    #[test]
    fn test_fire_is_fresh_within_grace() {
        assert!(fire_is_fresh(FIRST, FIRST));
        assert!(fire_is_fresh(FIRST, FIRST + 1));
        assert!(fire_is_fresh(
            FIRST,
            FIRST + SPOT_1M_REST_FIRE_STALE_GRACE_SECS
        ));
    }

    #[test]
    fn test_fire_is_fresh_rejects_stale_and_midnight_wrap() {
        assert!(!fire_is_fresh(
            FIRST,
            FIRST + SPOT_1M_REST_FIRE_STALE_GRACE_SECS + 1
        ));
        // Next-day wake: seconds-of-day wrapped below the boundary.
        assert!(!fire_is_fresh(LAST, 9 * 3600));
        assert!(!fire_is_fresh(LAST, 0));
    }

    // ---- spot_1m_day_is_over ----------------------------------------------

    #[test]
    fn test_day_is_over_non_trading_day_or_past_window() {
        assert!(spot_1m_day_is_over(10 * 3600, false));
        assert!(spot_1m_day_is_over(LAST + 1, true));
        assert!(!spot_1m_day_is_over(10 * 3600, true));
        // The last boundary itself is still IN the day.
        assert!(!spot_1m_day_is_over(LAST, true));
    }

    // ---- request body (2026-07-13 hotfix: PROVEN day-granular window) -------

    /// 2026-07-13 live-failure regression lock: the request body carries
    /// the ONLY live-proven window shape — the day-granular
    /// `[D 00:00:00, D+1 00:00:00)` window the 15:31 cross-verify uses
    /// successfully every trading day. The retired same-date 60-second
    /// window was answered 2xx WITHOUT the target candle for every minute
    /// of the first live session (SPOT1M-01 all day).
    #[test]
    fn test_regression_spot_1m_day_request_body_uses_proven_day_window() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let body = spot_1m_day_request_body("13", date);
        // securityId is a STRING (orders.md rule 4 class), interval "1".
        assert_eq!(body["securityId"], "13");
        assert_eq!(body["exchangeSegment"], "IDX_I");
        assert_eq!(body["instrument"], "INDEX");
        assert_eq!(body["interval"], "1");
        assert_eq!(body["oi"], false);
        // The proven full-day window — NEVER a same-date minute window.
        assert_eq!(body["fromDate"], "2026-07-13 00:00:00");
        assert_eq!(body["toDate"], "2026-07-14 00:00:00");
    }

    /// Month boundary: toDate is the NEXT calendar day even across a
    /// month end (the cross-verify `succ_opt` semantics).
    #[test]
    fn test_spot_1m_day_request_body_month_boundary() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 31).expect("valid date");
        let body = spot_1m_day_request_body("51", date);
        assert_eq!(body["fromDate"], "2026-07-31 00:00:00");
        assert_eq!(body["toDate"], "2026-08-01 00:00:00");
    }

    // ---- timestamps + parsing ----------------------------------------------

    #[test]
    fn test_minute_open_ist_nanos_matches_ist_as_epoch_convention() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 10).expect("valid date");
        let day_start = date
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc()
            .timestamp();
        let open_secs = 9 * 3600 + 15 * 60;
        assert_eq!(
            minute_open_ist_nanos(date, open_secs),
            (day_start + i64::from(open_secs)) * 1_000_000_000
        );
    }

    /// UTC→IST (+19800) conversion against a REAL doc-convention fixture
    /// (annexure rule 13 / historical-data.md §5: response timestamps are
    /// UNIX **UTC** epoch seconds): the 2026-07-13 09:15:00 IST candle is
    /// UTC epoch `1783914300` (03:45:00 UTC) and MUST match the matcher's
    /// IST-as-epoch target `1_783_934_100e9` exactly.
    #[test]
    fn test_parse_intraday_columnar_utc_epoch_fixture_matches_ist_minute() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let open_secs: u32 = 9 * 3600 + 15 * 60;
        let target = minute_open_ist_nanos(date, open_secs);
        // The genuine UTC epoch for IST 2026-07-13 09:15:00.
        let utc_ts: i64 = 1_783_914_300;
        assert_eq!(target, (utc_ts + 19_800) * 1_000_000_000);
        // Full-day over-delivering body: an earlier minute, the target,
        // and the next minute — client-side filter must pick exactly the
        // target.
        let body = format!(
            r#"{{"open":[50.0,100.0,200.0],"high":[51.0,101.0,201.0],
                "low":[49.0,99.0,199.0],"close":[50.5,100.5,200.5],
                "volume":[0,0,0],"timestamp":[{prev},{utc_ts},{next}]}}"#,
            prev = utc_ts - 60,
            next = utc_ts + 60
        );
        let (candle, backfill, stats) = parse_intraday_columnar_for_minutes(&body, target, None);
        let candle = candle.expect("target found");
        assert!(backfill.is_none(), "no backfill requested → none returned");
        assert_eq!(candle.minute_ts_ist_nanos, target);
        assert_eq!(candle.open, 100.0);
        assert_eq!(candle.close, 100.5);
        // 2026-07-14 diagnostics: the body stats ride along — 3 rows, the
        // newest candle is the minute AFTER the target.
        assert_eq!(stats, Some((3, target + NANOS_PER_MINUTE)));
        // A target NOT in the body → None (the "empty" arm).
        let (missing, _, _) =
            parse_intraday_columnar_for_minutes(&body, target + 120 * 1_000_000_000, None);
        assert!(missing.is_none());
    }

    /// Backfill extraction from the SAME full-day body: the previous
    /// minute is returned alongside the target — and ALSO when the target
    /// itself is absent (the vendor-lateness recovery path).
    #[test]
    fn test_parse_intraday_columnar_for_minutes_backfill_hit() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let target = minute_open_ist_nanos(date, 9 * 3600 + 16 * 60); // 09:16
        let prev = target - NANOS_PER_MINUTE; // 09:15
        let utc_prev = prev / 1_000_000_000 - 19_800;
        // Body carries ONLY the previous minute (target not sealed yet).
        let body_prev_only = format!(
            r#"{{"open":[100.0],"high":[101.0],"low":[99.0],"close":[100.5],
                "volume":[0],"timestamp":[{utc_prev}]}}"#
        );
        let (t, b, stats) =
            parse_intraday_columnar_for_minutes(&body_prev_only, target, Some(prev));
        assert!(t.is_none(), "target minute not sealed yet");
        assert_eq!(
            b.expect("backfill found").minute_ts_ist_nanos,
            prev,
            "previous minute must backfill even when the target is absent"
        );
        // 2026-07-14 diagnostics: this is the STALE shape — 1 row whose
        // newest candle is one minute behind the target.
        assert_eq!(stats, Some((1, prev)));
        // Body carries BOTH → both returned.
        let body_both = format!(
            r#"{{"open":[100.0,200.0],"high":[101.0,201.0],"low":[99.0,199.0],
                "close":[100.5,200.5],"volume":[0,0],
                "timestamp":[{utc_prev},{utc_target}]}}"#,
            utc_target = utc_prev + 60
        );
        let (t, b, stats) = parse_intraday_columnar_for_minutes(&body_both, target, Some(prev));
        assert_eq!(t.expect("target").minute_ts_ist_nanos, target);
        assert_eq!(b.expect("backfill").minute_ts_ist_nanos, prev);
        assert_eq!(stats, Some((2, target)));
    }

    #[test]
    fn test_parse_for_minutes_malformed_short_and_mismatched_bodies_are_none() {
        let target = 1_770_000_900_000_000_000;
        let bf = Some(target - NANOS_PER_MINUTE);
        // Malformed JSON.
        assert_eq!(
            parse_intraday_columnar_for_minutes("not json", target, bf),
            (None, None, None)
        );
        // Missing arrays.
        assert_eq!(
            parse_intraday_columnar_for_minutes("{}", target, bf),
            (None, None, None)
        );
        // Empty arrays.
        let empty = r#"{"open":[],"high":[],"low":[],"close":[],"volume":[],"timestamp":[]}"#;
        assert_eq!(
            parse_intraday_columnar_for_minutes(empty, target, bf),
            (None, None, None)
        );
        // Length-mismatched parallel arrays.
        let mismatched = r#"{"open":[1.0,2.0],"high":[1.0],"low":[1.0],
            "close":[1.0],"volume":[0],"timestamp":[1752118500]}"#;
        assert_eq!(
            parse_intraday_columnar_for_minutes(mismatched, target, bf),
            (None, None, None)
        );
    }

    // ---- backfill sweep (2026-07-13 hotfix) ----------------------------------

    /// Backfill decision: due exactly when the previous minute is inside
    /// the session AND not already persisted.
    #[test]
    fn test_backfill_minute_nanos_hit_and_not_needed() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let session_first = minute_open_ist_nanos(date, FIRST - 60); // 09:15 open
        let target_0916 = session_first + NANOS_PER_MINUTE; // 09:16 open
        // Never persisted anything → previous minute (09:15) is due.
        assert_eq!(
            backfill_minute_nanos(None, target_0916, session_first),
            Some(session_first)
        );
        // Previous minute already persisted → no backfill.
        assert_eq!(
            backfill_minute_nanos(Some(session_first), target_0916, session_first),
            None
        );
        // An older watermark (gap) still only looks back ONE minute.
        let target_0920 = session_first + 5 * NANOS_PER_MINUTE;
        assert_eq!(
            backfill_minute_nanos(Some(session_first), target_0920, session_first),
            Some(target_0920 - NANOS_PER_MINUTE)
        );
    }

    /// The session's FIRST fire (target = the 09:15 candle) has no
    /// in-session previous minute — never a pre-open backfill.
    #[test]
    fn test_backfill_minute_nanos_first_session_minute_has_no_backfill() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let session_first = minute_open_ist_nanos(date, FIRST - 60);
        assert_eq!(
            backfill_minute_nanos(None, session_first, session_first),
            None
        );
    }

    /// `last_persisted` reads back exactly the committed watermark —
    /// per-SID, `None` before the first confirmed persist.
    #[test]
    fn test_persist_tracker_last_persisted_reads_committed_watermark() {
        let mut t = PersistTracker::default();
        assert_eq!(t.last_persisted(51), None);
        t.commit(51, 42);
        assert_eq!(t.last_persisted(51), Some(42));
        assert_eq!(t.last_persisted(13), None, "per-SID isolation");
    }

    // ---- post-session sweep (M1, review 2026-07-13) --------------------------

    /// The M1 scenario: the 15:29 candle sealed late, its own 15:30:00
    /// fire found nothing (watermark stuck at 15:28) — the 15:33:30 sweep
    /// must select exactly the 15:29 minute and find it in the final
    /// day-window response.
    #[test]
    fn test_sweep_recovers_vendor_late_1529_minute() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let session_first = minute_open_ist_nanos(date, FIRST - 60); // 09:15
        let session_last = minute_open_ist_nanos(date, LAST - 60); // 15:29
        let watermark_1528 = session_last - NANOS_PER_MINUTE;
        let missing = sweep_missing_minutes(Some(watermark_1528), session_first, session_last);
        assert_eq!(missing, vec![session_last], "exactly the 15:29 minute");
        // The post-close day window now carries the late-sealed candle.
        let utc_1529 = session_last / 1_000_000_000 - 19_800;
        let body = format!(
            r#"{{"open":[100.0],"high":[101.0],"low":[99.0],"close":[100.5],
                "volume":[0],"timestamp":[{utc_1529}]}}"#
        );
        let candles = parse_intraday_1m_candles(&body);
        let found = select_minute_candle(&candles, missing[0]).expect("15:29 swept in");
        assert_eq!(found.minute_ts_ist_nanos, session_last);
    }

    /// Sweep no-ops when nothing is missing: watermark at the session's
    /// last minute (15:29) → empty selection → zero requests.
    #[test]
    fn test_sweep_missing_minutes_noop_when_complete() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let session_first = minute_open_ist_nanos(date, FIRST - 60);
        let session_last = minute_open_ist_nanos(date, LAST - 60);
        assert!(sweep_missing_minutes(Some(session_last), session_first, session_last).is_empty());
        // A watermark somehow past the session (defensive): still empty.
        assert!(
            sweep_missing_minutes(
                Some(session_last + NANOS_PER_MINUTE),
                session_first,
                session_last
            )
            .is_empty()
        );
    }

    /// No watermark at all (post-close boot / fresh tracker): the sweep
    /// selects the WHOLE 375-minute session — the full-day repair case —
    /// and a multi-minute tail gap selects each missing minute.
    #[test]
    fn test_sweep_missing_minutes_full_session_and_tail_gap() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let session_first = minute_open_ist_nanos(date, FIRST - 60);
        let session_last = minute_open_ist_nanos(date, LAST - 60);
        let all = sweep_missing_minutes(None, session_first, session_last);
        assert_eq!(all.len(), 375, "one per session minute");
        assert_eq!(all[0], session_first);
        assert_eq!(*all.last().expect("non-empty"), session_last);
        // Tail gap: watermark at 15:27 → 15:28 + 15:29 both selected.
        let missing = sweep_missing_minutes(
            Some(session_last - 2 * NANOS_PER_MINUTE),
            session_first,
            session_last,
        );
        assert_eq!(missing, vec![session_last - NANOS_PER_MINUTE, session_last]);
    }

    // ---- 429 coordination (2026-07-13 follow-up) -----------------------------

    /// The sweep fire instant is pinned at 15:33:30 IST — strictly clear of
    /// the bulk cross-verify's observed 15:31–15:33 429 burst window
    /// (2026-07-13 live session: 91/776 cross-verify fetches 429'd there).
    #[test]
    fn test_sweep_fire_instant_clears_cross_verify_burst_window() {
        assert_eq!(
            SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST,
            15 * 3600 + 33 * 60 + 30, // 15:33:30 = 56_010
        );
        // ≥ 150 s after the 15:31:00 cross-verify trigger (burst observed
        // through 15:33) and before the 16:30 IST box stop.
        assert!(
            SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST
                >= crate::cross_verify_1m_boot::CROSS_VERIFY_TRIGGER_SECS_OF_DAY_IST + 150
        );
        assert!(SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST > SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST);
        assert!(SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST < 16 * 3600 + 30 * 60);
    }

    /// Deterministic per-SID jitter: one distinct value per slot of the
    /// pinned index array (0 / 150 / 300 / 450 ms at the 2026-07-13 4-SID
    /// arity), bounded by `(slots - 1) × step`, wrapping for defensive
    /// out-of-range slots. Slot-based (not `sid % n`) because the pinned
    /// SIDs give colliding residues — ladders would re-poll in lockstep.
    #[test]
    fn test_ladder_jitter_ms_bounds_and_decorrelation() {
        let jitters: Vec<u64> = (0..SPOT_1M_REST_INDICES.len())
            .map(ladder_jitter_ms)
            .collect();
        assert_eq!(
            jitters,
            vec![0, 150, 300, 450],
            "one distinct shift per SID"
        );
        let max = (SPOT_1M_REST_LADDER_JITTER_SLOTS - 1) * SPOT_1M_REST_LADDER_JITTER_STEP_MS;
        for j in &jitters {
            assert!(*j <= max, "jitter bounded by (slots-1) x step");
        }
        // All four schedules are pairwise distinct — no lockstep re-polls.
        for a in 0..jitters.len() {
            for b in (a + 1)..jitters.len() {
                assert_ne!(jitters[a], jitters[b], "slots {a} and {b} collide");
            }
        }
        // Defensive wrap for a hypothetical 5th slot.
        assert_eq!(ladder_jitter_ms(4), 0);
    }

    /// The ladder sleep composition: jitter shifts ONLY the first re-poll
    /// (later deltas are relative, so the whole schedule carries the
    /// shift), and a 429 on the previous attempt adds exactly the bounded
    /// extra backoff.
    #[test]
    fn test_ladder_sleep_ms_jitter_and_429_backoff_composition() {
        let deltas = retry_sleep_deltas_ms();
        // First re-poll, no 429: base + jitter.
        assert_eq!(
            ladder_sleep_ms(deltas[0], true, 300, false),
            deltas[0] + 300
        );
        // Later re-poll: jitter NOT re-applied.
        assert_eq!(ladder_sleep_ms(deltas[1], false, 300, false), deltas[1]);
        // 429 on the previous attempt: + the bounded extra backoff.
        assert_eq!(
            ladder_sleep_ms(deltas[2], false, 300, true),
            deltas[2] + SPOT_1M_REST_429_EXTRA_BACKOFF_MS
        );
        // First re-poll after a first-attempt 429: both apply.
        assert_eq!(
            ladder_sleep_ms(deltas[0], true, 150, true),
            deltas[0] + 150 + SPOT_1M_REST_429_EXTRA_BACKOFF_MS
        );
    }

    /// Worst-case all-429 jittered schedule stays inside the hard per-SID
    /// budget: every rung rate-limited (extra backoff before each of the 4
    /// re-polls) + max jitter + one full request timeout < 20 s.
    #[test]
    fn test_ladder_worst_case_429_schedule_stays_inside_sid_budget() {
        let deltas = retry_sleep_deltas_ms();
        let max_jitter =
            (SPOT_1M_REST_LADDER_JITTER_SLOTS - 1) * SPOT_1M_REST_LADDER_JITTER_STEP_MS;
        let mut total_sleep_ms: u64 = 0;
        for (i, delta) in deltas.iter().enumerate() {
            total_sleep_ms += ladder_sleep_ms(*delta, i == 0, max_jitter, true);
        }
        // Scheduled sleeps sum to the (jittered) last offset + 4 backoffs.
        assert_eq!(
            total_sleep_ms,
            SPOT_1M_REST_RETRY_OFFSETS_MS[3]
                + max_jitter
                + deltas.len() as u64 * SPOT_1M_REST_429_EXTRA_BACKOFF_MS
        );
        // Plus one full request timeout: still inside the hard budget the
        // `tokio::time::timeout` enforces (19.3 s < 20 s).
        assert!(
            total_sleep_ms + SPOT_1M_REST_REQUEST_TIMEOUT_SECS * 1_000
                < SPOT_1M_REST_SID_BUDGET_SECS * 1_000,
            "worst-case 429 ladder must fit the per-SID budget"
        );
    }

    /// Tracker: max-merge commits; double-persisting the same minute
    /// (DEDUP-idempotent server-side) never regresses the watermark.
    #[test]
    fn test_persist_tracker_commit_max_merge_and_double_persist_idempotent() {
        let mut t = PersistTracker::default();
        assert_eq!(t.last_persisted(13), None);
        t.commit(13, 1_000);
        assert_eq!(t.last_persisted(13), Some(1_000));
        // Double persist of the SAME minute: idempotent.
        t.commit(13, 1_000);
        assert_eq!(t.last_persisted(13), Some(1_000));
        // A backfill commit of an OLDER minute never regresses.
        t.commit(13, 500);
        assert_eq!(t.last_persisted(13), Some(1_000));
        // Advance.
        t.commit(13, 2_000);
        assert_eq!(t.last_persisted(13), Some(2_000));
        // Per-SID isolation.
        assert_eq!(t.last_persisted(25), None);
    }

    /// Edge accounting stays honest: a fire whose OWN target fetch found
    /// nothing is fully failed for the edge even when the same fire
    /// backfilled the previous minute (backfill persists never increment
    /// `ok_count` — structural: only the `Found` arm does).
    #[test]
    fn test_backfill_never_flips_edge_accounting() {
        assert!(minute_fully_failed(0, false));
        let mut t = PersistTracker::default();
        t.commit(13, 1_000); // a backfilled minute committed…
        assert!(minute_fully_failed(0, false)); // …edge verdict unchanged
    }

    // ---- ladder deltas ------------------------------------------------------

    #[test]
    fn test_retry_sleep_deltas_ms_reconstruct_the_offset_schedule() {
        let deltas = retry_sleep_deltas_ms();
        assert_eq!(deltas, [700, 800, 1_500, 3_000]);
        // Cumulative deltas reproduce the constant offsets exactly.
        let mut cumulative = 0u64;
        for (delta, offset) in deltas.iter().zip(SPOT_1M_REST_RETRY_OFFSETS_MS.iter()) {
            cumulative += delta;
            assert_eq!(cumulative, *offset);
        }
    }

    // ---- 12-hour label -------------------------------------------------------

    #[test]
    fn test_format_minute_ist_12h_commandment_9() {
        assert_eq!(format_minute_ist_12h(9 * 3600 + 15 * 60), "9:15 AM");
        assert_eq!(format_minute_ist_12h(12 * 3600), "12:00 PM");
        assert_eq!(format_minute_ist_12h(15 * 3600 + 29 * 60), "3:29 PM");
        assert_eq!(format_minute_ist_12h(0), "12:00 AM");
    }

    // ---- edge tracker ---------------------------------------------------------

    #[test]
    fn test_failure_edge_pages_once_at_threshold_then_stays_silent() {
        let mut edge = FailureEdge::default();
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        assert_eq!(
            edge.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        );
        // Minutes 4..N of the same episode: silent (already paged).
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        assert_eq!(edge.record_minute(true), EdgeAction::None);
    }

    #[test]
    fn test_failure_edge_recovers_once_and_rearms() {
        let mut edge = FailureEdge::default();
        for _ in 0..2 {
            edge.record_minute(true);
        }
        assert_eq!(
            edge.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        );
        edge.record_minute(true); // 4th failed minute, silent
        assert_eq!(
            edge.record_minute(false),
            EdgeAction::Recover { failed_minutes: 4 }
        );
        // Fully re-armed: a fresh episode pages again at the threshold.
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        assert_eq!(
            edge.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        );
    }

    #[test]
    fn test_failure_edge_success_below_threshold_never_emits() {
        let mut edge = FailureEdge::default();
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        // Recovery WITHOUT a page: no Recover event (never a false ping).
        assert_eq!(edge.record_minute(false), EdgeAction::None);
        assert_eq!(edge.record_minute(false), EdgeAction::None);
    }

    // ---- per-SID not-served detector (2026-07-13 — INDIA VIX companion) ----

    const NIFTY: SecurityId = 13;
    const BANKNIFTY: SecurityId = 25;
    const SENSEX: SecurityId = 51;
    const VIX: SecurityId = 21;
    const N: u32 = SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD;

    fn minute(vix_served: bool, others_served: bool) -> [(SecurityId, bool); 4] {
        [
            (NIFTY, others_served),
            (BANKNIFTY, others_served),
            (SENSEX, others_served),
            (VIX, vix_served),
        ]
    }

    fn vix_action(actions: &[(SecurityId, SidEdgeAction)]) -> SidEdgeAction {
        actions
            .iter()
            .find(|(sid, _)| *sid == VIX)
            .map(|&(_, a)| a)
            .expect("VIX action present")
    }

    /// A 3-ok / 1-empty minute is NOT fully-failed and NOT edge-counted —
    /// per-SID independence: a never-serving VIX can never fail or delay
    /// the other 3 (2026-07-13 requirement 2a).
    #[test]
    fn test_partial_minute_three_ok_one_empty_is_not_fully_failed_or_edge_counted() {
        // The M1 verdict: 3 SIDs succeeded, persist ok → NOT fully failed.
        assert!(!minute_fully_failed(3, false));
        // And the escalation edge never counts it: threshold-many such
        // minutes still page nothing.
        let mut edge = FailureEdge::default();
        for _ in 0..(SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD * 2) {
            assert_eq!(
                edge.record_minute(minute_fully_failed(3, false)),
                EdgeAction::None,
                "a 3-ok/1-empty minute must never feed the global edge"
            );
        }
    }

    /// LATCH: the per-SID detector pages ONCE at N counted not-served
    /// minutes (each with sibling successes) and never re-pages while
    /// latched.
    #[test]
    fn test_sid_not_served_pages_once_at_threshold_then_stays_latched() {
        let mut t = SidServedTracker::default();
        for i in 1..N {
            assert_eq!(
                vix_action(&t.record_minute(&minute(false, true))),
                SidEdgeAction::None,
                "below threshold at minute {i}"
            );
        }
        assert_eq!(
            vix_action(&t.record_minute(&minute(false, true))),
            SidEdgeAction::Page { consecutive: N },
            "rising edge at exactly N"
        );
        // No re-page while the episode continues.
        for _ in 0..20 {
            assert_eq!(
                vix_action(&t.record_minute(&minute(false, true))),
                SidEdgeAction::None,
                "latched — never a second page in the same episode"
            );
        }
        // The healthy siblings never see any action.
        let actions = t.record_minute(&minute(false, true));
        for &(sid, action) in &actions {
            if sid != VIX {
                assert_eq!(action, SidEdgeAction::None, "sibling {sid} untouched");
            }
        }
    }

    /// RE-ARM: the SID's own recovery emits ONE Recover and re-arms the
    /// latch — a later persistent episode pages again.
    #[test]
    fn test_sid_not_served_recovery_rearms_the_latch() {
        let mut t = SidServedTracker::default();
        for _ in 0..N {
            t.record_minute(&minute(false, true));
        }
        // Recovery: VIX served again → one Info edge.
        assert_eq!(
            vix_action(&t.record_minute(&minute(true, true))),
            SidEdgeAction::Recover {
                not_served_minutes: N
            }
        );
        // A second served minute emits nothing (no repeated recovery ping).
        assert_eq!(
            vix_action(&t.record_minute(&minute(true, true))),
            SidEdgeAction::None
        );
        // Re-armed: a fresh N-minute episode pages again.
        for _ in 1..N {
            assert_eq!(
                vix_action(&t.record_minute(&minute(false, true))),
                SidEdgeAction::None
            );
        }
        assert_eq!(
            vix_action(&t.record_minute(&minute(false, true))),
            SidEdgeAction::Page { consecutive: N }
        );
    }

    /// GLOBAL OUTAGE: minutes where NO SID succeeded neither count toward
    /// nor reset a SID's streak — the detector can never fire on a general
    /// outage (that is the FailureEdge's page), and a mid-streak global
    /// blip cannot launder a genuine vendor-not-serving streak.
    #[test]
    fn test_sid_not_served_global_outage_neither_counts_nor_resets() {
        let mut t = SidServedTracker::default();
        // A long pure global outage never fires the per-SID page.
        for _ in 0..(N * 3) {
            for &(_, action) in &t.record_minute(&minute(false, false)) {
                assert_eq!(
                    action,
                    SidEdgeAction::None,
                    "global outage must not page per-SID"
                );
            }
        }
        // A streak interrupted by a global-outage minute HOLDS (does not
        // reset): N-1 counted + 1 global + 1 counted = page.
        for _ in 1..N {
            assert_eq!(
                vix_action(&t.record_minute(&minute(false, true))),
                SidEdgeAction::None
            );
        }
        assert_eq!(
            vix_action(&t.record_minute(&minute(false, false))),
            SidEdgeAction::None,
            "global-outage minute holds the streak"
        );
        assert_eq!(
            vix_action(&t.record_minute(&minute(false, true))),
            SidEdgeAction::Page { consecutive: N },
            "the held streak completes on the next counted minute"
        );
    }

    /// Recovery without a page emits nothing (no false Info ping), and a
    /// no-token / empty-verdict minute is a no-op.
    #[test]
    fn test_sid_not_served_no_false_recovery_and_empty_verdicts_noop() {
        let mut t = SidServedTracker::default();
        t.record_minute(&minute(false, true)); // 1 counted, below threshold
        assert_eq!(
            vix_action(&t.record_minute(&minute(true, true))),
            SidEdgeAction::None,
            "recovery below the page threshold is silent"
        );
        assert!(
            t.record_minute(&[]).is_empty(),
            "empty verdicts are a no-op"
        );
    }

    // ---- select_minute_candle ---------------------------------------------

    #[test]
    fn test_select_minute_candle_exact_match_only() {
        let mk = |ts: i64| MinuteCandle {
            minute_ts_ist_nanos: ts,
            open: 1.0,
            high: 2.0,
            low: 0.5,
            close: 1.5,
            volume: 0,
        };
        let candles = [mk(60_000_000_000), mk(120_000_000_000)];
        assert_eq!(
            select_minute_candle(&candles, 120_000_000_000).map(|c| c.minute_ts_ist_nanos),
            Some(120_000_000_000)
        );
        assert!(select_minute_candle(&candles, 180_000_000_000).is_none());
        assert!(select_minute_candle(&[], 60_000_000_000).is_none());
    }

    // ---- 2026-07-14 serving-delay diagnostics -------------------------------

    fn mk_candle(ts: i64) -> MinuteCandle {
        MinuteCandle {
            minute_ts_ist_nanos: ts,
            open: 1.0,
            high: 2.0,
            low: 0.5,
            close: 1.5,
            volume: 0,
        }
    }

    /// The empty-class split: zero-row bodies (and no 2xx stats at all)
    /// classify `empty_no_rows`; a body with candles but none at the
    /// target classifies `empty_stale` with the measured serving lag; a
    /// found target never reaches classification (the ladder returns
    /// `Found` first — pinned indirectly by the Found-arm tests above).
    #[test]
    fn test_classify_empty_body_no_rows_vs_stale() {
        let target = 1_783_934_100_000_000_000; // an IST minute open
        // No 2xx stats at all (every body parsed to zero candles).
        assert_eq!(classify_empty_body(None, target), EmptyClass::NoRows);
        // Defensive: a zero-row stats tuple is still NoRows.
        assert_eq!(
            classify_empty_body(Some((0, target)), target),
            EmptyClass::NoRows
        );
        // Candles present, newest 4 minutes behind the target → STALE with
        // a 240 s serving lag.
        let last = target - 4 * NANOS_PER_MINUTE;
        assert_eq!(
            classify_empty_body(Some((21, last)), target),
            EmptyClass::Stale {
                rows_in_response: 21,
                last_candle_ist_nanos: last,
                serving_lag_secs: 240,
            }
        );
    }

    /// Serving-lag math: whole seconds of `target − last`, clamped ≥ 0
    /// (candles beyond the target with the exact target missing is a
    /// pathological gap — never a negative lag).
    #[test]
    fn test_serving_lag_secs_math_and_clamp() {
        let target = 1_783_934_100_000_000_000;
        assert_eq!(serving_lag_secs(target, target), 0);
        assert_eq!(serving_lag_secs(target, target - NANOS_PER_MINUTE), 60);
        assert_eq!(serving_lag_secs(target, target - 90 * NANOS_PER_SEC), 90);
        // A body reaching PAST the target clamps to 0, never negative.
        assert_eq!(serving_lag_secs(target, target + NANOS_PER_MINUTE), 0);
    }

    /// `body_stats` yields `(rows, newest minute)`; empty → `None`.
    #[test]
    fn test_body_stats_rows_and_newest_minute() {
        assert_eq!(body_stats(&[]), None);
        let c = [mk_candle(60_000_000_000), mk_candle(300_000_000_000)];
        assert_eq!(body_stats(&c), Some((2, 300_000_000_000)));
    }

    /// The per-fire aggregate folds both classes: counts, max rows, max
    /// lag, and the NEWEST candle across stale SIDs.
    #[test]
    fn test_raw_body_capture_day_key_is_stable_within_a_day() {
        // Two minutes of the same IST day share the key; the next day's
        // first minute gets a NEW key (the once-per-day boundary).
        let t0 = 1_783_934_100_000_000_000_i64; // some session minute
        let t1 = t0 + 5 * NANOS_PER_MINUTE;
        let next_day = t0 + NANOS_PER_DAY_I64;
        assert_eq!(raw_body_capture_day_key(t0), raw_body_capture_day_key(t1));
        assert_ne!(
            raw_body_capture_day_key(t0),
            raw_body_capture_day_key(next_day)
        );
    }

    #[test]
    fn test_raw_body_capture_due_pure_semantics() {
        // Unlatched (0) → due; same day latched → not due; a NEW day is
        // due again even after yesterday's capture.
        assert!(raw_body_capture_due(0, 20_650));
        assert!(!raw_body_capture_due(20_650, 20_650));
        assert!(raw_body_capture_due(20_650, 20_651));
    }

    #[test]
    fn test_try_claim_raw_body_capture_fires_exactly_once_per_day_key() {
        // Distinct key space (negative — no real IST nanos map here) so the
        // process-global latch never races the other tests / day keys.
        let key = -777_001_i64;
        assert!(try_claim_raw_body_capture(key), "first claim must win");
        assert!(
            !try_claim_raw_body_capture(key),
            "second claim same day must be latched out"
        );
        let next_day = key + 1;
        assert!(
            try_claim_raw_body_capture(next_day),
            "a new day key re-arms the capture"
        );
    }

    #[test]
    fn test_empty_diagnostics_aggregate_folds_max() {
        let target = 1_783_934_100_000_000_000;
        let mut d = EmptyDiagnostics::default();
        d.record(EmptyClass::NoRows);
        d.record(EmptyClass::Stale {
            rows_in_response: 10,
            last_candle_ist_nanos: target - 5 * NANOS_PER_MINUTE,
            serving_lag_secs: 300,
        });
        d.record(EmptyClass::Stale {
            rows_in_response: 21,
            last_candle_ist_nanos: target - 2 * NANOS_PER_MINUTE,
            serving_lag_secs: 120,
        });
        assert_eq!(d.no_rows, 1);
        assert_eq!(d.stale, 2);
        assert_eq!(d.max_rows_in_response, 21);
        assert_eq!(d.max_serving_lag_secs, 300);
        assert_eq!(
            d.latest_candle_ist_nanos,
            Some(target - 2 * NANOS_PER_MINUTE)
        );
    }

    /// Commandment-9 label for a candle instant on its own trading day —
    /// and the out-of-day clamp never panics.
    #[test]
    fn test_ist_nanos_minute_label() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date");
        let base = minute_open_ist_nanos(date, 0);
        let n = minute_open_ist_nanos(date, 10 * 3600 + 38 * 60);
        assert_eq!(ist_nanos_minute_label(n, base), "10:38 AM");
        // A previous-day instant against today's base clamps, never panics.
        assert_eq!(
            ist_nanos_minute_label(base - NANOS_PER_MINUTE, base),
            "12:00 AM"
        );
    }

    /// PROBE BYTE-EQUALITY FIXTURE: the probe's `crossverify_day_window`
    /// shape ([`spot_1m_day_request_body`]) serializes BYTE-IDENTICALLY to
    /// the 15:31 cross-verify's own request for the same index — both
    /// delegate to the shared [`intraday_request_body`] with the exact
    /// args the cross-verify passes for an IDX_I target (`"INDEX"` /
    /// `"IDX_I"` / D → D+1). Pinned against a literal fixture of the
    /// serialized JSON so a drift in EITHER builder (or in serde_json key
    /// ordering) fails loudly.
    #[test]
    fn test_probe_crossverify_request_byte_equality_fixture() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date");
        let spot = spot_1m_day_request_body("13", date).to_string();
        // The cross-verify call shape (cross_verify_1m_boot::compare_one_target
        // for an index target): intraday_request_body(sid, segment,
        // instrument, trading_date, trading_date.succ()).
        let next = date.succ_opt().expect("next day");
        let crossverify =
            intraday_request_body("13", SPOT_1M_REST_SEGMENT_IDX_I, "INDEX", date, next)
                .to_string();
        assert_eq!(
            spot, crossverify,
            "the probe/normal fetch body must stay byte-exact to the \
             cross-verify's request"
        );
        // Literal fixture (serde_json BTreeMap ordering — alphabetical).
        assert_eq!(
            spot,
            r#"{"exchangeSegment":"IDX_I","fromDate":"2026-07-14 00:00:00","instrument":"INDEX","interval":"1","oi":false,"securityId":"13","toDate":"2026-07-15 00:00:00"}"#
        );
    }

    /// The `same_day_to_now_window` probe body differs from the proven day
    /// window ONLY in `toDate` (the current IST wall-clock instant).
    #[test]
    fn test_probe_to_now_body_todate_is_now() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date");
        let now = 9 * 3600 + 17 * 60 + 42; // 09:17:42 IST
        let body = probe_to_now_body("13", date, now);
        assert_eq!(body["toDate"], "2026-07-14 09:17:42");
        // Every other field matches the proven day window byte-for-byte.
        let day = spot_1m_day_request_body("13", date);
        for key in [
            "securityId",
            "exchangeSegment",
            "instrument",
            "interval",
            "oi",
            "fromDate",
        ] {
            assert_eq!(body[key], day[key], "field {key} must match");
        }
        // Out-of-range seconds-of-day clamps inside the day, never panics.
        let clamped = probe_to_now_body("13", date, SECONDS_PER_DAY + 5);
        assert_eq!(clamped["toDate"], "2026-07-14 23:59:59");
    }

    /// Probe gating truth table: off → never; first fire → FirstFire once;
    /// second slot only at/after the configured instant; both done → None.
    #[test]
    fn test_should_run_probe_gating() {
        let second = 11 * 3600;
        let mut state = ProbeState::default();
        // Diagnostics off: never, regardless of state.
        assert_eq!(should_run_probe(false, &state, FIRST, second), None);
        // First fire after boot.
        assert_eq!(
            should_run_probe(true, &state, FIRST, second),
            Some(ProbeSlot::FirstFire)
        );
        state.first_done = true;
        // Before the second instant: nothing due.
        assert_eq!(should_run_probe(true, &state, 10 * 3600, second), None);
        // At/after the second instant: the scheduled probe.
        assert_eq!(
            should_run_probe(true, &state, second, second),
            Some(ProbeSlot::SecondScheduled)
        );
        assert_eq!(
            should_run_probe(true, &state, second + 300, second),
            Some(ProbeSlot::SecondScheduled)
        );
        state.second_done = true;
        assert_eq!(should_run_probe(true, &state, second + 300, second), None);
        // A mid-session boot AFTER the second instant: the first-fire probe
        // runs first, then the second follows on a later fire.
        let fresh = ProbeState::default();
        assert_eq!(
            should_run_probe(true, &fresh, 12 * 3600, second),
            Some(ProbeSlot::FirstFire)
        );
    }

    /// Room gate: a probe needs ≥ PROBE_ROOM_SECS before the next minute
    /// boundary (3 × 5 s request timeout + 2 × 300 ms spacing ≈ 15.6 s
    /// worst case fits; a late-in-minute wake defers).
    #[test]
    fn test_probe_has_room_boundary_math() {
        // 09:16:02 → next boundary 09:17:00, 58 s of room.
        assert!(probe_has_room(9 * 3600 + 16 * 60 + 2));
        // Exactly 20 s of room qualifies.
        assert!(probe_has_room(10 * 3600 + 40));
        // 19 s of room defers.
        assert!(!probe_has_room(10 * 3600 + 41));
        // 1 s before the boundary defers.
        assert!(!probe_has_room(10 * 3600 + 59));
    }

    /// Previous-trading-day walk: skips weekends + injected holidays,
    /// bounded, and honestly `None` on a never-open calendar.
    #[test]
    fn test_previous_trading_day_walks_weekends_and_holidays() {
        // 2026-07-14 is a Tuesday → Monday 2026-07-13.
        let tue = NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date");
        let mon = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let fri = NaiveDate::from_ymd_opt(2026, 7, 10).expect("valid date");
        use chrono::Datelike;
        let is_weekday =
            |d: NaiveDate| !matches!(d.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun);
        assert_eq!(previous_trading_day(tue, is_weekday), Some(mon));
        // Monday walks back across the weekend to Friday.
        assert_eq!(previous_trading_day(mon, is_weekday), Some(fri));
        // Monday with a Friday holiday → Thursday 2026-07-09.
        let thu = NaiveDate::from_ymd_opt(2026, 7, 9).expect("valid date");
        assert_eq!(
            previous_trading_day(mon, |d| is_weekday(d) && d != fri),
            Some(thu)
        );
        // A calendar that is never open: bounded walk returns None.
        assert_eq!(previous_trading_day(tue, |_| false), None);
    }

    /// Probe body summary: rows + first/last + target presence + lag, and
    /// the `-1` no-candles sentinel.
    #[test]
    fn test_summarize_probe_candles_shape_and_sentinel() {
        let target = 1_783_934_100_000_000_000;
        // No candles: everything empty, lag sentinel -1.
        let empty = summarize_probe_candles(&[], target);
        assert_eq!(
            empty,
            ProbeBodySummary {
                rows: 0,
                first_candle_ist_nanos: None,
                last_candle_ist_nanos: None,
                target_present: false,
                serving_lag_secs: -1,
            }
        );
        // Stale body: 2 rows ending 2 minutes behind the target.
        let c = [
            mk_candle(target - 3 * NANOS_PER_MINUTE),
            mk_candle(target - 2 * NANOS_PER_MINUTE),
        ];
        let s = summarize_probe_candles(&c, target);
        assert_eq!(s.rows, 2);
        assert_eq!(
            s.first_candle_ist_nanos,
            Some(target - 3 * NANOS_PER_MINUTE)
        );
        assert_eq!(s.last_candle_ist_nanos, Some(target - 2 * NANOS_PER_MINUTE));
        assert!(!s.target_present);
        assert_eq!(s.serving_lag_secs, 120);
        // Target present: lag 0, presence true.
        let c2 = [mk_candle(target)];
        let s2 = summarize_probe_candles(&c2, target);
        assert!(s2.target_present);
        assert_eq!(s2.serving_lag_secs, 0);
    }

    // ---- GAP-11 persist stamping (hold-then-stamp) --------------------------

    fn held_ok_row_fixture(minute_open_secs: i64) -> RestFetchAuditRow {
        RestFetchAuditRow {
            ts_ist_nanos: minute_open_secs * NANOS_PER_SEC,
            trading_date_ist_nanos: 0,
            feed: "dhan",
            leg: "spot_1m",
            security_id: 13,
            exchange_segment: "IDX_I",
            symbol: "NIFTY",
            attempts: 1,
            final_http_status: 0,
            fetch_latency_ms: -1,
            close_to_data_ms: 1_042,
            close_to_persist_ms: -1,
            rate_limited_count: 0,
            outcome: RestFetchOutcome::Ok,
            error_class: "none",
        }
    }

    #[test]
    fn test_close_to_persist_ms_for_math_and_clamp() {
        // 09:15 minute open → closes 09:16:00; flush ACK at 09:16:01.500.
        let open_secs: i64 = 9 * 3600 + 15 * 60;
        let nanos = open_secs * NANOS_PER_SEC;
        let close_ms = (open_secs + 60) * MILLIS_PER_SEC;
        assert_eq!(close_to_persist_ms_for(nanos, 0, close_ms + 1_500), 1_500);
        // A non-midnight trading_date base subtracts out.
        let date_nanos = 1_700_000_000 * NANOS_PER_SEC;
        assert_eq!(
            close_to_persist_ms_for(date_nanos + nanos, date_nanos, close_ms + 250),
            250
        );
        // Clock jitter can never fabricate a negative latency.
        assert_eq!(close_to_persist_ms_for(nanos, 0, close_ms - 10), 0);
    }

    #[test]
    fn test_stamp_held_ok_rows_flush_ok_stamps_each_row() {
        let open_a: i64 = 9 * 3600 + 15 * 60; // closes 09:16:00
        let open_b = open_a + 60; // closes 09:17:00
        let held = vec![held_ok_row_fixture(open_a), held_ok_row_fixture(open_b)];
        // Flush ACK 2 s after the LATER minute's close.
        let now_ms = (open_b + 60) * MILLIS_PER_SEC + 2_000;
        let stamped = stamp_held_ok_rows(held, true, 0, now_ms);
        assert_eq!(stamped.len(), 2);
        // Per-row stamping: the earlier minute closed 60 s before.
        assert_eq!(stamped[0].close_to_persist_ms, 62_000);
        assert_eq!(stamped[1].close_to_persist_ms, 2_000);
        // The measured close_to_data_ms + everything else is untouched, so
        // persist ≥ data for own-fire rows by construction (flush follows
        // the verdict on the same wall clock).
        assert!(
            stamped
                .iter()
                .all(|r| r.close_to_data_ms == 1_042 && r.outcome == RestFetchOutcome::Ok)
        );
    }

    #[test]
    fn test_stamp_held_ok_rows_flush_err_discards_all() {
        // Discard is MANDATORY: `outcome` is in the audit DEDUP key, so an
        // ok row appended after a failed flush would land ALONGSIDE the
        // flush_failed named-gap row and lie about the ok path.
        let held = vec![held_ok_row_fixture(9 * 3600 + 15 * 60)];
        assert!(stamp_held_ok_rows(held, false, 0, i64::MAX / 2).is_empty());
        // Empty hold is a no-op on either arm.
        assert!(stamp_held_ok_rows(Vec::new(), true, 0, 0).is_empty());
    }

    // ---- GAP-11 review HIGH (2026-07-14): real rate-limit facts -------------

    #[test]
    fn test_dhan_failed_audit_class_terminal_429_maps_rate_limited() {
        // The Groww classification rule mirrored: the LAST attempt's 429
        // decides the terminal verdict (a 429-exhausted ladder must never
        // read `error` while the digest sums 0 rate-limit hits).
        let forensics = DhanLadderForensics {
            attempts: 5,
            rate_limited_count: 3,
            terminal_rate_limited: true,
        };
        assert_eq!(
            dhan_failed_audit_class(&forensics),
            (RestFetchOutcome::RateLimited, "rate_limited")
        );
    }

    #[test]
    fn test_dhan_failed_audit_class_non_429_maps_error() {
        // A mid-ladder 429 followed by a non-429 terminal failure stays
        // `error` (last status decides — Groww semantics), but the row
        // still carries the real rate_limited_count.
        let forensics = DhanLadderForensics {
            attempts: 5,
            rate_limited_count: 2,
            terminal_rate_limited: false,
        };
        assert_eq!(
            dhan_failed_audit_class(&forensics),
            (RestFetchOutcome::Error, "error")
        );
        // The budget-overrun sentinel arm (attempts 0) is also `error`.
        assert_eq!(
            dhan_failed_audit_class(&DhanLadderForensics::default()),
            (RestFetchOutcome::Error, "error")
        );
    }

    #[test]
    fn test_build_dhan_fetch_audit_row_carries_real_rate_limit_facts() {
        // Real attempts + 429 count land on the row; status/latency stay
        // the documented 0/-1 sentinels (FetchFailure carries neither).
        let row = build_dhan_fetch_audit_row(
            1_000,
            0,
            13,
            "NIFTY",
            5,
            3,
            RestFetchOutcome::RateLimited,
            -1,
            "rate_limited",
        );
        assert_eq!(row.attempts, 5);
        assert_eq!(row.rate_limited_count, 3);
        assert_eq!(row.outcome, RestFetchOutcome::RateLimited);
        assert_eq!(row.error_class, "rate_limited");
        assert_eq!(row.final_http_status, 0);
        assert_eq!(row.fetch_latency_ms, -1);
        assert_eq!(row.close_to_persist_ms, -1);
    }

    #[test]
    fn test_build_dhan_fetch_audit_row_skipped_boundary_shape() {
        // GAP-11 review MEDIUM 1 (2026-07-14): the Dhan spot leg's missed
        // boundaries land `outcome=skipped` / `boundary_skipped` rows with
        // the no-request sentinels (0 attempts / 0 429s / -1 latency) —
        // the Groww spot / Dhan chain shape.
        let row = build_dhan_fetch_audit_row(
            2_000,
            0,
            25,
            "BANKNIFTY",
            0,
            0,
            RestFetchOutcome::Skipped,
            -1,
            "boundary_skipped",
        );
        assert_eq!(row.outcome, RestFetchOutcome::Skipped);
        assert_eq!(row.error_class, "boundary_skipped");
        assert_eq!(row.attempts, 0);
        assert_eq!(row.rate_limited_count, 0);
        assert_eq!(row.close_to_data_ms, -1);
    }
}
