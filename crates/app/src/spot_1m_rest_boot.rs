//! Per-minute spot 1m REST pipeline — PR-2, the SPOT half (operator grant
//! 2026-07-12; runbook `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! Every trading-day minute close in session — the 09:15 candle closes at
//! 09:16:00 IST; the last (15:29) candle closes at 15:30:00 — this task
//! wakes shortly after the boundary and fetches THAT just-closed minute's
//! official 1m OHLCV for the 3 IDX_I spot indices (NIFTY 13, BANKNIFTY 25,
//! SENSEX 51) via Dhan `POST /v2/charts/intraday` (interval `"1"`), then
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
//!    fires at 15:33:30 — NOT 15:31 — so its ≤3 requests clear the 15:31
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
//! One fire = 3 concurrent requests (one per index), plus at most 4 ladder
//! re-polls per index spread over ~6 s — worst case 3 requests at any one
//! ladder instant, comfortably inside the 5/sec Data-API budget and the
//! prev-day fetcher's 4/sec headroom assumption (Q2/Q3 2026-06-23 lesson).
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

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    DHAN_CHARTS_INTRADAY_PATH, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
    SPOT_1M_REST_429_EXTRA_BACKOFF_MS, SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD,
    SPOT_1M_REST_FIRE_DELAY_MS, SPOT_1M_REST_FIRE_STALE_GRACE_SECS,
    SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST, SPOT_1M_REST_INDICES,
    SPOT_1M_REST_LADDER_JITTER_SLOTS, SPOT_1M_REST_LADDER_JITTER_STEP_MS,
    SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST, SPOT_1M_REST_MAX_BODY_BYTES,
    SPOT_1M_REST_REQUEST_TIMEOUT_SECS, SPOT_1M_REST_RETRY_OFFSETS_MS, SPOT_1M_REST_SID_BUDGET_SECS,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::{capture_rest_error_body, redact_url_params};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::types::SecurityId;
use tickvault_common::url_join::join_api_url;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::disk_health_watcher::classify_join_exit;
use tickvault_storage::spot_1m_rest_persistence::{
    SPOT_1M_REST_SEGMENT_IDX_I, Spot1mRestRow, Spot1mRestWriter, ensure_spot_1m_rest_table,
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
/// full-day body. Malformed / short / length-mismatched bodies parse to an
/// empty set (the reused panic-free columnar parser) and therefore yield
/// `(None, None)`. Pure.
#[must_use]
pub fn parse_intraday_columnar_for_minutes(
    body: &str,
    target_minute_ist_nanos: i64,
    backfill_minute_ist_nanos: Option<i64>,
) -> (Option<MinuteCandle>, Option<MinuteCandle>) {
    let candles = parse_intraday_1m_candles(body);
    (
        select_minute_candle(&candles, target_minute_ist_nanos),
        backfill_minute_ist_nanos.and_then(|b| select_minute_candle(&candles, b)),
    )
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
/// to 15:33:30 so the sweep's ≤3 requests land AFTER the 15:31 bulk
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
/// 150 / 300 ms for the 3 spot SIDs. The slot (a SID's fixed position in
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
    /// appeared — counted `outcome="empty"`, included in the failure edge.
    Empty {
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
) -> Result<String, FetchFailure> {
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
        let error_body = read_body_capped(resp).await.unwrap_or_default();
        return Err(FetchFailure {
            rate_limited: status == reqwest::StatusCode::TOO_MANY_REQUESTS,
            msg: format!(
                "http {status} url={} body={}",
                redact_url_params(url),
                capture_rest_error_body(&error_body)
            ),
        });
    }
    read_body_capped(resp).await.map_err(|msg| FetchFailure {
        rate_limited: false,
        msg,
    })
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
) -> SidFetchOutcome {
    let deltas = retry_sleep_deltas_ms();
    let mut last_error: Option<String> = None;
    let mut prev_rate_limited = false;
    // Sticky: the FIRST 2xx body carrying the due backfill minute wins —
    // preserved across rungs and across a failing own-minute verdict.
    let mut backfill_found: Option<MinuteCandle> = None;
    for attempt in 0..=deltas.len() {
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
        match result {
            Ok(body_text) => {
                let (target, backfill) = parse_intraday_columnar_for_minutes(
                    &body_text,
                    target_minute_ist_nanos,
                    backfill_minute_ist_nanos,
                );
                if backfill_found.is_none() {
                    backfill_found = backfill;
                }
                if let Some(candle) = target {
                    let close_to_data_ms =
                        (ist_millis_of_day_now() - minute_close_ms_of_day).max(0);
                    return SidFetchOutcome::Found {
                        candle,
                        close_to_data_ms,
                        backfill_candle: backfill_found,
                    };
                }
                // 2xx without the target minute — the seal may not have
                // landed yet; the next ladder rung re-polls.
                last_error = None;
                prev_rate_limited = false;
            }
            Err(failure) => {
                if failure.rate_limited {
                    // DH-904 class: counted; the NEXT rung waits the extra
                    // bounded backoff; NEVER retried past the ladder.
                    metrics::counter!("tv_spot1m_rate_limited_total").increment(1);
                }
                prev_rate_limited = failure.rate_limited;
                last_error = Some(failure.msg);
            }
        }
    }
    match last_error {
        Some(reason) => SidFetchOutcome::Failed {
            reason,
            backfill_candle: backfill_found,
        },
        None => SidFetchOutcome::Empty {
            backfill_candle: backfill_found,
        },
    }
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
) -> SidFetchOutcome {
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
        ),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            metrics::counter!("tv_spot1m_sid_budget_exceeded_total").increment(1);
            SidFetchOutcome::Failed {
                reason: format!(
                    "ladder budget exceeded ({SPOT_1M_REST_SID_BUDGET_SECS}s) — peer stalling"
                ),
                backfill_candle: None,
            }
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
    let mut edge = FailureEdge::default();
    // 2026-07-13 backfill sweep: per-SID latest PERSISTED minute — drives
    // the previous-minute repair on every fire.
    let mut tracker = PersistTracker::default();
    // H1 (2026-07-12): the last boundary actually HANDLED (fired or
    // skipped-stale) — the next fire is always STRICTLY after it, so a
    // fast-completing fire can never re-fire the same boundary second.
    let mut last_fired: Option<u32> = None;
    info!(
        indices = SPOT_1M_REST_INDICES.len(),
        "spot_1m_rest: per-minute fetch loop armed (fires each minute close \
         09:16:00–15:30:00 IST, ~0.3–1.3s after the boundary)"
    );

    loop {
        // Audit Rule 3: re-read the wall clock + trading-day verdict EVERY
        // iteration (a suspend can cross midnight and stale the verdict).
        if !params.calendar.is_trading_day_today() {
            info!("spot_1m_rest: no longer a trading day — exiting");
            return;
        }
        let now = ist_secs_of_day_now();
        let Some(fire) = next_fire_after(now, last_fired) else {
            info!(
                "spot_1m_rest: past 15:30 IST — today's minute fires complete; \
                 running the one bounded post-session sweep"
            );
            run_post_session_sweep(&client, &url, &params, &mut writer, &mut tracker).await;
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
            record_skipped_boundaries(&params, &mut edge, missed, fire);
            // Advance past everything that already elapsed so the next
            // iteration never re-selects a long-gone boundary.
            if woke > fire {
                last_fired = Some((woke.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
            }
            continue;
        }

        fire_one_minute(
            &params,
            &client,
            &url,
            &mut writer,
            &mut edge,
            &mut tracker,
            fire,
        )
        .await;
        // PR-3 sequencing: tell the option-chain leg this minute's spot
        // fire is DONE (success or failure — the chain must never block on
        // a failing spot leg). `send_replace` never errors.
        if let Some(tx) = &params.minute_done_tx {
            tx.send_replace(Some(fire));
        }
        last_fired = Some(fire);
        // H2 overrun accounting: boundaries that fully elapsed DURING the
        // fire can never be fetched — count them loudly + feed the edge.
        let after = ist_secs_of_day_now();
        let missed = count_missed_boundaries(fire, after.saturating_sub(1));
        record_skipped_boundaries(&params, &mut edge, missed, fire);
        if missed > 0 {
            last_fired = Some((after.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
        }
    }
}

/// Loud accounting for minute boundaries that elapsed UNFETCHED (fire
/// overrun / suspend / clock step): counter + ONE coalesced coded log +
/// each missed minute feeds the failure edge so a sustained-overrun outage
/// still reaches the SPOT1M-01 escalation page (2026-07-12 H2 fix).
fn record_skipped_boundaries(
    params: &Spot1mRestTaskParams,
    edge: &mut FailureEdge,
    skipped: u32,
    context_secs_of_day: u32,
) {
    if skipped == 0 {
        return;
    }
    metrics::counter!("tv_spot1m_boundary_skipped_total").increment(u64::from(skipped));
    let around = format_minute_ist_12h(context_secs_of_day);
    error!(
        code = ErrorCode::Spot1m01FetchDegraded.code_str(),
        stage = "boundary_skipped",
        skipped,
        around = %around,
        "SPOT1M-01: minute boundaries elapsed unfetched (fire overrun / \
         suspend) — those minutes stay absent (re-fetchable via backfill)"
    );
    for _ in 0..skipped {
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

/// One minute-close fire: 3 concurrent ladder fetches (each carrying the
/// full-day proven window) → persist the target minute AND the
/// previous-minute backfill when due → counters → edge accounting.
/// Failures are coalesced to ONE coded log per fire. Edge honesty: the
/// verdict is the OWN target minute's fetch + persist — a backfill hit
/// never counts as this fire's `ok` (a minute that lands only via
/// next-fire backfill was still that fire's failure).
async fn fire_one_minute(
    params: &Spot1mRestTaskParams,
    client: &reqwest::Client,
    url: &str,
    writer: &mut Spot1mRestWriter,
    edge: &mut FailureEdge,
    tracker: &mut PersistTracker,
    fire_secs_of_day: u32,
) {
    let minute_open_secs = fire_secs_of_day.saturating_sub(60);
    let minute_label = format_minute_ist_12h(minute_open_secs);
    let trading_date = today_ist();

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
    // M1 (2026-07-12): a minute is fully-OK for the edge ONLY when the
    // fetch succeeded AND append+flush confirmed — a day-long QuestDB
    // outage must eventually page via the SAME Spot1mFetchDegraded path.
    let mut persist_failed = false;
    let mut sample_failure: Option<String> = None;
    // Minutes appended this fire, committed to the tracker ONLY after the
    // flush confirms (a failed flush discards the buffer — never a false
    // watermark advance).
    let mut staged: Vec<(SecurityId, i64)> = Vec::new();

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
            // shift so the 3 concurrent ladders never re-poll in lockstep.
            let jitter_ms = ladder_jitter_ms(slot);
            join_set.spawn(async move {
                let outcome = fetch_minute_bounded(
                    &client,
                    &url,
                    &jwt,
                    &body,
                    target_nanos,
                    backfill_nanos,
                    minute_close_ms,
                    jitter_ms,
                )
                .await;
                (security_id, symbol, outcome)
            });
        }
        let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
        while let Some(joined) = join_set.join_next().await {
            let Ok((security_id, symbol, outcome)) = joined else {
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
            let (own_outcome, backfill_candle) = match outcome {
                SidFetchOutcome::Found {
                    candle,
                    close_to_data_ms,
                    backfill_candle,
                } => (Some((candle, close_to_data_ms)), backfill_candle),
                SidFetchOutcome::Empty { backfill_candle } => {
                    empty_count = empty_count.saturating_add(1);
                    metrics::counter!("tv_spot1m_fetch_total", "outcome" => "empty").increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!(
                            "sid {security_id}: 2xx but the minute's candle never \
                             appeared within the re-poll ladder"
                        ));
                    }
                    (None, backfill_candle)
                }
                SidFetchOutcome::Failed {
                    reason,
                    backfill_candle,
                } => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_spot1m_fetch_total", "outcome" => "error").increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!("sid {security_id}: {reason}"));
                    }
                    (None, backfill_candle)
                }
            };
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
                } else {
                    staged.push((security_id, candle.minute_ts_ist_nanos));
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
                } else {
                    metrics::counter!("tv_spot1m_backfilled_total").increment(1);
                    info!(
                        security_id,
                        symbol,
                        backfill_close_to_data_ms,
                        "spot_1m_rest: previous minute backfilled from this \
                         fire's full-day response (DEDUP-idempotent)"
                    );
                    staged.push((security_id, backfill.minute_ts_ist_nanos));
                }
            }
        }
        if let Err(err) = writer.flush() {
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
        } else {
            // Flush confirmed — advance the per-SID persisted watermark.
            for (security_id, minute_nanos) in staged {
                tracker.commit(security_id, minute_nanos);
            }
        }
    } else {
        // No token at fire time — REST cannot succeed; the whole minute is
        // a full miss (counted per SID for honest rate math).
        error_count = SPOT_1M_REST_INDICES.len();
        sample_failure = Some("no access token available at fire time".to_string());
        for _ in 0..error_count {
            metrics::counter!("tv_spot1m_fetch_total", "outcome" => "error").increment(1);
        }
    }

    record_minute_verdict(
        params,
        edge,
        &minute_label,
        ok_count,
        error_count,
        empty_count,
        persist_failed,
        sample_failure.as_deref(),
    );
}

/// M1 (2026-07-12): the edge's "fully failed" verdict for one fired
/// minute — no SID succeeded, OR the persist leg (append/flush) failed.
/// A fetched-but-never-persisted minute is NOT ok. Pure.
#[must_use]
pub fn minute_fully_failed(ok_count: usize, persist_failed: bool) -> bool {
    ok_count == 0 || persist_failed
}

/// ONE bounded post-session repair sweep (~15:33:30 IST, single fire —
/// M1, review 2026-07-13; moved off 15:31 the same day so it clears the
/// bulk cross-verify's observed 15:31–15:33 429 burst window): once the
/// session is final, re-fetch the proven
/// day window ONCE per SID that still has session minutes above its
/// persisted watermark (the 15:29 candle after a vendor-late seal, a
/// flush-failed backfill row, any tail gap the per-minute one-minute
/// lookback could not reach) and persist every one found. Bounded: ≤3
/// requests total, ≤375 minutes/SID, DEDUP-idempotent re-appends; loud
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

    let mut swept: u64 = 0;
    let mut still_missing: u64 = 0;
    let mut staged: Vec<(SecurityId, i64)> = Vec::new();
    let mut persist_failed = false;
    for (security_id, symbol) in SPOT_1M_REST_INDICES {
        let missing = sweep_missing_minutes(
            tracker.last_persisted(security_id),
            session_first,
            session_last,
        );
        if missing.is_empty() {
            continue;
        }
        let body = spot_1m_day_request_body(&security_id.to_string(), trading_date);
        let candles = match spot_1m_fetch_once(client, url, jwt.expose_secret(), &body).await {
            Ok(body_text) => parse_intraday_1m_candles(&body_text),
            Err(failure) => {
                if failure.rate_limited {
                    metrics::counter!("tv_spot1m_rate_limited_total").increment(1);
                }
                error!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage = "sweep_failed",
                    security_id,
                    reason = %failure.msg,
                    "SPOT1M-01: post-session sweep fetch failed for this SID"
                );
                still_missing = still_missing.saturating_add(missing.len() as u64);
                continue;
            }
        };
        let mut found_for_sid: u64 = 0;
        for minute_nanos in &missing {
            let Some(candle) = select_minute_candle(&candles, *minute_nanos) else {
                still_missing = still_missing.saturating_add(1);
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
                persist_failed = true;
                metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "append")
                    .increment(1);
                error!(
                    code = ErrorCode::Spot1m02PersistFailed.code_str(),
                    stage = "append",
                    security_id,
                    ?err,
                    "SPOT1M-02: post-session sweep row append failed"
                );
                still_missing = still_missing.saturating_add(1);
            } else {
                found_for_sid = found_for_sid.saturating_add(1);
                staged.push((security_id, *minute_nanos));
            }
        }
        swept = swept.saturating_add(found_for_sid);
    }
    if let Err(err) = writer.flush() {
        persist_failed = true;
        metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "flush").increment(1);
        error!(
            code = ErrorCode::Spot1m02PersistFailed.code_str(),
            stage = "flush",
            ?err,
            "SPOT1M-02: post-session sweep ILP flush failed — pending swept \
             rows discarded (poisoned-buffer defense)"
        );
        still_missing = still_missing.saturating_add(swept);
        swept = 0;
    } else {
        for (security_id, minute_nanos) in staged {
            tracker.commit(security_id, minute_nanos);
        }
    }
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
                // Coalesced ONCE per fire (never per retry); log-sink-only —
                // sub-edge failures never page (the escalation arm does).
                error!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage = "minute_failed",
                    minute = minute_label,
                    ok = ok_count,
                    errors = error_count,
                    empty = empty_count,
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
        let (candle, backfill) = parse_intraday_columnar_for_minutes(&body, target, None);
        let candle = candle.expect("target found");
        assert!(backfill.is_none(), "no backfill requested → none returned");
        assert_eq!(candle.minute_ts_ist_nanos, target);
        assert_eq!(candle.open, 100.0);
        assert_eq!(candle.close, 100.5);
        // A target NOT in the body → None (the "empty" arm).
        let (missing, _) =
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
        let (t, b) = parse_intraday_columnar_for_minutes(&body_prev_only, target, Some(prev));
        assert!(t.is_none(), "target minute not sealed yet");
        assert_eq!(
            b.expect("backfill found").minute_ts_ist_nanos,
            prev,
            "previous minute must backfill even when the target is absent"
        );
        // Body carries BOTH → both returned.
        let body_both = format!(
            r#"{{"open":[100.0,200.0],"high":[101.0,201.0],"low":[99.0,199.0],
                "close":[100.5,200.5],"volume":[0,0],
                "timestamp":[{utc_prev},{utc_target}]}}"#,
            utc_target = utc_prev + 60
        );
        let (t, b) = parse_intraday_columnar_for_minutes(&body_both, target, Some(prev));
        assert_eq!(t.expect("target").minute_ts_ist_nanos, target);
        assert_eq!(b.expect("backfill").minute_ts_ist_nanos, prev);
    }

    #[test]
    fn test_parse_for_minutes_malformed_short_and_mismatched_bodies_are_none() {
        let target = 1_770_000_900_000_000_000;
        let bf = Some(target - NANOS_PER_MINUTE);
        // Malformed JSON.
        assert_eq!(
            parse_intraday_columnar_for_minutes("not json", target, bf),
            (None, None)
        );
        // Missing arrays.
        assert_eq!(
            parse_intraday_columnar_for_minutes("{}", target, bf),
            (None, None)
        );
        // Empty arrays.
        let empty = r#"{"open":[],"high":[],"low":[],"close":[],"volume":[],"timestamp":[]}"#;
        assert_eq!(
            parse_intraday_columnar_for_minutes(empty, target, bf),
            (None, None)
        );
        // Length-mismatched parallel arrays.
        let mismatched = r#"{"open":[1.0,2.0],"high":[1.0],"low":[1.0],
            "close":[1.0],"volume":[0],"timestamp":[1752118500]}"#;
        assert_eq!(
            parse_intraday_columnar_for_minutes(mismatched, target, bf),
            (None, None)
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
    /// pinned index array (0 / 150 / 300 ms), bounded by
    /// `(slots - 1) × step`, wrapping for defensive out-of-range slots.
    /// Slot-based (not `sid % 3`) because the pinned SIDs 13/25/51 give
    /// 1/1/0 under `% 3` — two ladders would still re-poll in lockstep.
    #[test]
    fn test_ladder_jitter_ms_bounds_and_decorrelation() {
        let jitters: Vec<u64> = (0..SPOT_1M_REST_INDICES.len())
            .map(ladder_jitter_ms)
            .collect();
        assert_eq!(jitters, vec![0, 150, 300], "one distinct shift per SID");
        let max = (SPOT_1M_REST_LADDER_JITTER_SLOTS - 1) * SPOT_1M_REST_LADDER_JITTER_STEP_MS;
        for j in &jitters {
            assert!(*j <= max, "jitter bounded by (slots-1) x step");
        }
        // All three schedules are pairwise distinct — no lockstep re-polls.
        assert!(jitters[0] != jitters[1] && jitters[1] != jitters[2] && jitters[0] != jitters[2]);
        // Defensive wrap for a hypothetical 4th slot.
        assert_eq!(ladder_jitter_ms(3), 0);
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
}
