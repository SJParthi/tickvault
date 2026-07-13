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

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate};
use secrecy::ExposeSecret;
use serde_json::json;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    DHAN_CANDLE_INTERVAL_1MIN, DHAN_CHARTS_INTRADAY_PATH, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
    SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD, SPOT_1M_REST_FIRE_DELAY_MS,
    SPOT_1M_REST_FIRE_STALE_GRACE_SECS, SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST,
    SPOT_1M_REST_INDICES, SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST, SPOT_1M_REST_MAX_BODY_BYTES,
    SPOT_1M_REST_REQUEST_TIMEOUT_SECS, SPOT_1M_REST_RETRY_OFFSETS_MS, SPOT_1M_REST_SID_BUDGET_SECS,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::{capture_rest_error_body, redact_url_params};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::url_join::join_api_url;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::disk_health_watcher::classify_join_exit;
use tickvault_storage::spot_1m_rest_persistence::{
    SPOT_1M_REST_SEGMENT_IDX_I, Spot1mRestRow, Spot1mRestWriter, ensure_spot_1m_rest_table,
};

use crate::cross_verify_1m_boot::{MinuteCandle, parse_intraday_1m_candles};

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

/// `fromDate`/`toDate` datetime strings (`"YYYY-MM-DD HH:MM:SS"`) for ONE
/// minute: `from` = the minute open, `to` = open + 60 s. Dhan may
/// over-deliver beyond `toDate` (community-reported intraday behaviour) —
/// the consumer filters client-side to the exact minute regardless. `None`
/// only on an impossible seconds-of-day (defensive). Pure.
#[must_use]
pub fn minute_window_strings(
    trading_date: NaiveDate,
    minute_open_secs_of_day: u32,
) -> Option<(String, String)> {
    let open = trading_date.and_hms_opt(
        minute_open_secs_of_day / 3600,
        (minute_open_secs_of_day % 3600) / 60,
        minute_open_secs_of_day % 60,
    )?;
    let close = open + ChronoDuration::seconds(60);
    Some((
        open.format("%Y-%m-%d %H:%M:%S").to_string(),
        close.format("%Y-%m-%d %H:%M:%S").to_string(),
    ))
}

/// The `/v2/charts/intraday` request body for ONE index and ONE minute
/// window. `securityId` is a STRING; `interval` is the STRING `"1"`
/// (historical-data.md rules 4-5). Pure.
#[must_use]
pub fn spot_1m_request_body(
    security_id: &str,
    from_datetime: &str,
    to_datetime: &str,
) -> serde_json::Value {
    json!({
        "securityId": security_id,
        "exchangeSegment": SPOT_1M_REST_SEGMENT_IDX_I,
        "instrument": SPOT_1M_INSTRUMENT_INDEX,
        "interval": DHAN_CANDLE_INTERVAL_1MIN,
        "oi": false,
        "fromDate": from_datetime,
        "toDate": to_datetime,
    })
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

/// Parse a Dhan intraday columnar body and pick the target minute's candle.
/// Malformed / short / length-mismatched bodies parse to an empty set (the
/// reused panic-free columnar parser) and therefore yield `None`. Pure.
#[must_use]
pub fn parse_intraday_columnar_for_minute(
    body: &str,
    target_minute_ist_nanos: i64,
) -> Option<MinuteCandle> {
    select_minute_candle(&parse_intraday_1m_candles(body), target_minute_ist_nanos)
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

/// One index's per-minute fetch verdict after the bounded ladder.
#[derive(Clone, Debug, PartialEq)]
enum SidFetchOutcome {
    /// The target minute's candle was retrieved (`close_to_data_ms` =
    /// minute close → retrieval wall-clock latency).
    Found {
        candle: MinuteCandle,
        close_to_data_ms: i64,
    },
    /// Every attempt got a parseable 2xx but the target minute never
    /// appeared — counted `outcome="empty"`, included in the failure edge.
    Empty,
    /// The last attempt (transport / non-2xx) failure, bounded + redacted.
    Failed(String),
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
/// fire instant, then re-polls at [`SPOT_1M_REST_RETRY_OFFSETS_MS`] until
/// the target minute's candle appears — after the last offset the minute
/// is `Empty`/`Failed`, never an unbounded in-minute retry (DH-904/429
/// counts via the REAL `StatusCode` + falls out of the ladder like any
/// other error). The WHOLE ladder is additionally hard-bounded by
/// [`SPOT_1M_REST_SID_BUDGET_SECS`] in [`fetch_minute_bounded`] so no
/// stall combination can overrun the minute (2026-07-12 H2 fix).
async fn fetch_minute_with_ladder(
    client: &reqwest::Client,
    url: &str,
    jwt: &secrecy::SecretString,
    body: &serde_json::Value,
    target_minute_ist_nanos: i64,
    minute_close_ms_of_day: i64,
) -> SidFetchOutcome {
    let deltas = retry_sleep_deltas_ms();
    let mut last_error: Option<String> = None;
    for attempt in 0..=deltas.len() {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(deltas[attempt - 1])).await;
        }
        let started = std::time::Instant::now();
        let result = spot_1m_fetch_once(client, url, jwt.expose_secret(), body).await;
        metrics::histogram!("tv_spot1m_fetch_duration_ms")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        match result {
            Ok(body_text) => {
                if let Some(candle) =
                    parse_intraday_columnar_for_minute(&body_text, target_minute_ist_nanos)
                {
                    let close_to_data_ms =
                        (ist_millis_of_day_now() - minute_close_ms_of_day).max(0);
                    return SidFetchOutcome::Found {
                        candle,
                        close_to_data_ms,
                    };
                }
                // 2xx without the target minute — the seal may not have
                // landed yet; the next ladder rung re-polls.
                last_error = None;
            }
            Err(failure) => {
                if failure.rate_limited {
                    // DH-904 class: counted; NEVER retried past the ladder.
                    metrics::counter!("tv_spot1m_rate_limited_total").increment(1);
                }
                last_error = Some(failure.msg);
            }
        }
    }
    match last_error {
        Some(reason) => SidFetchOutcome::Failed(reason),
        None => SidFetchOutcome::Empty,
    }
}

/// The ladder wrapped in the HARD per-SID wall-clock budget
/// ([`SPOT_1M_REST_SID_BUDGET_SECS`]): a budget overrun is that SID's
/// failure for the minute — the fire can never overrun the next boundary
/// (the const-asserts in `constants.rs` pin the budget < 60 s including
/// the per-request timeout).
async fn fetch_minute_bounded(
    client: &reqwest::Client,
    url: &str,
    jwt: &secrecy::SecretString,
    body: &serde_json::Value,
    target_minute_ist_nanos: i64,
    minute_close_ms_of_day: i64,
) -> SidFetchOutcome {
    match tokio::time::timeout(
        Duration::from_secs(SPOT_1M_REST_SID_BUDGET_SECS),
        fetch_minute_with_ladder(
            client,
            url,
            jwt,
            body,
            target_minute_ist_nanos,
            minute_close_ms_of_day,
        ),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            metrics::counter!("tv_spot1m_sid_budget_exceeded_total").increment(1);
            SidFetchOutcome::Failed(format!(
                "ladder budget exceeded ({SPOT_1M_REST_SID_BUDGET_SECS}s) — peer stalling"
            ))
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
            info!("spot_1m_rest: past 15:30 IST — today's minute fires complete");
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

        fire_one_minute(&params, &client, &url, &mut writer, &mut edge, fire).await;
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

/// One minute-close fire: 3 concurrent ladder fetches → persist → counters
/// → edge accounting. Failures are coalesced to ONE coded log per fire.
async fn fire_one_minute(
    params: &Spot1mRestTaskParams,
    client: &reqwest::Client,
    url: &str,
    writer: &mut Spot1mRestWriter,
    edge: &mut FailureEdge,
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

    if let Some(jwt) = jwt {
        let Some((from_dt, to_dt)) = minute_window_strings(trading_date, minute_open_secs) else {
            // Defensive: impossible seconds-of-day. Counted as a full miss.
            error_count = SPOT_1M_REST_INDICES.len();
            sample_failure = Some("internal: minute window build failed".to_string());
            for _ in 0..error_count {
                metrics::counter!("tv_spot1m_fetch_total", "outcome" => "error").increment(1);
            }
            record_minute_verdict(
                params,
                edge,
                &minute_label,
                0,
                error_count,
                0,
                false,
                sample_failure.as_deref(),
            );
            return;
        };
        let target_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
        let minute_close_ms = i64::from(fire_secs_of_day).saturating_mul(MILLIS_PER_SEC);

        let mut join_set = tokio::task::JoinSet::new();
        for (security_id, symbol) in SPOT_1M_REST_INDICES {
            let client = client.clone();
            let url = url.to_string();
            let jwt = jwt.clone();
            let body = spot_1m_request_body(&security_id.to_string(), &from_dt, &to_dt);
            join_set.spawn(async move {
                let outcome =
                    fetch_minute_bounded(&client, &url, &jwt, &body, target_nanos, minute_close_ms)
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
            match outcome {
                SidFetchOutcome::Found {
                    candle,
                    close_to_data_ms,
                } => {
                    ok_count = ok_count.saturating_add(1);
                    metrics::counter!("tv_spot1m_fetch_total", "outcome" => "ok").increment(1);
                    #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
                    metrics::histogram!("tv_spot1m_close_to_data_ms")
                        .record(close_to_data_ms as f64);
                    let row = Spot1mRestRow {
                        ts_ist_nanos: candle.minute_ts_ist_nanos,
                        trading_date_ist_nanos: trading_date_nanos,
                        // Unreachable for the pinned 13/25/51 set; a
                        // hypothetical overflow is LOUD + a visible
                        // sentinel, never a silent sid=0 (review LOW).
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
                    };
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
                    }
                }
                SidFetchOutcome::Empty => {
                    empty_count = empty_count.saturating_add(1);
                    metrics::counter!("tv_spot1m_fetch_total", "outcome" => "empty").increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!(
                            "sid {security_id}: 2xx but the minute's candle never \
                             appeared within the re-poll ladder"
                        ));
                    }
                }
                SidFetchOutcome::Failed(reason) => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_spot1m_fetch_total", "outcome" => "error").increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!("sid {security_id}: {reason}"));
                    }
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

    // ---- minute_window_strings / request body ------------------------------

    #[test]
    fn test_minute_window_strings_open_plus_60s() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 10).expect("valid date");
        let (from, to) = minute_window_strings(date, 9 * 3600 + 15 * 60).expect("valid window");
        assert_eq!(from, "2026-07-10 09:15:00");
        assert_eq!(to, "2026-07-10 09:16:00");
        // The last session minute: 15:29 → 15:30.
        let (from, to) = minute_window_strings(date, 15 * 3600 + 29 * 60).expect("valid window");
        assert_eq!(from, "2026-07-10 15:29:00");
        assert_eq!(to, "2026-07-10 15:30:00");
    }

    #[test]
    fn test_spot_1m_request_body_shape_string_security_id() {
        let body = spot_1m_request_body("13", "2026-07-10 09:15:00", "2026-07-10 09:16:00");
        // securityId is a STRING (orders.md rule 4 class), interval "1".
        assert_eq!(body["securityId"], "13");
        assert_eq!(body["exchangeSegment"], "IDX_I");
        assert_eq!(body["instrument"], "INDEX");
        assert_eq!(body["interval"], "1");
        assert_eq!(body["oi"], false);
        assert_eq!(body["fromDate"], "2026-07-10 09:15:00");
        assert_eq!(body["toDate"], "2026-07-10 09:16:00");
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

    /// UTC→IST (+19800) conversion: a Dhan intraday timestamp of UTC
    /// 03:45:00 is IST 09:15:00 — the parsed candle's minute bucket must
    /// equal our IST-as-epoch target for the same wall-clock minute.
    #[test]
    fn test_parse_intraday_columnar_for_minute_utc_to_ist_and_filter() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 10).expect("valid date");
        let open_secs: u32 = 9 * 3600 + 15 * 60;
        let target = minute_open_ist_nanos(date, open_secs);
        // The UTC epoch for IST 2026-07-10 09:15:00 = (IST-as-epoch) - 19800.
        let utc_ts = target / 1_000_000_000 - 19_800;
        // Over-delivering body: the target minute AND the next minute (the
        // community-reported toDate over-delivery) — client-side filter
        // must pick exactly the target.
        let body = format!(
            r#"{{"open":[100.0,200.0],"high":[101.0,201.0],"low":[99.0,199.0],
                "close":[100.5,200.5],"volume":[0,0],"timestamp":[{utc_ts},{next}]}}"#,
            next = utc_ts + 60
        );
        let candle = parse_intraday_columnar_for_minute(&body, target).expect("target found");
        assert_eq!(candle.minute_ts_ist_nanos, target);
        assert_eq!(candle.open, 100.0);
        assert_eq!(candle.close, 100.5);
        // A target NOT in the body → None (the "empty" arm).
        assert!(parse_intraday_columnar_for_minute(&body, target + 120 * 1_000_000_000).is_none());
    }

    #[test]
    fn test_parse_for_minute_malformed_short_and_mismatched_bodies_are_none() {
        let target = 1_770_000_900_000_000_000;
        // Malformed JSON.
        assert!(parse_intraday_columnar_for_minute("not json", target).is_none());
        // Missing arrays.
        assert!(parse_intraday_columnar_for_minute("{}", target).is_none());
        // Empty arrays.
        let empty = r#"{"open":[],"high":[],"low":[],"close":[],"volume":[],"timestamp":[]}"#;
        assert!(parse_intraday_columnar_for_minute(empty, target).is_none());
        // Length-mismatched parallel arrays.
        let mismatched = r#"{"open":[1.0,2.0],"high":[1.0],"low":[1.0],
            "close":[1.0],"volume":[0],"timestamp":[1752118500]}"#;
        assert!(parse_intraday_columnar_for_minute(mismatched, target).is_none());
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
