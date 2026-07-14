//! Per-minute option-chain REST pipeline — PR-3, the OPTION-CHAIN half
//! (operator grant 2026-07-12; runbook
//! `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! Every trading-day minute close in session — sequenced immediately AFTER
//! the spot leg (`spot_1m_rest_boot.rs`) — this task pulls the FULL
//! current-expiry option chain for the 3 IDX_I underlyings (NIFTY 13,
//! BANKNIFTY 25, SENSEX 51 — the `CHAIN_1M_UNDERLYINGS` subset; INDIA VIX
//! is SPOT-ONLY per the 2026-07-13 operator scope addition and can never
//! enter this leg) via Dhan `POST /v2/optionchain` and persists
//! every per-strike per-leg row to the `option_chain_1m` QuestDB table
//! (DEDUP-idempotent — a re-fetch UPSERTs in place). Cold path ONLY: the
//! WS candle pipeline, tick capture and trading are untouched.
//!
//! ## Config semantics (the honest reading — no silent behaviour change)
//! The operator's 2026-07-12 grant gates this half DEFAULT-OFF pending a
//! live entitlement probe (the account had NO Option Chain Data-API
//! entitlement in June 2026 — the DH-902/806 class — and the entitlement
//! is unprobeable from the dev sandbox). The `[option_chain_1m]` config:
//! - `enabled = true` → run the pipeline. The day-start expirylist warmup
//!   doubles as the probe; an entitlement-class reject fires ONE
//!   edge-triggered HIGH page (CHAIN-01) and the pipeline stays DOWN for
//!   the day.
//! - `enabled = false` + `probe_and_report = true` (the default) → run ONE
//!   boot-time expirylist probe, report the verdict via Telegram (entitled
//!   → an Info telling the operator which setting to flip; not entitled →
//!   an Info naming the DH-902/806 class), then exit. The full pipeline
//!   NEVER auto-runs while `enabled = false` — the operator flips the
//!   config. This is the honest reading of "auto-enables/reports": a
//!   report, never a silent behaviour change.
//!
//! ## Sequencing (spot first, chain right after — best-effort, bounded)
//! The spot leg publishes a `tokio::sync::watch` signal at the END of each
//! of its fires (success or failure). This task sleeps to each minute
//! boundary + [`CHAIN_1M_FALLBACK_DELAY_MS`] but wakes EARLY when the spot
//! signal reaches its minute — so the chain normally fires the moment the
//! spot fetch completes (~0.3–1.5 s after the boundary), and the fallback
//! timer guarantees it is never blocked when the spot leg is disabled,
//! dead, or slow (in the slow case the two run concurrently — their rate
//! budgets are independent: chain = 1 unique request / 3 s per underlying
//! on the option-chain API, spot = Data-API 5/sec).
//!
//! ## Rate budget (option-chain API: 1 unique request / 3 s)
//! One fire = 3 concurrent requests, one per DISTINCT underlying —
//! explicitly allowed inside the 3 s window (option-chain.md rule 4,
//! re-verified 2026-07-12). One request per underlying per minute leaves
//! ~60 s gaps, so a defensive per-underlying ≥[`CHAIN_1M_MIN_GAP_SECS`]
//! min-gap guard never engages in normal operation. NO in-minute re-poll
//! ladder (unlike the spot leg): the chain is a live snapshot, not a
//! just-sealed candle — a failed minute is counted and the next boundary
//! re-attempts.
//!
//! ## Disk envelope (flagged operator follow-up)
//! ~150 strikes × 2 legs × 3 underlyings ≈ up to ~2–3K rows/minute at
//! wide chains (~70 MB/trading-day, ~6–8 GB per 90-day hot window) — see
//! the retention note in
//! `crates/storage/src/option_chain_1m_persistence.rs`.
//!
//! ## Lifetime
//! Same single-day-pass rationale as the spot leg: the task runs today's
//! remaining minute closes and exits after 15:30 IST (the AWS box stops at
//! 16:30 IST and cold-boots fresh each trading morning). A supervised
//! respawn wrapper (classify_join_exit +
//! `tv_chain1m_task_respawn_total{reason}` + bounded backoff) keeps the
//! scheduler alive mid-session — EXCEPT after an entitlement /
//! expirylist-failure stop, which deliberately stays down for the day
//! (never a per-minute reject storm). Panic honesty (the TICK-FLUSH-01
//! precedent): the release profile sets `panic = "abort"`, so the
//! panic-respawn arm is an unwind-build (dev/test) self-heal path only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    CHAIN_1M_CONSECUTIVE_FAIL_PAGE_THRESHOLD, CHAIN_1M_DECISION_CEILING_SECS,
    CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS, CHAIN_1M_FALLBACK_DELAY_MS, CHAIN_1M_MAX_BODY_BYTES,
    CHAIN_1M_MIN_GAP_SECS, CHAIN_1M_REQUEST_TIMEOUT_SECS, CHAIN_1M_UNDERLYING_BUDGET_SECS,
    CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD, CHAIN_1M_UNDERLYINGS,
    DHAN_OPTION_CHAIN_EXPIRYLIST_PATH, DHAN_OPTION_CHAIN_PATH, IST_UTC_OFFSET_SECONDS,
    SECONDS_PER_DAY, SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::{capture_rest_error_body, redact_url_params};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::url_join::join_api_url;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::disk_health_watcher::classify_join_exit;
use tickvault_storage::option_chain_1m_persistence::{
    OPTION_CHAIN_1M_LEG_CE, OPTION_CHAIN_1M_LEG_PE, OPTION_CHAIN_1M_SEGMENT_IDX_I,
    OptionChain1mRow, OptionChain1mWriter, ensure_option_chain_1m_table,
};

use crate::spot_1m_rest_boot::{
    EdgeAction, FailureEdge, accumulation_within_cap, count_missed_boundaries,
    declared_len_within_cap, fire_is_fresh, format_minute_ist_12h, minute_open_ist_nanos,
    next_fire_after, spot_1m_day_is_over,
};

/// Backoff before the supervisor respawns a dead/failed scheduler run.
const CHAIN_1M_RESPAWN_BACKOFF_SECS: u64 = 30;
/// Milliseconds per second / per day (wall-clock latency math).
const MILLIS_PER_SEC: i64 = 1_000;
const MILLIS_PER_DAY: i64 = 86_400_000;
/// Nanoseconds per second (IST-epoch → nanos).
const NANOS_PER_SEC: i64 = 1_000_000_000;
/// Dhan's Y2K-safe expiry date format (`"2026-07-16"`).
const EXPIRY_DATE_FMT: &str = "%Y-%m-%d";
/// Expirylist attempts = first try + the two constant backoffs.
const EXPIRYLIST_ATTEMPTS: usize = CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS.len() + 1;

// The reused FailureEdge pages at the SPOT threshold constant — the chain
// contract pins the same value, so the reuse stays honest by construction.
const _: () = assert!(
    CHAIN_1M_CONSECUTIVE_FAIL_PAGE_THRESHOLD == SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD,
    "the chain leg reuses the spot FailureEdge — thresholds must agree"
);

/// Everything the scheduler needs, cloneable so the supervisor can respawn
/// the inner run (all fields are `Arc`s / watch handles or cheap owned
/// copies).
#[derive(Clone)]
pub struct OptionChain1mTaskParams {
    /// Live token handle — re-`load()`ed EVERY fire (the 24h JWT rotates
    /// mid-session; AUTH-GAP-05 re-mints swap it atomically).
    pub token_handle: TokenHandle,
    /// Telegram dispatcher for the edge page + probe verdicts.
    pub notifier: Arc<NotificationService>,
    /// Trading calendar (weekday + NSE-holiday aware) — re-checked every
    /// loop iteration (audit-findings Rule 3).
    pub calendar: Arc<TradingCalendar>,
    /// QuestDB target for the `option_chain_1m` table.
    pub questdb: QuestDbConfig,
    /// Dhan REST v2 base URL (joined via `join_api_url` — never `format!`).
    pub rest_api_base_url: String,
    /// Dhan client id — the option-chain endpoints require the extra
    /// `client-id` header (option-chain.md rule 3).
    pub client_id: String,
    /// Spot-leg "minute completed" signal (the boundary seconds-of-day the
    /// spot leg just finished firing). `None` when the spot leg is
    /// disabled — the fallback timer then paces every chain fire.
    pub spot_minute_done: Option<tokio::sync::watch::Receiver<Option<u32>>>,
}

// ---------------------------------------------------------------------------
// Pure request builders + expiry selection
// ---------------------------------------------------------------------------

/// The `/v2/optionchain/expirylist` request body for one underlying —
/// PascalCase field names, `UnderlyingScrip` is an INTEGER (option-chain.md
/// rule 5; contrast the charts APIs' STRING securityId). Pure.
#[must_use]
pub fn expirylist_request_body(underlying_scrip: u64) -> Value {
    json!({
        "UnderlyingScrip": underlying_scrip,
        "UnderlyingSeg": OPTION_CHAIN_1M_SEGMENT_IDX_I,
    })
}

/// The `/v2/optionchain` request body for one underlying + expiry. Pure.
#[must_use]
pub fn chain_request_body(underlying_scrip: u64, expiry: &str) -> Value {
    json!({
        "UnderlyingScrip": underlying_scrip,
        "UnderlyingSeg": OPTION_CHAIN_1M_SEGMENT_IDX_I,
        "Expiry": expiry,
    })
}

/// Parse an expirylist response body into dates. Malformed bodies / rows
/// parse to an empty set (panic-free); individual malformed dates are
/// skipped. Pure.
#[must_use]
pub fn parse_expiry_list(body: &str) -> Vec<NaiveDate> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(items) = v.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|s| NaiveDate::parse_from_str(s.trim(), EXPIRY_DATE_FMT).ok())
        .collect()
}

/// Select the CURRENT expiry: the nearest date at-or-after `today` (IST).
/// On expiry day the same-day expiry IS still the current one through the
/// session (the never-roll house precedent — it falls out of tomorrow's
/// warmup). `None` when every listed expiry is already past (data
/// anomaly — never guessed). Pure.
#[must_use]
pub fn select_current_expiry(expiries: &[NaiveDate], today: NaiveDate) -> Option<NaiveDate> {
    expiries.iter().copied().filter(|e| *e >= today).min()
}

/// `true` when the spot leg's watch signal covers this fire boundary —
/// the spot leg finished firing this (or a later) minute close. Pure.
#[must_use]
pub fn spot_signal_satisfies(signal: Option<u32>, fire_secs_of_day: u32) -> bool {
    signal.is_some_and(|s| s >= fire_secs_of_day)
}

/// Defensive per-underlying pacing: milliseconds still to wait so two
/// requests for the SAME underlying are ≥ [`CHAIN_1M_MIN_GAP_SECS`] apart
/// (Dhan's 1-unique-request-per-3s limit). A midnight-wrapped/backwards
/// clock yields 0 (never a long spurious sleep). Pure.
#[must_use]
pub fn min_gap_wait_ms(last_request_ms_of_day: Option<i64>, now_ms_of_day: i64) -> u64 {
    let Some(last) = last_request_ms_of_day else {
        return 0;
    };
    if now_ms_of_day < last {
        return 0;
    }
    let elapsed = now_ms_of_day - last;
    let gap = (CHAIN_1M_MIN_GAP_SECS as i64) * MILLIS_PER_SEC;
    u64::try_from(gap.saturating_sub(elapsed)).unwrap_or(0)
}

/// Backoff before re-entering the fire loop after a stale wake where the
/// wall clock sits BEFORE the fire moment (clock step-BACK / NTP step /
/// VM pause-resume): milliseconds until the fire moment. Without this,
/// the already-satisfied spot signal makes [`wait_for_signal_or_fallback`]
/// return with ZERO awaits and the staleness gate `continue`s straight
/// back to the SAME fire — a no-yield busy-spin pinning a tokio worker
/// (2-vCPU prod host) plus a `warn!` storm for the step magnitude
/// (hostile-review H1). `0` when the wake is at/past the fire (the normal
/// stale path re-arms via `last_fired`). Pure.
#[must_use]
pub fn stale_wake_backoff_ms(fire_secs_of_day: u32, woke_at_secs_of_day: u32) -> u64 {
    u64::from(fire_secs_of_day.saturating_sub(woke_at_secs_of_day)).saturating_mul(1_000)
}

/// A retry may LAUNCH only while the fire is still inside the decision
/// ceiling — measured from the minute close, REAL wall clock, including
/// the same-key min-gap wait still ahead of it (2026-07-14 hardening;
/// runbook `cross-source-chain-coverage-2026-07-14.md` §2). Pure.
#[must_use]
pub fn chain_retry_allowed(elapsed_ms_since_close_at_launch: i64) -> bool {
    elapsed_ms_since_close_at_launch >= 0
        && elapsed_ms_since_close_at_launch
            < (CHAIN_1M_DECISION_CEILING_SECS as i64) * MILLIS_PER_SEC
}

/// The REAL-clock elapsed-since-close a retry would LAUNCH at: now minus
/// the minute close, PLUS the same-key ≥3s gap wait still ahead of it
/// (the [`min_gap_wait_ms`] output) — the [`chain_retry_allowed`] input.
/// Saturating so a pathological clock never wraps. Pure.
#[must_use]
pub fn retry_launch_elapsed_ms(
    now_ms_of_day: i64,
    minute_close_ms_of_day: i64,
    gap_wait_ms: u64,
) -> i64 {
    now_ms_of_day
        .saturating_sub(minute_close_ms_of_day)
        .saturating_add(i64::try_from(gap_wait_ms).unwrap_or(i64::MAX))
}

/// `true` when the body parses as the documented Dhan error-JSON shape —
/// a JSON OBJECT carrying an `errorCode` or `errorType` field
/// (api-introduction.md rule 6: exactly 3 string fields, always present).
/// A gateway/WAF HTML block page never satisfies this, so wording checks
/// gated on it cannot false-positive on proxy pages (hostile-review M2).
/// Pure.
#[must_use]
pub fn body_has_dhan_error_shape(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|v| {
        v.as_object()
            .is_some_and(|o| o.contains_key("errorCode") || o.contains_key("errorType"))
    })
}

/// Entitlement-class classification (CHAIN-01 vs transient CHAIN-02): a
/// reject naming `DH-902` or Data-API `806` anywhere in the body, or a
/// 401/403 whose body BOTH has the Dhan error-JSON shape
/// ([`body_has_dhan_error_shape`] — a WAF/proxy HTML page mentioning
/// "subscription" is a TRANSIENT, never a day-killing verdict) AND names
/// the missing SUBSCRIPTION. A bare 401 without that wording is a
/// token-class transient (the renewal machinery owns it), never an
/// entitlement verdict. Residual false-positive direction (documented in
/// the runbook §2b): a genuine Dhan-shaped 403 naming "subscription" for
/// a non-entitlement reason still classifies absent — bounded to one HIGH
/// page, day-scoped, WS untouched. Pure.
#[must_use]
pub fn is_entitlement_reject(status: u16, body: &str) -> bool {
    if body.contains("DH-902") || body.contains("\"806\"") {
        return true;
    }
    if matches!(status, 401 | 403) && body_has_dhan_error_shape(body) {
        let lower = body.to_lowercase();
        return lower.contains("subscrib") || lower.contains("data apis not");
    }
    false
}

// ---------------------------------------------------------------------------
// Pure chain-response parsing (hostile-input hardened)
// ---------------------------------------------------------------------------

/// Hard cap on parsed strikes per chain response (a hostile/corrupt body
/// inside the 8 MiB cap can never mint unbounded rows/RAM): a real NIFTY
/// chain carries ~100–200 strikes; 400 strikes (⇒ ≤800 legs) is ~2×
/// headroom. Strikes past the cap are COUNTED as truncated (coalesced
/// `error!` at the caller — never silent).
pub const MAX_STRIKES_PER_CHAIN: usize = 400;

/// Strike-plausibility ceiling (exclusive): a parsed strike must satisfy
/// `0 < strike < MAX_PLAUSIBLE_STRIKE` or it is skipped + counted with
/// the invalid keys (no NSE index strike approaches 10M).
pub const MAX_PLAUSIBLE_STRIKE: f64 = 10_000_000.0;

/// One parsed option leg (CE or PE) of one strike.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedLeg {
    /// Strike price (parsed from Dhan's decimal-string map key).
    pub strike: f64,
    /// `"CE"` or `"PE"`.
    pub leg: &'static str,
    /// Dhan SecurityId of this contract (0 when absent).
    pub contract_security_id: i64,
    pub last_price: f64,
    pub implied_volatility: f64,
    pub delta: f64,
    pub theta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub oi: i64,
    pub volume: i64,
    pub previous_oi: i64,
}

/// One parsed option-chain snapshot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedChain {
    /// The response's own `data.last_price` (underlying spot).
    pub underlying_spot: f64,
    /// Every present leg across every strike (a one-sided deep-OTM strike
    /// contributes one leg — CE or PE `null`/absent is skipped, never a
    /// panic; option-chain.md rule 7).
    pub legs: Vec<ParsedLeg>,
    /// Strike keys that did not parse as decimal numbers OR parsed to an
    /// implausible value (non-finite, ≤ 0, ≥ [`MAX_PLAUSIBLE_STRIKE`]) —
    /// skipped, counted by the caller — never silent.
    pub invalid_strikes: u32,
    /// Strikes past [`MAX_STRIKES_PER_CHAIN`] — dropped by the parse cap,
    /// counted by the caller (coalesced `error!`) — never silent.
    pub truncated_strikes: u32,
    /// KEPT strike count (excludes invalid + truncated keys) — the
    /// ladder-shrink watermark's input (2026-07-14 hardening).
    pub strike_count: u32,
}

/// Tolerant numeric read: JSON number (int OR float) → f64; anything else
/// (absent / null / string / object) → 0.0. Pure.
fn val_f64(obj: &Value, key: &str) -> f64 {
    obj.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Tolerant integer read: JSON int → i64; a float (Dhan number-type wobble)
/// truncates; anything else → 0. Pure.
#[allow(clippy::cast_possible_truncation)] // APPROVED: deliberate f64→i64 truncation fallback for number-type wobble
fn val_i64(obj: &Value, key: &str) -> i64 {
    match obj.get(key) {
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        None => 0,
    }
}

/// Parse one leg object (`ce`/`pe`) — absent/null/non-object legs return
/// `None` (skipped); field-level type wobble degrades per-field to 0 /
/// 0.0, never a parse failure. Pure.
fn parse_leg(strike: f64, leg_name: &'static str, leg_val: Option<&Value>) -> Option<ParsedLeg> {
    let obj = leg_val?;
    if !obj.is_object() {
        return None;
    }
    let greeks = obj.get("greeks").cloned().unwrap_or(Value::Null);
    Some(ParsedLeg {
        strike,
        leg: leg_name,
        contract_security_id: val_i64(obj, "security_id"),
        last_price: val_f64(obj, "last_price"),
        implied_volatility: val_f64(obj, "implied_volatility"),
        delta: val_f64(&greeks, "delta"),
        theta: val_f64(&greeks, "theta"),
        gamma: val_f64(&greeks, "gamma"),
        vega: val_f64(&greeks, "vega"),
        oi: val_i64(obj, "oi"),
        volume: val_i64(obj, "volume"),
        previous_oi: val_i64(obj, "previous_oi"),
    })
}

/// Parse a full `/v2/optionchain` response body. `None` on a malformed
/// top-level shape (not JSON / no `data` object / no `oc` map — the
/// CHAIN-02 parse-failure arm); a well-formed response with ZERO strikes
/// parses to an empty `legs` (the `outcome="empty"` arm). Strike keys are
/// DECIMAL STRINGS (`"25650.000000"`) parsed as f64 — never assumed
/// integer (option-chain.md rule 6); unparsable keys are skipped +
/// counted. Panic-free on hostile input by construction (`serde_json`
/// value walking, no indexing, no unwrap). Pure.
#[must_use]
pub fn parse_option_chain(body: &str) -> Option<ParsedChain> {
    let v: Value = serde_json::from_str(body).ok()?;
    let data = v.get("data")?;
    let oc = data.get("oc").and_then(Value::as_object)?;
    let mut chain = ParsedChain {
        underlying_spot: val_f64(data, "last_price"),
        ..ParsedChain::default()
    };
    let mut strikes_kept: usize = 0;
    for (strike_key, legs) in oc {
        let Ok(strike) = strike_key.trim().parse::<f64>() else {
            chain.invalid_strikes = chain.invalid_strikes.saturating_add(1);
            continue;
        };
        // Plausibility bound: finite, positive, below the sanity ceiling
        // (a corrupt/hostile key like "-5" or "1e300" never mints a row).
        if !strike.is_finite() || strike <= 0.0 || strike >= MAX_PLAUSIBLE_STRIKE {
            chain.invalid_strikes = chain.invalid_strikes.saturating_add(1);
            continue;
        }
        if strikes_kept >= MAX_STRIKES_PER_CHAIN {
            chain.truncated_strikes = chain.truncated_strikes.saturating_add(1);
            continue;
        }
        strikes_kept += 1;
        if let Some(ce) = parse_leg(strike, OPTION_CHAIN_1M_LEG_CE, legs.get("ce")) {
            chain.legs.push(ce);
        }
        if let Some(pe) = parse_leg(strike, OPTION_CHAIN_1M_LEG_PE, legs.get("pe")) {
            chain.legs.push(pe);
        }
    }
    // Bounded by MAX_STRIKES_PER_CHAIN (400) — the cast can never clip.
    chain.strike_count = u32::try_from(strikes_kept).unwrap_or(u32::MAX);
    Some(chain)
}

// ---------------------------------------------------------------------------
// Wall-clock helpers (IST) — mirrors of the spot leg's private helpers
// ---------------------------------------------------------------------------

/// IST seconds-of-day from the wall clock.
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

/// IST calendar date for "now".
fn today_ist() -> NaiveDate {
    let utc = DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0).unwrap_or_default();
    (utc + ChronoDuration::seconds(i64::from(IST_UTC_OFFSET_SECONDS))).date_naive()
}

/// Retrieval wall-clock instant as IST nanoseconds.
fn fetched_at_ist_nanos_now() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS).saturating_mul(NANOS_PER_SEC))
}

// ---------------------------------------------------------------------------
// HTTP leg
// ---------------------------------------------------------------------------

/// One request's typed failure — `entitlement` is classified from the RAW
/// (pre-redaction) body via [`is_entitlement_reject`]; `msg` is the
/// bounded secret-redacted capture (DHAN-REST-400 discipline).
#[derive(Clone, Debug, PartialEq)]
struct ChainFetchFailure {
    entitlement: bool,
    msg: String,
}

/// Read a response body with the [`CHAIN_1M_MAX_BODY_BYTES`] cap enforced
/// BOTH on the declared `Content-Length` and on the streamed accumulation
/// (the csv_downloader §18 pattern, reusing the spot leg's pure cap
/// checks) — a misbehaving/hostile server can never buffer unbounded
/// bytes here.
async fn read_body_capped(mut resp: reqwest::Response) -> Result<String, String> {
    if !declared_len_within_cap(resp.content_length(), CHAIN_1M_MAX_BODY_BYTES) {
        return Err(format!(
            "body too large: declared {} bytes > cap {CHAIN_1M_MAX_BODY_BYTES}",
            resp.content_length().unwrap_or_default()
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        // Transport errors ride the same secret-redact + 300-char bound
        // as body captures (a reqwest error can echo the URL/peer text).
        .map_err(|e| format!("read: {}", capture_rest_error_body(&e.to_string())))?
    {
        if !accumulation_within_cap(buf.len(), chunk.len(), CHAIN_1M_MAX_BODY_BYTES) {
            return Err(format!(
                "body exceeded cap {CHAIN_1M_MAX_BODY_BYTES} bytes mid-stream"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| "body not valid UTF-8".to_string())
}

/// One option-chain-family REST round-trip (chain OR expirylist — they
/// share headers + auth + classification) → the raw 2xx body text. `Err`
/// carries the entitlement verdict + status + token-redacted URL +
/// ≤300-char secret-redacted body.
async fn chain_fetch_once(
    client: &reqwest::Client,
    url: &str,
    jwt: &str,
    client_id: &str,
    body: &Value,
) -> Result<String, ChainFetchFailure> {
    // 2026-07-14 operator pacing directive: the option-chain API routes
    // through the SAME shared Dhan Data-API limiter as the spot leg
    // (per-minute chain fires + expirylist warmup/probe all funnel
    // through this fn). The 1-unique-per-3s per-underlying min-gap stays
    // LAYERED ON TOP, unchanged.
    crate::dhan_data_api_limiter::shared_dhan_data_api_limiter()
        .acquire()
        .await;
    let resp = client
        .post(url)
        .header("access-token", jwt)
        // The option-chain endpoints require the EXTRA client-id header
        // (option-chain.md rule 3) — missing it is an auth error.
        .header("client-id", client_id)
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| ChainFetchFailure {
            entitlement: false,
            // Same secret-redact + 300-char bound as body captures — a
            // reqwest send error can echo the URL/peer text unbounded.
            msg: format!("send: {}", capture_rest_error_body(&e.to_string())),
        })?;
    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Feed the shared self-tuner from the REAL StatusCode — chain
            // 429s and spot 429s tune ONE pacing decision.
            crate::dhan_data_api_limiter::shared_dhan_data_api_limiter().record_429();
        }
        let error_body = read_body_capped(resp).await.unwrap_or_default();
        return Err(ChainFetchFailure {
            entitlement: is_entitlement_reject(status.as_u16(), &error_body),
            msg: format!(
                "http {status} url={} body={}",
                redact_url_params(url),
                capture_rest_error_body(&error_body)
            ),
        });
    }
    read_body_capped(resp)
        .await
        .map_err(|msg| ChainFetchFailure {
            entitlement: false,
            msg,
        })
}

// ---------------------------------------------------------------------------
// Day-start expirylist warmup + boot-time probe
// ---------------------------------------------------------------------------

/// One resolved underlying, ready for per-minute chain fetches.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedUnderlying {
    /// Dhan IDX_I SecurityId of the underlying.
    pub security_id: u64,
    /// Human symbol.
    pub symbol: &'static str,
    /// Today's CURRENT expiry (from the expirylist API — never guessed).
    pub expiry: NaiveDate,
    /// The expiry as the request-body string (`"YYYY-MM-DD"`).
    pub expiry_str: String,
}

/// Day-start warmup verdict.
enum WarmupOutcome {
    /// Every underlying resolved a current expiry.
    Resolved(Vec<ResolvedUnderlying>),
    /// The broker named the DH-902/806 entitlement class — the pipeline
    /// (or the probe) reports and stays down.
    EntitlementAbsent(String),
    /// Bounded retries exhausted (transport / non-2xx / malformed / no
    /// usable expiry) — CHAIN-04; the pipeline degrades to
    /// disabled-for-the-day.
    Failed(String),
}

/// One underlying's bounded expirylist fetch: first try + the constant
/// backoffs. Entitlement rejects short-circuit (retrying a permanent
/// account verdict is pointless).
async fn fetch_expirylist_bounded(
    client: &reqwest::Client,
    url: &str,
    jwt: &secrecy::SecretString,
    client_id: &str,
    security_id: u64,
) -> Result<Vec<NaiveDate>, ChainFetchFailure> {
    let body = expirylist_request_body(security_id);
    let mut last_failure = ChainFetchFailure {
        entitlement: false,
        msg: "expirylist never attempted".to_string(),
    };
    for attempt in 0..EXPIRYLIST_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(
                CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS[attempt - 1],
            ))
            .await;
        }
        match chain_fetch_once(client, url, jwt.expose_secret(), client_id, &body).await {
            Ok(body_text) => {
                let expiries = parse_expiry_list(&body_text);
                if expiries.is_empty() {
                    last_failure = ChainFetchFailure {
                        entitlement: false,
                        msg: format!("sid {security_id}: 2xx but no parseable expiry dates"),
                    };
                    continue;
                }
                return Ok(expiries);
            }
            Err(failure) => {
                let stop = failure.entitlement;
                last_failure = failure;
                if stop {
                    break;
                }
            }
        }
    }
    Err(last_failure)
}

/// Day-start warmup: resolve the current expiry for ALL 3 underlyings.
/// Sequential (one underlying at a time — the expirylist runs once per
/// day; pacing over concurrency here) with the per-call bounded retry.
async fn resolve_current_expiries(
    client: &reqwest::Client,
    expirylist_url: &str,
    jwt: &secrecy::SecretString,
    client_id: &str,
    today: NaiveDate,
) -> WarmupOutcome {
    let mut resolved = Vec::with_capacity(CHAIN_1M_UNDERLYINGS.len());
    for (security_id, symbol) in CHAIN_1M_UNDERLYINGS {
        match fetch_expirylist_bounded(client, expirylist_url, jwt, client_id, security_id).await {
            Ok(expiries) => {
                let Some(expiry) = select_current_expiry(&expiries, today) else {
                    return WarmupOutcome::Failed(format!(
                        "sid {security_id} ({symbol}): every listed expiry is already \
                         past — refusing to guess (data anomaly)"
                    ));
                };
                resolved.push(ResolvedUnderlying {
                    security_id,
                    symbol,
                    expiry,
                    expiry_str: expiry.format(EXPIRY_DATE_FMT).to_string(),
                });
            }
            Err(failure) if failure.entitlement => {
                return WarmupOutcome::EntitlementAbsent(format!(
                    "sid {security_id} ({symbol}): {}",
                    failure.msg
                ));
            }
            Err(failure) => {
                return WarmupOutcome::Failed(format!(
                    "sid {security_id} ({symbol}): {}",
                    failure.msg
                ));
            }
        }
        // Defensive pacing between the day-start expirylist calls (same
        // underlying never repeats here, but the 1-unique-per-3s limit is
        // honored across the warmup batch for good measure).
        tokio::time::sleep(Duration::from_secs(CHAIN_1M_MIN_GAP_SECS)).await;
    }
    WarmupOutcome::Resolved(resolved)
}

/// Probe-only path (`enabled = false` + `probe_and_report = true`): ONE
/// expirylist probe for NIFTY at boot on a trading day, verdict via
/// Telegram, then exit. NEVER runs the pipeline.
// TEST-EXEMPT: live-deps async runner — the classification (is_entitlement_reject / parse_expiry_list / select_current_expiry) is unit-tested below; wiring pinned by crates/app/tests/option_chain_1m_wiring_guard.rs.
pub async fn run_option_chain_1m_probe(params: OptionChain1mTaskParams) {
    if !params.calendar.is_trading_day_today() {
        info!("option_chain_1m: non-trading day — entitlement probe skipped");
        return;
    }
    let url = join_api_url(&params.rest_api_base_url, DHAN_OPTION_CHAIN_EXPIRYLIST_PATH);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(CHAIN_1M_REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::Chain04ExpirylistFailed.code_str(),
                stage = "probe_client_build",
                ?err,
                "CHAIN-04: HTTP client build failed — the option-chain \
                 entitlement probe did not run today"
            );
            return;
        }
    };
    let jwt: Option<secrecy::SecretString> = {
        let guard = params.token_handle.load();
        guard
            .as_ref()
            .as_ref()
            .map(|state| state.access_token().clone())
    };
    let Some(jwt) = jwt else {
        error!(
            code = ErrorCode::Chain04ExpirylistFailed.code_str(),
            stage = "probe_no_token",
            "CHAIN-04: no access token at probe time — the option-chain \
             entitlement probe did not run today"
        );
        return;
    };
    // Probe with the FIRST underlying only (NIFTY) — one cheap call
    // answers the account-level entitlement question.
    let (probe_sid, probe_symbol) = CHAIN_1M_UNDERLYINGS[0];
    match fetch_expirylist_bounded(&client, &url, &jwt, &params.client_id, probe_sid).await {
        Ok(expiries) => {
            info!(
                symbol = probe_symbol,
                expiries = expiries.len(),
                config_key = "[option_chain_1m].enabled",
                "option_chain_1m: entitlement probe PASSED — chain data is \
                 available; pipeline stays OFF until the config is flipped \
                 (the Telegram body carries the plain-English action; the \
                 exact key lives HERE)"
            );
            params
                .notifier
                .notify(NotificationEvent::ChainEntitlementConfirmed);
        }
        Err(failure) if failure.entitlement => {
            info!(
                detail = %failure.msg,
                "option_chain_1m: entitlement probe verdict — NOT entitled \
                 (DH-902/806 class); pipeline stays OFF"
            );
            params
                .notifier
                .notify(NotificationEvent::ChainEntitlementAbsent {
                    pipeline_enabled: false,
                    detail: failure.msg,
                });
        }
        Err(failure) => {
            // Transient/inconclusive — log-sink only (the probe is a
            // best-effort report; tomorrow's boot re-probes).
            error!(
                code = ErrorCode::Chain04ExpirylistFailed.code_str(),
                stage = "probe_inconclusive",
                detail = %failure.msg,
                "CHAIN-04: option-chain entitlement probe was inconclusive \
                 (transient failure) — no verdict today; tomorrow's boot \
                 re-probes"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Per-minute fire
// ---------------------------------------------------------------------------

/// One underlying's per-minute chain verdict.
#[derive(Clone, Debug, PartialEq)]
enum ChainFetchOutcome {
    /// A parseable chain with ≥1 leg (`close_to_data_ms` = minute close →
    /// retrieval wall-clock latency).
    Found {
        chain: ParsedChain,
        close_to_data_ms: i64,
    },
    /// A parseable 2xx whose chain carried ZERO strikes — counted
    /// `outcome="empty"`, included in the failure edge, never silent.
    Empty,
    /// The broker named the entitlement class — stops the pipeline for
    /// the day.
    Entitlement(String),
    /// Transport / non-2xx / malformed body / budget overrun.
    Failed(String),
}

/// One underlying's bounded per-minute fetch: the defensive min-gap wait,
/// then ONE request, hard-bounded by [`CHAIN_1M_UNDERLYING_BUDGET_SECS`].
async fn fetch_chain_bounded(
    client: &reqwest::Client,
    url: &str,
    jwt: &secrecy::SecretString,
    client_id: &str,
    body: &Value,
    minute_close_ms_of_day: i64,
    last_request_ms: Option<i64>,
) -> (ChainFetchOutcome, i64) {
    let attempt = async {
        let wait_ms = min_gap_wait_ms(last_request_ms, ist_millis_of_day_now());
        if wait_ms > 0 {
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }
        let requested_at_ms = ist_millis_of_day_now();
        let started = std::time::Instant::now();
        let result = chain_fetch_once(client, url, jwt.expose_secret(), client_id, body).await;
        metrics::histogram!("tv_chain1m_fetch_duration_ms")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        let outcome = match result {
            Ok(body_text) => match parse_option_chain(&body_text) {
                Some(chain) if chain.legs.is_empty() => ChainFetchOutcome::Empty,
                Some(chain) => {
                    let close_to_data_ms =
                        (ist_millis_of_day_now() - minute_close_ms_of_day).max(0);
                    ChainFetchOutcome::Found {
                        chain,
                        close_to_data_ms,
                    }
                }
                None => ChainFetchOutcome::Failed(
                    "2xx but the body was not a parseable option chain".to_string(),
                ),
            },
            Err(failure) if failure.entitlement => ChainFetchOutcome::Entitlement(failure.msg),
            Err(failure) => ChainFetchOutcome::Failed(failure.msg),
        };
        (outcome, requested_at_ms)
    };
    match tokio::time::timeout(
        Duration::from_secs(CHAIN_1M_UNDERLYING_BUDGET_SECS),
        attempt,
    )
    .await
    {
        Ok(v) => v,
        Err(_elapsed) => (
            ChainFetchOutcome::Failed(format!(
                "chain budget exceeded ({CHAIN_1M_UNDERLYING_BUDGET_SECS}s) — peer stalling"
            )),
            ist_millis_of_day_now(),
        ),
    }
}

/// What phase 2 does with ONE first-pass outcome (2026-07-14 hardening —
/// runbook `cross-source-chain-coverage-2026-07-14.md` §2). Pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryDecision {
    /// Not retry-eligible (Found / Entitlement) — the first-pass outcome
    /// stands. Persist failures happen in phase 3 (our side) and the
    /// no-token minute never builds outcomes at all — neither can reach
    /// this classifier.
    Keep,
    /// Eligible, but the decision ceiling refuses the LAUNCH — counted
    /// (`tv_chain1m_retry_total{outcome="skipped_ceiling"}`) + coded,
    /// never silent.
    SkippedCeiling,
    /// Launch the single bounded retry.
    Launch,
}

/// Retry-eligibility + ceiling gate for one first-pass outcome: ONLY
/// `Empty` and transport-`Failed` are eligible; the launch must sit
/// inside [`chain_retry_allowed`]'s decision ceiling. Pure.
fn chain_retry_decision(outcome: &ChainFetchOutcome, elapsed_ms_at_launch: i64) -> RetryDecision {
    if !matches!(
        outcome,
        ChainFetchOutcome::Empty | ChainFetchOutcome::Failed(_)
    ) {
        return RetryDecision::Keep;
    }
    if chain_retry_allowed(elapsed_ms_at_launch) {
        RetryDecision::Launch
    } else {
        RetryDecision::SkippedCeiling
    }
}

/// Phase-2 bounded retry pass: at most ONE retry per underlying per
/// minute for first-pass `Empty` / transport-`Failed` outcomes,
/// SEQUENTIAL over the (≤3, usually ≤1) queue. Each retry honors the
/// same-key ≥3s gap (the wait itself happens INSIDE the injected fetch —
/// `fetch_chain_bounded`'s existing internal gap sleep) and may LAUNCH
/// only inside the [`CHAIN_1M_DECISION_CEILING_SECS`] window measured on
/// the REAL wall clock from the minute close INCLUDING that wait; a
/// refused retry is counted + coded, never silent. The retry's outcome
/// REPLACES the first pass's for ALL downstream processing (a retry
/// answering with the entitlement class is honored exactly like a pass-1
/// entitlement → the day-scoped CHAIN-01 stop). NEVER retried: `Found`,
/// `Entitlement`, the no-token minute (no outcomes exist), persist
/// failures (phase 3, our side). Generic over the fetch + clock so the
/// unit tests below inject deterministic outcomes; production passes the
/// real `fetch_chain_bounded` closure + `ist_millis_of_day_now`.
async fn chain_retry_pass<N, F, Fut>(
    outcomes: &mut [(usize, ChainFetchOutcome)],
    targets: &[ResolvedUnderlying],
    last_request_ms: &mut [Option<i64>],
    minute_close_ms: i64,
    minute_label: &str,
    now_ms_of_day: N,
    mut fetch: F,
) where
    N: Fn() -> i64,
    F: FnMut(&ResolvedUnderlying, Option<i64>) -> Fut,
    Fut: std::future::Future<Output = (ChainFetchOutcome, i64)>,
{
    for (idx, outcome) in outcomes.iter_mut() {
        // Defensive: every idx comes from the phase-1 enumerate, so the
        // lookup always succeeds — a miss just keeps the pass-1 outcome.
        let Some(target) = targets.get(*idx) else {
            continue;
        };
        let prior_request_ms = last_request_ms.get(*idx).copied().flatten();
        let gap_wait_ms = min_gap_wait_ms(prior_request_ms, now_ms_of_day());
        let elapsed_at_launch =
            retry_launch_elapsed_ms(now_ms_of_day(), minute_close_ms, gap_wait_ms);
        match chain_retry_decision(outcome, elapsed_at_launch) {
            RetryDecision::Keep => continue,
            RetryDecision::SkippedCeiling => {
                metrics::counter!("tv_chain1m_retry_total", "outcome" => "skipped_ceiling")
                    .increment(1);
                let symbol = target.symbol;
                warn!(
                    code = ErrorCode::Chain02FetchDegraded.code_str(),
                    stage = "retry_skipped_ceiling",
                    symbol,
                    elapsed_ms_at_launch = elapsed_at_launch,
                    ceiling_secs = CHAIN_1M_DECISION_CEILING_SECS,
                    minute = minute_label,
                    "CHAIN-02: retry refused — the decision ceiling has \
                     passed (counted, never silent; the minute's failure \
                     rides the existing accounting)"
                );
                continue;
            }
            RetryDecision::Launch => {}
        }
        let (retry_outcome, requested_at_ms) = fetch(target, prior_request_ms).await;
        if let Some(slot) = last_request_ms.get_mut(*idx) {
            *slot = Some(requested_at_ms);
        }
        match &retry_outcome {
            ChainFetchOutcome::Found { .. } => {
                metrics::counter!("tv_chain1m_retry_total", "outcome" => "recovered").increment(1);
            }
            ChainFetchOutcome::Empty | ChainFetchOutcome::Failed(_) => {
                metrics::counter!("tv_chain1m_retry_total", "outcome" => "still_failed")
                    .increment(1);
            }
            // A retry answering with the entitlement class is neither
            // recovered nor still-failed in the retry sense — it becomes
            // the day-scoped stop; the minute-final fetch counter
            // (phase 3's entitlement arm) records it.
            ChainFetchOutcome::Entitlement(_) => {}
        }
        *outcome = retry_outcome;
    }
}

/// The chain edge's "fully failed" verdict for one fired minute — no
/// underlying succeeded, OR the persist leg (append/flush) failed. A
/// fetched-but-never-persisted minute is NOT ok (the spot M1 precedent).
#[must_use]
pub fn chain_minute_fully_failed(ok_count: usize, persist_failed: bool) -> bool {
    ok_count == 0 || persist_failed
}

// ---------------------------------------------------------------------------
// Pure per-underlying not-served detector (2026-07-14 — the Dhan mirror
// of PR #1537's Groww detector; the NIFTY expiry-day vendor-cutoff
// companion: Groww stopped serving the same-day-expiring NIFTY chain at
// 14:54 IST while BANKNIFTY + SENSEX kept working, and the ok==0
// escalation edge paged nobody all afternoon — the Dhan leg carried the
// IDENTICAL blind spot).
// FOLLOW-UP: duplicate-now-extract-later — once #1537 merges, extract
// the shared tracker into one module consumed by both chain legs (the
// FailureEdge import precedent).
// ---------------------------------------------------------------------------

/// What the caller must do for ONE underlying after recording a minute's
/// per-underlying served verdicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnderlyingEdgeAction {
    /// Nothing to page for this underlying this minute.
    None,
    /// RISING edge: this underlying reached
    /// [`CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD`] consecutive counted
    /// not-served minutes (each with ≥1 sibling success) — page ONCE
    /// (High), latched until this underlying's own recovery.
    Page { consecutive: u32 },
    /// FALLING edge: a paged underlying's chain was served again — one
    /// Info ping; the latch re-arms.
    Recover { not_served_minutes: u32 },
}

/// Per-underlying not-served state: consecutive COUNTED not-served
/// minutes + the page latch.
#[derive(Debug, Default)]
struct UnderlyingServedState {
    consecutive_not_served: u32,
    paged: bool,
}

/// Per-underlying "is the vendor serving this chain?" tracker.
/// Distinguishes vendor-not-serving-ONE-underlying from a global outage:
///
/// | This minute, this underlying | ≥1 OTHER underlying served? | Effect on this underlying |
/// |---|---|---|
/// | served (chain with strikes retrieved) | — | streak reset; `Recover` if paged |
/// | not served (empty OR error) | yes | streak +1; `Page` once at the threshold |
/// | not served | no (global outage) | HOLD — neither counts nor resets |
///
/// The global-outage HOLD keeps the two signals disjoint for the
/// FETCH-failure class: a full fetch outage (ok == 0) is the
/// [`FailureEdge`] escalation's page, this edge needs ≥1 OK — mutually
/// exclusive per minute WITHIN that class. HONEST OVERLAP: a
/// persist-failed minute with ok ≥ 1 can legitimately count toward BOTH
/// edges (the M1 gate makes the escalation edge count it fully-failed
/// while an empty sibling counts here) — two DISTINCT signals:
/// persistence broken + vendor not serving one underlying. "Served" is
/// FETCH-level (`Found`): the vendor-serving question — persist
/// failures are OUR side and already feed the escalation edge via the
/// spot-M1 persist gate. An underlying MISSING from a minute's verdicts
/// (the unwind-build join-failure arm) is simply untouched — a
/// per-underlying HOLD. Pure state machine — unit-tested without a
/// clock. State is per scheduler run (session-scoped, same envelope as
/// [`FailureEdge`] — a task respawn restarts the streak; the run itself
/// is per trading day).
#[derive(Debug, Default)]
pub struct UnderlyingServedTracker {
    per_underlying: std::collections::HashMap<&'static str, UnderlyingServedState>,
}

impl UnderlyingServedTracker {
    /// Record one fired minute's per-underlying served verdicts
    /// (`served` = a chain with strikes was retrieved for that underlying
    /// this fire) and return one action per input underlying,
    /// index-aligned with `verdicts`.
    pub fn record_minute(
        &mut self,
        verdicts: &[(&'static str, bool)],
    ) -> Vec<(&'static str, UnderlyingEdgeAction)> {
        let any_served = verdicts.iter().any(|&(_, served)| served);
        verdicts
            .iter()
            .map(|&(underlying, served)| {
                let state = self.per_underlying.entry(underlying).or_default();
                let action = if served {
                    let not_served_minutes = state.consecutive_not_served;
                    let was_paged = state.paged;
                    state.consecutive_not_served = 0;
                    state.paged = false;
                    if was_paged {
                        UnderlyingEdgeAction::Recover { not_served_minutes }
                    } else {
                        UnderlyingEdgeAction::None
                    }
                } else if any_served {
                    state.consecutive_not_served = state.consecutive_not_served.saturating_add(1);
                    if !state.paged
                        && state.consecutive_not_served >= CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD
                    {
                        state.paged = true;
                        UnderlyingEdgeAction::Page {
                            consecutive: state.consecutive_not_served,
                        }
                    } else {
                        UnderlyingEdgeAction::None
                    }
                } else {
                    // Global-outage minute (no underlying served): HOLD.
                    UnderlyingEdgeAction::None
                };
                (underlying, action)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Strike-ladder day-max shrink watermark (2026-07-14 — log-only)
// ---------------------------------------------------------------------------

/// What one Found minute's strike count means for the ladder watermark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderAction {
    /// Not shrunk (day-max ratcheted as needed); silently clears the
    /// episode latch.
    Normal,
    /// FIRST shrunk minute of an episode — the caller emits the ONE
    /// edge-latched coded warn (+ the per-minute counter).
    ShrunkFirst,
    /// A further shrunk minute inside a latched episode — counter only.
    ShrunkLatched,
}

/// `true` when a Found chain's kept-strike count dropped below HALF the
/// day-max watermark (⌈day_max/2⌉). The day's FIRST Found never flags
/// (`day_max == 0` seeds the watermark) — an all-day-small chain is the
/// Empty/not-served territory, never this detector's. Pure.
#[must_use]
pub fn ladder_shrunk(day_max: u32, strikes: u32) -> bool {
    day_max > 0 && strikes < day_max.div_ceil(2)
}

/// Per-underlying strike-ladder DAY-MAX watermark (log-only; runbook
/// `cross-source-chain-coverage-2026-07-14.md` §3): counted per shrunk
/// minute + ONE coded warn per episode; a silent (non-shrunk) recovery
/// clears the latch. HEURISTIC evidence, NEVER a page — expiry-day
/// ladder narrowing is legitimate vendor behavior and no expected-strike
/// baseline exists. Intra-day only; a supervisor respawn resets it;
/// shrink-to-ZERO strikes is the `Empty` class and never reaches this
/// (Found-only input).
#[derive(Clone, Copy, Debug, Default)]
pub struct LadderWatermark {
    day_max: u32,
    shrunk_latched: bool,
}

impl LadderWatermark {
    /// Feed one Found minute's kept-strike count; the returned action
    /// says what the caller emits. Pure state machine.
    pub fn observe(&mut self, strikes: u32) -> LadderAction {
        let shrunk = ladder_shrunk(self.day_max, strikes);
        self.day_max = self.day_max.max(strikes);
        if shrunk {
            if self.shrunk_latched {
                LadderAction::ShrunkLatched
            } else {
                self.shrunk_latched = true;
                LadderAction::ShrunkFirst
            }
        } else {
            self.shrunk_latched = false;
            LadderAction::Normal
        }
    }

    /// The current day-max (log payloads).
    #[must_use]
    pub fn day_max(&self) -> u32 {
        self.day_max
    }
}

/// Per-run chain serving-health state (2026-07-14) — threaded into every
/// fire as ONE bundled param: the per-underlying not-served tracker +
/// the per-underlying (index-aligned with the resolved targets)
/// strike-ladder watermarks. Same lifetime as the [`FailureEdge`]: this
/// run, which is per trading day; a supervisor respawn restarts the
/// streaks (documented ~10-minute re-detection envelope).
#[derive(Debug, Default)]
struct ChainServingHealth {
    not_served: UnderlyingServedTracker,
    ladders: [LadderWatermark; CHAIN_1M_UNDERLYINGS.len()],
}

/// What the run loop does after one fired minute.
enum MinuteVerdict {
    Continue,
    /// Entitlement reject observed mid-session — stop for the day.
    EntitlementStop(String),
}

/// One minute-close fire: 3 concurrent bounded chain fetches → rows →
/// persist → counters → edge accounting. Failures are coalesced to ONE
/// coded log per fire.
#[allow(clippy::too_many_arguments)] // APPROVED: private fire sink — a struct would be pure ceremony (spot precedent)
async fn fire_one_chain_minute(
    params: &OptionChain1mTaskParams,
    client: &reqwest::Client,
    chain_url: &str,
    targets: &[ResolvedUnderlying],
    last_request_ms: &mut [Option<i64>],
    writer: &mut OptionChain1mWriter,
    edge: &mut FailureEdge,
    health: &mut ChainServingHealth,
    fire_secs_of_day: u32,
) -> MinuteVerdict {
    let minute_open_secs = fire_secs_of_day.saturating_sub(60);
    let minute_label = format_minute_ist_12h(minute_open_secs);
    let trading_date = today_ist();

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
    let mut persist_failed = false;
    let mut entitlement: Option<String> = None;
    let mut sample_failure: Option<String> = None;
    // Per-underlying FETCH-level served verdicts (`Found` = served) for
    // the not-served detector — the vendor-serving question, deliberately
    // NOT persist-gated (persist failures are ours; the escalation edge
    // owns them via the M1 gate). A no-token minute never builds the
    // outcomes, so the verdicts stay EMPTY → the sink records nothing —
    // a whole-fire tracker HOLD (the #1537 auth-abort mirror).
    let mut served_verdicts: Vec<(&'static str, bool)> = Vec::with_capacity(targets.len());

    if let Some(jwt) = jwt {
        let target_minute_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
        let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
        let minute_close_ms = i64::from(fire_secs_of_day).saturating_mul(MILLIS_PER_SEC);

        // 2026-07-14 pacing decision (runbook
        // `cross-source-chain-coverage-2026-07-14.md` §2): the JoinSet
        // concurrency is KEPT — Dhan's option-chain rate-limit doc
        // (quoted 2026-07-14): "Rate limit for Option Chain API is set to
        // one unique request every 3 seconds. This means you can fetch
        // entire option chain for multiple different underlying
        // instrument or multiple expiries of same instrument concurrently
        // every 3 seconds." The 3s bound is per unique (underlying,
        // expiry) KEY — enforced by min_gap_wait_ms through the
        // per-underlying last_request_ms stamps; DISTINCT underlyings go
        // concurrently. A future sequentialization must be a deliberate,
        // rule-edited change.
        let mut join_set = tokio::task::JoinSet::new();
        for (idx, target) in targets.iter().enumerate() {
            let client = client.clone();
            let url = chain_url.to_string();
            let jwt = jwt.clone();
            let client_id = params.client_id.clone();
            let body = chain_request_body(target.security_id, &target.expiry_str);
            let prior_request_ms = last_request_ms.get(idx).copied().flatten();
            join_set.spawn(async move {
                let (outcome, requested_at_ms) = fetch_chain_bounded(
                    &client,
                    &url,
                    &jwt,
                    &client_id,
                    &body,
                    minute_close_ms,
                    prior_request_ms,
                )
                .await;
                (idx, outcome, requested_at_ms)
            });
        }
        // Phase 1 — drain the JoinSet into the first-pass outcomes
        // (stamps last_request_ms exactly as before; the join-failure arm
        // keeps its error accounting and pushes nothing — idx unknown;
        // unwind-builds only, release panics abort).
        let mut outcomes: Vec<(usize, ChainFetchOutcome)> = Vec::with_capacity(targets.len());
        while let Some(joined) = join_set.join_next().await {
            let Ok((idx, outcome, requested_at_ms)) = joined else {
                error_count = error_count.saturating_add(1);
                metrics::counter!("tv_chain1m_fetch_total", "outcome" => "error").increment(1);
                if sample_failure.is_none() {
                    sample_failure = Some("fetch task join failed".to_string());
                }
                continue;
            };
            if let Some(slot) = last_request_ms.get_mut(idx) {
                *slot = Some(requested_at_ms);
            }
            outcomes.push((idx, outcome));
        }
        // Phase 2 — the bounded same-key retry pass (≤1 per underlying
        // per minute, decision-ceiling-gated; refused retries counted).
        chain_retry_pass(
            &mut outcomes,
            targets,
            last_request_ms,
            minute_close_ms,
            &minute_label,
            ist_millis_of_day_now,
            |target, prior_request_ms| {
                let body = chain_request_body(target.security_id, &target.expiry_str);
                let jwt = jwt.clone();
                async move {
                    fetch_chain_bounded(
                        client,
                        chain_url,
                        &jwt,
                        &params.client_id,
                        &body,
                        minute_close_ms,
                        prior_request_ms,
                    )
                    .await
                }
            },
        )
        .await;
        // Phase 3 — process the FINAL post-retry outcomes (the per-outcome
        // match below is the pre-2026-07-14 processing body, unchanged).
        for (idx, outcome) in outcomes {
            let Some(target) = targets.get(idx) else {
                continue;
            };
            // ONE verdict line sees the FINAL post-retry outcome for every
            // processed class (2026-07-14 not-served detector input).
            served_verdicts.push((
                target.symbol,
                matches!(&outcome, ChainFetchOutcome::Found { .. }),
            ));
            match outcome {
                ChainFetchOutcome::Found {
                    chain,
                    close_to_data_ms,
                } => {
                    ok_count = ok_count.saturating_add(1);
                    metrics::counter!("tv_chain1m_fetch_total", "outcome" => "ok").increment(1);
                    #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
                    metrics::histogram!("tv_chain1m_close_to_data_ms")
                        .record(close_to_data_ms as f64);
                    if chain.invalid_strikes > 0 {
                        metrics::counter!("tv_chain1m_invalid_strikes_total")
                            .increment(u64::from(chain.invalid_strikes));
                        warn!(
                            symbol = target.symbol,
                            invalid_strikes = chain.invalid_strikes,
                            minute = %minute_label,
                            "option_chain_1m: skipped unparsable/implausible strike keys"
                        );
                    }
                    if chain.truncated_strikes > 0 {
                        // Coalesced ONCE per underlying per fire — a body
                        // past the strike cap is counted, never silent.
                        metrics::counter!("tv_chain1m_strikes_truncated_total")
                            .increment(u64::from(chain.truncated_strikes));
                        error!(
                            code = ErrorCode::Chain02FetchDegraded.code_str(),
                            stage = "strikes_truncated",
                            symbol = target.symbol,
                            truncated_strikes = chain.truncated_strikes,
                            cap = MAX_STRIKES_PER_CHAIN,
                            minute = %minute_label,
                            "CHAIN-02: chain response exceeded the strike cap — \
                             extra strikes dropped (counted, never silent)"
                        );
                    }
                    // 2026-07-14 ladder visibility (log-only; runbook
                    // `cross-source-chain-coverage-2026-07-14.md` §3): the
                    // Dhan mirrors of the Groww per-chain size histograms
                    // + the day-max strike-ladder shrink watermark.
                    #[allow(clippy::cast_precision_loss)] // APPROVED: histogram samples only
                    {
                        metrics::histogram!("tv_chain1m_strikes_per_chain")
                            .record(f64::from(chain.strike_count));
                        metrics::histogram!("tv_chain1m_legs_per_chain")
                            .record(chain.legs.len() as f64);
                    }
                    if let Some(ladder) = health.ladders.get_mut(idx) {
                        let day_max_before = ladder.day_max();
                        match ladder.observe(chain.strike_count) {
                            LadderAction::ShrunkFirst => {
                                metrics::counter!(
                                    "tv_chain1m_ladder_shrunk_total",
                                    "underlying" => target.symbol
                                )
                                .increment(1);
                                warn!(
                                    code = ErrorCode::Chain02FetchDegraded.code_str(),
                                    stage = "ladder_shrunk",
                                    underlying = target.symbol,
                                    strikes = chain.strike_count,
                                    day_max = day_max_before,
                                    minute = %minute_label,
                                    "CHAIN-02: this underlying's chain came back \
                                     with under half its day-max strike count — a \
                                     partial ladder (heuristic evidence; counted \
                                     per minute, one warn per episode, NEVER a \
                                     page — expiry-day narrowing is legitimate \
                                     vendor behavior)"
                                );
                            }
                            LadderAction::ShrunkLatched => {
                                metrics::counter!(
                                    "tv_chain1m_ladder_shrunk_total",
                                    "underlying" => target.symbol
                                )
                                .increment(1);
                            }
                            LadderAction::Normal => {}
                        }
                    }
                    let expiry_nanos = minute_open_ist_nanos(target.expiry, 0);
                    let fetched_at = fetched_at_ist_nanos_now();
                    let underlying_sid = i64::try_from(target.security_id).unwrap_or_else(|_| {
                        // Unreachable for the pinned 13/25/51 set; loud +
                        // visible sentinel, never a silent sid=0.
                        error!(
                            code = ErrorCode::Chain03PersistFailed.code_str(),
                            stage = "sid_overflow",
                            security_id = target.security_id,
                            "CHAIN-03: underlying security_id exceeds i64 — \
                             rows stamped with the i64::MAX sentinel"
                        );
                        i64::MAX
                    });
                    for leg in &chain.legs {
                        let row = OptionChain1mRow {
                            ts_ist_nanos: target_minute_nanos,
                            trading_date_ist_nanos: trading_date_nanos,
                            underlying_security_id: underlying_sid,
                            underlying_symbol: target.symbol,
                            expiry_ist_nanos: expiry_nanos,
                            strike: leg.strike,
                            leg: leg.leg,
                            contract_security_id: leg.contract_security_id,
                            last_price: leg.last_price,
                            iv: leg.implied_volatility,
                            delta: leg.delta,
                            theta: leg.theta,
                            gamma: leg.gamma,
                            vega: leg.vega,
                            oi: leg.oi,
                            volume: leg.volume,
                            previous_oi: leg.previous_oi,
                            underlying_spot: chain.underlying_spot,
                            fetched_at_ist_nanos: fetched_at,
                        };
                        if let Err(err) = writer.append_row(&row) {
                            persist_failed = true;
                            metrics::counter!(
                                "tv_chain1m_persist_errors_total", "stage" => "append"
                            )
                            .increment(1);
                            error!(
                                code = ErrorCode::Chain03PersistFailed.code_str(),
                                stage = "append",
                                symbol = target.symbol,
                                ?err,
                                "CHAIN-03: option_chain_1m row append failed"
                            );
                            if sample_failure.is_none() {
                                sample_failure = Some(format!("persist append failed: {err:#}"));
                            }
                            // One append failure poisons the batch shape —
                            // stop appending this underlying's remaining
                            // legs (the flush discard covers the rest).
                            break;
                        }
                    }
                }
                ChainFetchOutcome::Empty => {
                    empty_count = empty_count.saturating_add(1);
                    metrics::counter!("tv_chain1m_fetch_total", "outcome" => "empty").increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!(
                            "{}: 2xx but the chain carried zero strikes",
                            target.symbol
                        ));
                    }
                }
                ChainFetchOutcome::Entitlement(detail) => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_chain1m_fetch_total", "outcome" => "entitlement")
                        .increment(1);
                    if entitlement.is_none() {
                        entitlement = Some(detail);
                    }
                }
                ChainFetchOutcome::Failed(reason) => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_chain1m_fetch_total", "outcome" => "error").increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!("{}: {reason}", target.symbol));
                    }
                }
            }
        }
        if let Err(err) = writer.flush() {
            persist_failed = true;
            metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "flush").increment(1);
            error!(
                code = ErrorCode::Chain03PersistFailed.code_str(),
                stage = "flush",
                ?err,
                "CHAIN-03: option_chain_1m ILP flush failed — pending rows \
                 discarded (poisoned-buffer defense; minutes stay absent and \
                 re-fetchable via DEDUP-idempotent backfill)"
            );
            if sample_failure.is_none() {
                sample_failure = Some(format!("persist flush failed: {err:#}"));
            }
        }
    } else {
        // No token at fire time — REST cannot succeed; the whole minute is
        // a full miss (counted per underlying for honest rate math).
        error_count = targets.len();
        sample_failure = Some("no access token available at fire time".to_string());
        for _ in 0..error_count {
            metrics::counter!("tv_chain1m_fetch_total", "outcome" => "error").increment(1);
        }
    }

    // An entitlement stop pages CHAIN-01 (once, day-scoped) at the caller
    // — skip the CHAIN-02 edge accounting for the same minute so one
    // event can never double-page (hostile-review L2).
    match entitlement {
        Some(detail) => MinuteVerdict::EntitlementStop(detail),
        None => {
            record_chain_minute_verdict(
                params,
                edge,
                &minute_label,
                ok_count,
                error_count,
                empty_count,
                persist_failed,
                sample_failure.as_deref(),
            );
            record_chain_underlying_verdicts(
                params,
                &mut health.not_served,
                &served_verdicts,
                &minute_label,
            );
            MinuteVerdict::Continue
        }
    }
}

/// Coalesced per-minute verdict: ONE coded log per fired minute with any
/// failure, plus the edge-triggered escalation page / recovery ping.
#[allow(clippy::too_many_arguments)] // APPROVED: private verdict sink — a struct would be pure ceremony (spot precedent)
fn record_chain_minute_verdict(
    params: &OptionChain1mTaskParams,
    edge: &mut FailureEdge,
    minute_label: &str,
    ok_count: usize,
    error_count: usize,
    empty_count: usize,
    persist_failed: bool,
    sample_failure: Option<&str>,
) {
    let fully_failed = chain_minute_fully_failed(ok_count, persist_failed);
    match edge.record_minute(fully_failed) {
        EdgeAction::Page { consecutive } => {
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "escalation",
                consecutive,
                minute = minute_label,
                sample = sample_failure.unwrap_or("none captured"),
                "CHAIN-02: per-minute option-chain fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::ChainFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label.to_string(),
                });
        }
        EdgeAction::Recover { failed_minutes } => {
            info!(
                failed_minutes,
                minute = minute_label,
                "option_chain_1m: per-minute fetch recovered after a paged episode"
            );
            params
                .notifier
                .notify(NotificationEvent::ChainFetchRecovered {
                    minute_ist: minute_label.to_string(),
                    failed_minutes,
                });
        }
        EdgeAction::None => {
            if error_count > 0 || empty_count > 0 || persist_failed {
                // Coalesced ONCE per fire; log-sink-only — sub-edge
                // failures never page (the escalation arm does).
                error!(
                    code = ErrorCode::Chain02FetchDegraded.code_str(),
                    stage = "minute_failed",
                    minute = minute_label,
                    ok = ok_count,
                    errors = error_count,
                    empty = empty_count,
                    persist_failed,
                    sample = sample_failure.unwrap_or("none captured"),
                    "CHAIN-02: per-minute option-chain fetch degraded for this minute"
                );
            }
        }
    }
}

/// 2026-07-14 not-served companion (the #1537 Groww mirror; Dhan emits
/// stay FIELD-LESS on `feed` — the Dhan-sites convention): feed one fired
/// minute's per-underlying FETCH-level served verdicts into the
/// [`UnderlyingServedTracker`] and emit the edge-latched per-underlying
/// page / recovery ping + the per-counted-minute counter
/// (`tv_chain1m_underlying_not_served_total{underlying}` — 3 static label
/// values, the pinned plain symbols). Counting semantics live in the
/// tracker doc. Skipped-boundary and no-token minutes deliberately never
/// reach this sink (nothing was fetched for ANY underlying — the HOLD arm
/// by construction); an EntitlementStop minute skips it too (CHAIN-01
/// owns the day — the verdicts are discarded).
fn record_chain_underlying_verdicts(
    params: &OptionChain1mTaskParams,
    not_served: &mut UnderlyingServedTracker,
    verdicts: &[(&'static str, bool)],
    minute_label: &str,
) {
    if verdicts.is_empty() {
        return;
    }
    let any_served = verdicts.iter().any(|&(_, served)| served);
    let actions = not_served.record_minute(verdicts);
    for (&(underlying, served), &(_, action)) in verdicts.iter().zip(actions.iter()) {
        if !served && any_served {
            // One counted vendor-not-serving minute for this underlying
            // (a global-outage minute is deliberately NOT counted here).
            metrics::counter!(
                "tv_chain1m_underlying_not_served_total", "underlying" => underlying
            )
            .increment(1);
        }
        match action {
            UnderlyingEdgeAction::Page { consecutive } => {
                error!(
                    code = ErrorCode::Chain02FetchDegraded.code_str(),
                    stage = "underlying_not_served",
                    underlying,
                    consecutive_minutes = consecutive,
                    minute = minute_label,
                    "CHAIN-02: Dhan is not serving this underlying's option \
                     chain while the other underlyings succeed — paging once \
                     per underlying (edge-latched; re-armed on this \
                     underlying's own recovery)"
                );
                params
                    .notifier
                    .notify(NotificationEvent::Chain1mUnderlyingNotServed {
                        underlying,
                        empty_minutes: consecutive,
                    });
            }
            UnderlyingEdgeAction::Recover { not_served_minutes } => {
                info!(
                    underlying,
                    not_served_minutes,
                    minute = minute_label,
                    "option_chain_1m: this underlying's chain is being served \
                     again after a paged not-served episode"
                );
                params
                    .notifier
                    .notify(NotificationEvent::Chain1mUnderlyingServedRecovered {
                        underlying,
                        empty_minutes: not_served_minutes,
                    });
            }
            UnderlyingEdgeAction::None => {}
        }
    }
}

/// Loud accounting for minute boundaries that elapsed UNFETCHED (fire
/// overrun / suspend / clock step) — the spot leg's H2 pattern with the
/// chain's own counter + edge.
fn record_chain_skipped_boundaries(
    params: &OptionChain1mTaskParams,
    edge: &mut FailureEdge,
    skipped: u32,
    context_secs_of_day: u32,
) {
    if skipped == 0 {
        return;
    }
    metrics::counter!("tv_chain1m_boundary_skipped_total").increment(u64::from(skipped));
    let around = format_minute_ist_12h(context_secs_of_day);
    error!(
        code = ErrorCode::Chain02FetchDegraded.code_str(),
        stage = "boundary_skipped",
        skipped,
        around = %around,
        "CHAIN-02: minute boundaries elapsed unfetched (fire overrun / \
         suspend) — those minutes stay absent (re-fetchable via backfill)"
    );
    for _ in 0..skipped {
        if let EdgeAction::Page { consecutive } = edge.record_minute(true) {
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "escalation",
                consecutive,
                minute = %around,
                "CHAIN-02: per-minute option-chain fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::ChainFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: around.clone(),
                });
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduler run + supervisor
// ---------------------------------------------------------------------------

/// Sleep until this fire's moment: normally woken EARLY by the spot leg's
/// "minute completed" signal for this (or a later) boundary; bounded by
/// the fallback timer (boundary + [`CHAIN_1M_FALLBACK_DELAY_MS`]) so a
/// disabled/dead/slow spot leg never blocks the chain.
async fn wait_until_chain_fire(
    fire_secs_of_day: u32,
    now_secs_of_day: u32,
    spot_rx: &mut Option<tokio::sync::watch::Receiver<Option<u32>>>,
) {
    let sleep_ms = u64::from(fire_secs_of_day.saturating_sub(now_secs_of_day))
        .saturating_mul(1_000)
        + CHAIN_1M_FALLBACK_DELAY_MS;
    wait_for_signal_or_fallback(sleep_ms, fire_secs_of_day, spot_rx).await;
}

/// The signal-or-timer core of [`wait_until_chain_fire`], parameterized on
/// the fallback duration so the tokio tests exercise every arm with
/// millisecond timers (no `test-util` paused clock available).
/// `pub(crate)` since 2026-07-13: the Groww chain leg
/// (`groww_option_chain_1m_boot.rs`) reuses this exact core with its own
/// fallback constant — one sequencing implementation, two feeds.
// TEST-EXEMPT: exercised by test_wait_until_chain_fire_signal_wakes_early_and_fallback_bounds + the stale-wake tokio tests below (visibility widened only).
pub(crate) async fn wait_for_signal_or_fallback(
    fallback_ms: u64,
    fire_secs_of_day: u32,
    spot_rx: &mut Option<tokio::sync::watch::Receiver<Option<u32>>>,
) {
    let fallback = tokio::time::sleep(Duration::from_millis(fallback_ms));
    let Some(rx) = spot_rx.as_mut() else {
        fallback.await;
        return;
    };
    if spot_signal_satisfies(*rx.borrow(), fire_secs_of_day) {
        // The spot leg already completed this minute (we fell behind) —
        // fire immediately; the freshness gate covers pathological clocks.
        return;
    }
    tokio::pin!(fallback);
    loop {
        tokio::select! {
            () = &mut fallback => return,
            changed = rx.changed() => {
                if changed.is_err() {
                    // Every spot sender dropped (spot leg permanently
                    // gone) — the fallback timer owns pacing from here.
                    (&mut fallback).await;
                    return;
                }
                if spot_signal_satisfies(*rx.borrow(), fire_secs_of_day) {
                    return;
                }
            }
        }
    }
}

/// Run today's remaining chain minute fires, then return. Never panics;
/// every fault path logs (coded) + counts. Sets `disabled_for_day` before
/// returning on an entitlement / expirylist stop so the supervisor knows
/// NOT to respawn (the pipeline stays down for the day — never a reject
/// storm).
// TEST-EXEMPT: live-deps async runner — every scheduling / parsing / edge / classification decision is a pure fn unit-tested below (+ the reused spot pure fns); the HTTP leg mirrors the tested spot pattern; wiring pinned by crates/app/tests/option_chain_1m_wiring_guard.rs.
pub async fn run_option_chain_1m(
    params: OptionChain1mTaskParams,
    disabled_for_day: Arc<AtomicBool>,
) {
    // Idempotent DDL first (CREATE → ADD COLUMN self-heal → DEDUP ENABLE);
    // failures degrade loudly inside (CHAIN-03) and never block the run.
    ensure_option_chain_1m_table(&params.questdb).await;

    if !params.calendar.is_trading_day_today() {
        info!("option_chain_1m: non-trading day — skipping all minute fires");
        return;
    }
    let expirylist_url = join_api_url(&params.rest_api_base_url, DHAN_OPTION_CHAIN_EXPIRYLIST_PATH);
    let chain_url = join_api_url(&params.rest_api_base_url, DHAN_OPTION_CHAIN_PATH);
    // ONE long-lived client for the whole session (per-minute rebuild is
    // the exact TLS/resolver churn HTTP-CLIENT-01 §0 condemns).
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(CHAIN_1M_REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "client_build",
                ?err,
                "CHAIN-02: HTTP client build failed — per-minute chain fetch \
                 degraded; supervisor will retry after backoff"
            );
            metrics::counter!("tv_chain1m_fetch_total", "outcome" => "error").increment(1);
            return;
        }
    };

    // ---- Day-start expirylist warmup (doubles as the entitlement probe
    // for the enabled pipeline; NEVER a guessed expiry) ----
    let warmup_jwt: Option<secrecy::SecretString> = {
        let guard = params.token_handle.load();
        guard
            .as_ref()
            .as_ref()
            .map(|state| state.access_token().clone())
    };
    let Some(warmup_jwt) = warmup_jwt else {
        error!(
            code = ErrorCode::Chain04ExpirylistFailed.code_str(),
            stage = "warmup_no_token",
            "CHAIN-04: no access token at warmup time — supervisor will \
             retry after backoff"
        );
        return;
    };
    let today = today_ist();
    let targets = match resolve_current_expiries(
        &client,
        &expirylist_url,
        &warmup_jwt,
        &params.client_id,
        today,
    )
    .await
    {
        WarmupOutcome::Resolved(targets) => targets,
        WarmupOutcome::EntitlementAbsent(detail) => {
            metrics::counter!("tv_chain1m_fetch_total", "outcome" => "entitlement").increment(1);
            error!(
                code = ErrorCode::Chain01EntitlementAbsent.code_str(),
                stage = "warmup",
                detail = %detail,
                "CHAIN-01: option-chain entitlement absent (DH-902/806 \
                 class) — the chain pipeline stays DOWN for the day"
            );
            params
                .notifier
                .notify(NotificationEvent::ChainEntitlementAbsent {
                    pipeline_enabled: true,
                    detail,
                });
            disabled_for_day.store(true, Ordering::SeqCst);
            return;
        }
        WarmupOutcome::Failed(detail) => {
            metrics::counter!("tv_chain1m_expirylist_failed_total").increment(1);
            error!(
                code = ErrorCode::Chain04ExpirylistFailed.code_str(),
                stage = "warmup",
                detail = %detail,
                "CHAIN-04: day-start expirylist warmup failed after bounded \
                 retries — the chain pipeline degrades to DISABLED for the \
                 day (never a guessed expiry)"
            );
            params
                .notifier
                .notify(NotificationEvent::ChainExpirylistFailed { detail });
            disabled_for_day.store(true, Ordering::SeqCst);
            return;
        }
    };
    for t in &targets {
        info!(
            symbol = t.symbol,
            security_id = t.security_id,
            expiry = %t.expiry_str,
            "option_chain_1m: current expiry resolved from the expirylist API"
        );
    }

    let mut writer = OptionChain1mWriter::new(&params.questdb);
    let mut edge = FailureEdge::default();
    // Per-underlying not-served tracker + ladder watermarks (2026-07-14)
    // — same lifetime as the FailureEdge: this run, which is per trading
    // day; a mid-day supervisor respawn restarts the streaks.
    let mut health = ChainServingHealth::default();
    let mut last_fired: Option<u32> = None;
    let mut last_request_ms: Vec<Option<i64>> = vec![None; targets.len()];
    let mut spot_rx = params.spot_minute_done.clone();
    info!(
        underlyings = targets.len(),
        "option_chain_1m: per-minute chain fetch loop armed (fires each \
         minute close 09:16:00–15:30:00 IST, right after the spot leg)"
    );

    loop {
        // Audit Rule 3: re-read the wall clock + trading-day verdict EVERY
        // iteration (a suspend can cross midnight and stale the verdict).
        if !params.calendar.is_trading_day_today() {
            info!("option_chain_1m: no longer a trading day — exiting");
            return;
        }
        let now = ist_secs_of_day_now();
        let Some(fire) = next_fire_after(now, last_fired) else {
            info!("option_chain_1m: past 15:30 IST — today's minute fires complete");
            return;
        };
        wait_until_chain_fire(fire, now, &mut spot_rx).await;

        // Staleness gate (suspend / clock-step defense — the spot leg's
        // pattern): skip + recompute, never fetch a long-gone minute.
        let woke = ist_secs_of_day_now();
        if !fire_is_fresh(fire, woke) {
            warn!(
                fire_secs = fire,
                woke_at_secs = woke,
                "option_chain_1m: woke too far past the minute boundary \
                 (suspend/clock step?) — skipping this minute"
            );
            let missed = count_missed_boundaries(fire.saturating_sub(60), woke.saturating_sub(1));
            record_chain_skipped_boundaries(&params, &mut edge, missed, fire);
            if woke > fire {
                last_fired = Some(
                    (woke.min(tickvault_common::constants::SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST)
                        / 60)
                        * 60,
                );
            }
            let backoff_ms = stale_wake_backoff_ms(fire, woke);
            if backoff_ms > 0 {
                // Clock stepped BACK across the boundary (woke < fire):
                // the spot signal already satisfies this fire, so the
                // next wait would return with ZERO awaits — sleep up to
                // the fire moment instead of busy-spinning + storming
                // the warn! above (hostile-review H1).
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }
            continue;
        }

        let verdict = fire_one_chain_minute(
            &params,
            &client,
            &chain_url,
            &targets,
            &mut last_request_ms,
            &mut writer,
            &mut edge,
            &mut health,
            fire,
        )
        .await;
        last_fired = Some(fire);
        if let MinuteVerdict::EntitlementStop(detail) = verdict {
            error!(
                code = ErrorCode::Chain01EntitlementAbsent.code_str(),
                stage = "mid_session",
                detail = %detail,
                "CHAIN-01: option-chain entitlement reject mid-session — the \
                 chain pipeline stays DOWN for the rest of the day"
            );
            params
                .notifier
                .notify(NotificationEvent::ChainEntitlementAbsent {
                    pipeline_enabled: true,
                    detail,
                });
            disabled_for_day.store(true, Ordering::SeqCst);
            return;
        }
        // Overrun accounting: boundaries that fully elapsed DURING the
        // fire can never be fetched — count them loudly + feed the edge.
        let after = ist_secs_of_day_now();
        let missed = count_missed_boundaries(fire, after.saturating_sub(1));
        record_chain_skipped_boundaries(&params, &mut edge, missed, fire);
        if missed > 0 {
            last_fired = Some(
                (after.min(tickvault_common::constants::SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST)
                    / 60)
                    * 60,
            );
        }
    }
}

/// Spawn the supervised per-minute chain scheduler. The supervisor
/// respawns a dead/failed run after a bounded backoff, and exits cleanly
/// once today's window is over, on graceful-shutdown cancel, or after an
/// entitlement / expirylist disabled-for-the-day stop (deliberately NOT
/// respawned — the pipeline stays down; tomorrow's boot re-warms).
// TEST-EXEMPT: tokio supervisor wiring over the unit-tested pure decisions (spot_1m_day_is_over / classify_join_exit / the disabled-for-day latch); spawn site pinned by crates/app/tests/option_chain_1m_wiring_guard.rs.
pub fn spawn_supervised_option_chain_1m(
    params: OptionChain1mTaskParams,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let disabled_for_day = Arc::new(AtomicBool::new(false));
        loop {
            let inner = tokio::spawn(run_option_chain_1m(
                params.clone(),
                Arc::clone(&disabled_for_day),
            ));
            let result = inner.await;
            let reason = classify_join_exit(&result);
            let day_over = spot_1m_day_is_over(
                ist_secs_of_day_now(),
                params.calendar.is_trading_day_today(),
            );
            match &result {
                Ok(()) if disabled_for_day.load(Ordering::SeqCst) => {
                    info!(
                        "option_chain_1m: pipeline disabled for the day \
                         (entitlement / expirylist stop) — supervisor exiting"
                    );
                    return;
                }
                Ok(()) if day_over => {
                    info!("option_chain_1m: day complete — supervisor exiting");
                    return;
                }
                Err(join_err) if join_err.is_cancelled() => {
                    // Graceful shutdown teardown — not an abort.
                    return;
                }
                _ => {}
            }
            metrics::counter!("tv_chain1m_task_respawn_total", "reason" => reason).increment(1);
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "task_respawn",
                reason,
                "CHAIN-02: per-minute chain fetch task died mid-window — \
                 respawning after backoff"
            );
            tokio::time::sleep(Duration::from_secs(CHAIN_1M_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- request bodies (PascalCase, INTEGER scrip) --------------------------

    #[test]
    fn test_option_chain_request_pascal_case_underlying_scrip_int() {
        let body = chain_request_body(13, "2026-07-16");
        // UnderlyingScrip is an INTEGER (option-chain.md rule 5 — contrast
        // the charts APIs' STRING securityId), fields are PascalCase.
        assert_eq!(body["UnderlyingScrip"], 13);
        assert!(body["UnderlyingScrip"].is_u64());
        assert_eq!(body["UnderlyingSeg"], "IDX_I");
        assert_eq!(body["Expiry"], "2026-07-16");
        // No camelCase leakage.
        assert!(body.get("underlyingScrip").is_none());
        assert!(body.get("expiry").is_none());

        let el = expirylist_request_body(25);
        assert_eq!(el["UnderlyingScrip"], 25);
        assert!(el["UnderlyingScrip"].is_u64());
        assert_eq!(el["UnderlyingSeg"], "IDX_I");
        assert!(el.get("Expiry").is_none(), "expirylist has no Expiry field");
    }

    // ---- expiry list parse + current-expiry selection -------------------------

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date")
    }

    #[test]
    fn test_parse_expiry_list_happy_and_hostile() {
        let body = r#"{"data":["2026-07-16","2026-07-23","2026-07-30"],"status":"success"}"#;
        assert_eq!(
            parse_expiry_list(body),
            vec![d("2026-07-16"), d("2026-07-23"), d("2026-07-30")]
        );
        // Malformed entries are skipped, not fatal.
        let mixed = r#"{"data":["2026-07-16","not-a-date",42,null,"2026-08-27"]}"#;
        assert_eq!(
            parse_expiry_list(mixed),
            vec![d("2026-07-16"), d("2026-08-27")]
        );
        // Hostile shapes parse to empty, never panic.
        assert!(parse_expiry_list("not json").is_empty());
        assert!(parse_expiry_list("{}").is_empty());
        assert!(parse_expiry_list(r#"{"data":"2026-07-16"}"#).is_empty());
        assert!(parse_expiry_list(r#"{"data":{}}"#).is_empty());
        assert!(parse_expiry_list(r#"{"data":[]}"#).is_empty());
    }

    #[test]
    fn test_select_current_expiry_nearest_at_or_after_today() {
        let expiries = [d("2026-07-16"), d("2026-07-23"), d("2026-08-27")];
        // Mid-week: the nearest future expiry wins (order-independent).
        assert_eq!(
            select_current_expiry(&expiries, d("2026-07-12")),
            Some(d("2026-07-16"))
        );
        let shuffled = [d("2026-08-27"), d("2026-07-16"), d("2026-07-23")];
        assert_eq!(
            select_current_expiry(&shuffled, d("2026-07-12")),
            Some(d("2026-07-16"))
        );
        // Between expiries: the next one.
        assert_eq!(
            select_current_expiry(&expiries, d("2026-07-17")),
            Some(d("2026-07-23"))
        );
    }

    /// Expiry day: the SAME-DAY expiry is still the current one through
    /// the session (never rolls intraday — the house never-roll precedent;
    /// it falls out of tomorrow's warmup).
    #[test]
    fn test_select_current_expiry_on_expiry_day_keeps_same_day() {
        let expiries = [d("2026-07-16"), d("2026-07-23")];
        assert_eq!(
            select_current_expiry(&expiries, d("2026-07-16")),
            Some(d("2026-07-16"))
        );
        // The morning after: advanced automatically.
        assert_eq!(
            select_current_expiry(&expiries, d("2026-07-17")),
            Some(d("2026-07-23"))
        );
        // Every expiry past (stale master anomaly) → None, never a guess.
        assert_eq!(select_current_expiry(&expiries, d("2026-08-01")), None);
        assert_eq!(select_current_expiry(&[], d("2026-07-12")), None);
    }

    // ---- chain response parsing (decimal strikes, Option<> legs, hostile) ----

    const SAMPLE_CHAIN: &str = r#"{
        "data": {
            "last_price": 25642.8,
            "oc": {
                "25650.000000": {
                    "ce": {
                        "average_price": 146.99,
                        "greeks": {"delta": 0.53871, "theta": -15.1539,
                                   "gamma": 0.00132, "vega": 12.18593},
                        "implied_volatility": 9.789,
                        "last_price": 134,
                        "oi": 3786445,
                        "previous_close_price": 244.85,
                        "previous_oi": 402220,
                        "previous_volume": 31931705,
                        "security_id": 42528,
                        "top_ask_price": 134,
                        "top_ask_quantity": 1365,
                        "top_bid_price": 133.55,
                        "top_bid_quantity": 1625,
                        "volume": 117567970
                    },
                    "pe": {
                        "last_price": 98.5,
                        "oi": 100,
                        "volume": 200,
                        "previous_oi": 50,
                        "implied_volatility": 11.2,
                        "security_id": 42529,
                        "greeks": {"delta": -0.46, "theta": -14.1,
                                   "gamma": 0.0013, "vega": 12.1}
                    }
                },
                "25700.500000": {
                    "ce": null
                }
            }
        },
        "status": "success"
    }"#;

    #[test]
    fn test_option_chain_strike_keys_parse_decimal_strings() {
        let chain = parse_option_chain(SAMPLE_CHAIN).expect("parseable chain");
        assert_eq!(chain.underlying_spot, 25_642.8);
        assert_eq!(chain.invalid_strikes, 0);
        // Strike keys are DECIMAL STRINGS parsed as f64 — never integer.
        let ce = chain
            .legs
            .iter()
            .find(|l| l.leg == "CE")
            .expect("CE leg present");
        assert_eq!(ce.strike, 25_650.0);
        assert_eq!(ce.contract_security_id, 42_528);
        assert_eq!(ce.last_price, 134.0);
        assert_eq!(ce.implied_volatility, 9.789);
        assert_eq!(ce.delta, 0.538_71);
        assert_eq!(ce.theta, -15.153_9);
        assert_eq!(ce.gamma, 0.001_32);
        assert_eq!(ce.vega, 12.185_93);
        assert_eq!(ce.oi, 3_786_445);
        assert_eq!(ce.volume, 117_567_970);
        assert_eq!(ce.previous_oi, 402_220);
        let pe = chain
            .legs
            .iter()
            .find(|l| l.leg == "PE")
            .expect("PE leg present");
        assert_eq!(pe.strike, 25_650.0);
        assert_eq!(pe.delta, -0.46);
    }

    /// CE or PE may be absent/null on deep-OTM strikes (option-chain.md
    /// rule 7) — the absent side is skipped, never a panic; a strike with
    /// BOTH sides absent contributes zero legs.
    #[test]
    fn test_option_chain_ce_pe_optional_absent_side() {
        let chain = parse_option_chain(SAMPLE_CHAIN).expect("parseable chain");
        // The 25700.5 strike has ce=null and no pe — zero legs from it.
        assert_eq!(chain.legs.len(), 2, "only the two-sided strike's legs");
        assert!(chain.legs.iter().all(|l| l.strike == 25_650.0));
    }

    #[test]
    fn test_parse_option_chain_hostile_inputs_never_panic() {
        // Malformed top-level shapes → None (the parse-failure arm).
        assert!(parse_option_chain("not json").is_none());
        assert!(parse_option_chain("{}").is_none());
        assert!(parse_option_chain(r#"{"data": 42}"#).is_none());
        assert!(parse_option_chain(r#"{"data": {"oc": "nope"}}"#).is_none());
        assert!(parse_option_chain(r#"{"data": {"oc": []}}"#).is_none());
        // Zero strikes → Some(empty) — the outcome="empty" arm.
        let empty = parse_option_chain(r#"{"data": {"last_price": 1.0, "oc": {}}}"#)
            .expect("empty chain parses");
        assert!(empty.legs.is_empty());
        assert_eq!(empty.underlying_spot, 1.0);
        // Unparsable / non-finite strike keys are skipped + COUNTED.
        let bad_keys = r#"{"data": {"oc": {
            "abc": {"ce": {"last_price": 1}},
            "NaN": {"ce": {"last_price": 1}},
            "inf": {"ce": {"last_price": 1}},
            "100.000000": {"ce": {"last_price": 2}}
        }}}"#;
        let parsed = parse_option_chain(bad_keys).expect("parses");
        // "abc" fails parse; "NaN"/"inf" parse to non-finite f64 → skipped.
        assert_eq!(parsed.invalid_strikes, 3);
        assert_eq!(parsed.legs.len(), 1);
        assert_eq!(parsed.legs[0].strike, 100.0);
        // Field-level type wobble degrades per-field, never fails the row:
        // floats where ints are documented truncate; strings/objects → 0.
        let wobble = r#"{"data": {"oc": {"100.000000": {"pe": {
            "oi": 12.9, "volume": "junk", "last_price": {"x": 1},
            "greeks": "not an object"
        }}}}}"#;
        let parsed = parse_option_chain(wobble).expect("parses");
        assert_eq!(parsed.legs.len(), 1);
        let leg = &parsed.legs[0];
        assert_eq!(leg.leg, "PE");
        assert_eq!(leg.oi, 12, "float→int truncation fallback");
        assert_eq!(leg.volume, 0, "string degrades to 0");
        assert_eq!(leg.last_price, 0.0, "object degrades to 0.0");
        assert_eq!(leg.delta, 0.0, "non-object greeks degrade to 0.0");
        // A leg that is present but not an object is skipped.
        let non_obj_leg = r#"{"data": {"oc": {"100.000000": {"ce": 5}}}}"#;
        assert!(
            parse_option_chain(non_obj_leg)
                .expect("parses")
                .legs
                .is_empty()
        );
    }

    /// Implausible strikes (≤ 0 or ≥ the sanity ceiling) are skipped +
    /// counted with the invalid keys — never a row, never silent.
    #[test]
    fn test_parse_option_chain_implausible_strikes_skipped_and_counted() {
        let body = r#"{"data": {"oc": {
            "0.000000": {"ce": {"last_price": 1}},
            "-5.000000": {"ce": {"last_price": 1}},
            "99999999999.000000": {"ce": {"last_price": 1}},
            "25650.000000": {"ce": {"last_price": 2}}
        }}}"#;
        let parsed = parse_option_chain(body).expect("parses");
        assert_eq!(parsed.invalid_strikes, 3, "0 / negative / >=10M skipped");
        assert_eq!(parsed.legs.len(), 1);
        assert_eq!(parsed.legs[0].strike, 25_650.0);
    }

    /// Strikes past [`MAX_STRIKES_PER_CHAIN`] are dropped + counted as
    /// truncated (a hostile body inside the byte cap can never mint
    /// unbounded rows).
    #[test]
    fn test_parse_option_chain_strike_cap_truncates_and_counts() {
        let mut entries: Vec<String> = Vec::new();
        for i in 0..(MAX_STRIKES_PER_CHAIN + 7) {
            entries.push(format!(
                r#""{}.000000": {{"ce": {{"last_price": 1}}}}"#,
                10_000 + i
            ));
        }
        let body = format!(r#"{{"data": {{"oc": {{ {} }}}}}}"#, entries.join(","));
        let parsed = parse_option_chain(&body).expect("parses");
        assert_eq!(parsed.truncated_strikes, 7);
        assert_eq!(parsed.legs.len(), MAX_STRIKES_PER_CHAIN);
        assert_eq!(parsed.invalid_strikes, 0);
    }

    /// serde_json's built-in recursion limit (128) makes deeply-nested
    /// hostile JSON ERROR instead of overflowing the stack — verified
    /// here as the depth guard: ~200-deep nesting returns `None`, no
    /// abort (hostile-review recursion-depth follow-up; evidence that no
    /// extra depth guard is needed on this parse path).
    #[test]
    fn test_parse_option_chain_deep_nesting_errors_cleanly_no_overflow() {
        let deep = format!("{{\"data\":{}{}}}", "[".repeat(200), "]".repeat(200));
        assert!(parse_option_chain(&deep).is_none());
        // Same via a deeply nested OBJECT chain.
        let deep_obj = format!("{}\"x\"{}", "{\"a\":".repeat(200), "}".repeat(200));
        assert!(parse_option_chain(&deep_obj).is_none());
    }

    // ---- entitlement classification ------------------------------------------

    #[test]
    fn test_is_entitlement_reject_classification() {
        // DH-902 / DATA 806 named in the body → entitlement, any status.
        assert!(is_entitlement_reject(
            400,
            r#"{"errorType":"x","errorCode":"DH-902","errorMessage":"no access"}"#
        ));
        assert!(is_entitlement_reject(
            401,
            r#"{"errorCode":"806","errorMessage":"Data APIs not subscribed"}"#
        ));
        // 401/403 whose DHAN-SHAPED error body names the subscription →
        // entitlement (the shape gate: errorCode/errorType present).
        assert!(is_entitlement_reject(
            403,
            r#"{"errorType":"Access","errorCode":"DH-XYZ","errorMessage":"user has not subscribed to Data APIs"}"#
        ));
        assert!(is_entitlement_reject(
            401,
            r#"{"errorCode":"X","errorMessage":"Data APIs not enabled"}"#
        ));
        // A WAF/gateway HTML 403 mentioning "subscription" is NOT Dhan's
        // error shape → TRANSIENT, never a day-killing entitlement
        // verdict (hostile-review M2 — the false-positive direction).
        assert!(!is_entitlement_reject(
            403,
            "<html><body>Access denied. See our subscription plans for subscribers.</body></html>"
        ));
        // Un-shaped plain text with the wording is likewise transient.
        assert!(!is_entitlement_reject(
            403,
            "user has not subscribed to Data APIs"
        ));
        // Dhan-shaped 401 WITHOUT the wording is a token-class transient.
        assert!(!is_entitlement_reject(
            401,
            r#"{"errorType":"Auth","errorCode":"DH-901","errorMessage":"invalid token"}"#
        ));
        // A bare 401 (token-expired class) is TRANSIENT — the renewal
        // machinery owns it; never an entitlement verdict.
        assert!(!is_entitlement_reject(401, "invalid token"));
        assert!(!is_entitlement_reject(401, ""));
        // Rate limits / server errors are transient.
        assert!(!is_entitlement_reject(429, "too many requests"));
        assert!(!is_entitlement_reject(500, "internal error"));
        // "806" must be the quoted code, not any number in the body.
        assert!(!is_entitlement_reject(400, r#"{"took_ms": 806}"#));
        // The shape helper itself: object with errorCode/errorType = yes;
        // arrays / plain text / other objects = no.
        assert!(body_has_dhan_error_shape(r#"{"errorCode":"DH-902"}"#));
        assert!(body_has_dhan_error_shape(r#"{"errorType":"Access"}"#));
        assert!(!body_has_dhan_error_shape(r#"{"status":"failed"}"#));
        assert!(!body_has_dhan_error_shape(r#"["errorCode"]"#));
        assert!(!body_has_dhan_error_shape("<html>errorCode</html>"));
    }

    // ---- pacing (1 unique request / 3s per underlying) -------------------------

    #[test]
    fn test_option_chain_pacing_one_unique_per_3s() {
        // No prior request → no wait.
        assert_eq!(min_gap_wait_ms(None, 1_000_000), 0);
        // 1s since the last request to the SAME underlying → wait 2s more.
        assert_eq!(min_gap_wait_ms(Some(1_000_000), 1_001_000), 2_000);
        // Exactly at the gap → no wait.
        assert_eq!(min_gap_wait_ms(Some(1_000_000), 1_003_000), 0);
        // Well past the gap (the normal ~60s per-minute cadence) → none.
        assert_eq!(min_gap_wait_ms(Some(1_000_000), 1_060_000), 0);
        // Same-instant double fire → the full gap.
        assert_eq!(min_gap_wait_ms(Some(1_000_000), 1_000_000), 3_000);
        // Backwards/midnight-wrapped clock → 0, never a spurious sleep.
        assert_eq!(min_gap_wait_ms(Some(86_000_000), 5_000), 0);
    }

    // ---- spot→chain sequencing signal ----------------------------------------

    #[test]
    fn test_spot_signal_satisfies_minute_ordering() {
        let fire = 10 * 3600; // 10:00:00 boundary
        // No signal yet (spot never fired) → not satisfied.
        assert!(!spot_signal_satisfies(None, fire));
        // An older minute's signal → not satisfied (spot still working).
        assert!(!spot_signal_satisfies(Some(fire - 60), fire));
        // This minute's completion → satisfied.
        assert!(spot_signal_satisfies(Some(fire), fire));
        // A LATER minute (chain fell behind) → satisfied too.
        assert!(spot_signal_satisfies(Some(fire + 60), fire));
    }

    /// The wait core itself (real-time tokio, millisecond fallbacks — the
    /// workspace tokio has no `test-util` paused clock): a spot signal for
    /// the fire minute wakes the chain EARLY; without one the fallback
    /// timer fires; a dropped spot sender degrades to the fallback timer
    /// (never a hang).
    #[tokio::test]
    async fn test_wait_until_chain_fire_signal_wakes_early_and_fallback_bounds() {
        use tokio::sync::watch;
        let fire = 10 * 3600;
        const FALLBACK_MS: u64 = 200;
        let fb = Duration::from_millis(FALLBACK_MS);

        // (a) No receiver (spot disabled): the fallback timer paces.
        let started = std::time::Instant::now();
        wait_for_signal_or_fallback(FALLBACK_MS, fire, &mut None).await;
        assert!(started.elapsed() >= fb, "must wait the full fallback");

        // (b) Spot signals this minute → wakes on the signal, early.
        let (tx, rx) = watch::channel::<Option<u32>>(None);
        let mut rx_opt = Some(rx);
        let waiter = tokio::spawn(async move {
            let started = std::time::Instant::now();
            wait_for_signal_or_fallback(FALLBACK_MS, fire, &mut rx_opt).await;
            started.elapsed()
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        tx.send_replace(Some(fire));
        let waited = waiter.await.expect("waiter completes");
        assert!(
            waited < fb,
            "signal must wake the chain before the fallback: {waited:?}"
        );

        // (c) A signal for an OLDER minute does NOT wake it; the fallback
        // still bounds the wait.
        let (tx, rx) = watch::channel::<Option<u32>>(Some(fire - 60));
        let mut rx_opt = Some(rx);
        let waiter = tokio::spawn(async move {
            let started = std::time::Instant::now();
            wait_for_signal_or_fallback(FALLBACK_MS, fire, &mut rx_opt).await;
            started.elapsed()
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        tx.send_replace(Some(fire - 60)); // still the old minute
        let waited = waiter.await.expect("waiter completes");
        assert!(
            waited >= fb,
            "old-minute signal must NOT wake it: {waited:?}"
        );

        // (d) Sender dropped mid-wait → degrades to the fallback timer
        // (never a hang, never an instant spurious fire).
        let (tx, rx) = watch::channel::<Option<u32>>(None);
        let mut rx_opt = Some(rx);
        let waiter = tokio::spawn(async move {
            let started = std::time::Instant::now();
            wait_for_signal_or_fallback(FALLBACK_MS, fire, &mut rx_opt).await;
            started.elapsed()
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(tx);
        let waited = waiter.await.expect("waiter completes");
        assert!(
            waited >= fb,
            "dropped sender degrades to the timer: {waited:?}"
        );

        // (e) Spot ALREADY completed this minute (chain fell behind) →
        // immediate fire, no fallback sleep.
        let (tx, rx) = watch::channel::<Option<u32>>(Some(fire));
        let mut rx_opt = Some(rx);
        let started = std::time::Instant::now();
        wait_for_signal_or_fallback(FALLBACK_MS, fire, &mut rx_opt).await;
        assert!(started.elapsed() < fb, "already-signaled fires immediately");
        drop(tx);

        // The production wrapper feeds the constant-derived fallback into
        // the same core (no receiver → pure timer; tiny boundary delta).
        let started = std::time::Instant::now();
        wait_until_chain_fire(fire, fire, &mut None).await;
        assert!(
            started.elapsed() >= Duration::from_millis(CHAIN_1M_FALLBACK_DELAY_MS),
            "wrapper honors the constant fallback"
        );
    }

    /// Clock-step-BACK defense (hostile-review H1): when a stale wake
    /// lands BEFORE the fire moment (NTP step / VM pause-resume) with the
    /// spot signal already satisfied, the pre-satisfied wait returns with
    /// ZERO awaits — the stale arm's `stale_wake_backoff_ms` sleep is
    /// what breaks the busy-spin: non-zero exactly when `woke < fire`,
    /// and the loop actually awaits it.
    #[tokio::test]
    async fn test_clock_step_back_stale_arm_awaits_instead_of_busy_spin() {
        use tokio::sync::watch;
        let fire: u32 = 560 * 60; // 09:20:00 IST
        // The spin ingredient: a pre-satisfied signal returns instantly
        // (no await at all), so the loop alone would spin at CPU speed.
        let (tx, rx) = watch::channel::<Option<u32>>(Some(fire));
        let mut rx_opt = Some(rx);
        let started = std::time::Instant::now();
        wait_for_signal_or_fallback(5_000, fire, &mut rx_opt).await;
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "pre-satisfied signal returns without awaiting"
        );
        drop(tx);
        // The backoff is non-zero precisely for the step-back shape…
        assert_eq!(stale_wake_backoff_ms(fire, fire - 3), 3_000);
        assert_eq!(stale_wake_backoff_ms(fire, fire.saturating_sub(1)), 1_000);
        // …and zero on the normal stale-forward / exact-boundary shapes
        // (those re-arm via `last_fired`, no sleep needed).
        assert_eq!(stale_wake_backoff_ms(fire, fire), 0);
        assert_eq!(stale_wake_backoff_ms(fire, fire + 90), 0);
        // The loop-shaped composition: pre-satisfied wait + the stale
        // arm's backoff sleep — one iteration now awaits >= the step gap
        // instead of spinning (millisecond-scaled for the test).
        let (tx, rx) = watch::channel::<Option<u32>>(Some(fire));
        let mut rx_opt = Some(rx);
        let started = std::time::Instant::now();
        wait_for_signal_or_fallback(5_000, fire, &mut rx_opt).await;
        let backoff_ms = stale_wake_backoff_ms(fire, fire - 1).min(120);
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the stale-arm backoff bounds the loop iteration"
        );
        drop(tx);
    }

    // ---- edge verdict ----------------------------------------------------------

    #[test]
    fn test_chain_minute_fully_failed_requires_fetch_and_persist_ok() {
        assert!(!chain_minute_fully_failed(3, false));
        assert!(!chain_minute_fully_failed(1, false));
        assert!(chain_minute_fully_failed(0, false));
        // Fetched but the persist leg failed → STILL failed for the edge.
        assert!(chain_minute_fully_failed(3, true));
        assert!(chain_minute_fully_failed(0, true));
    }

    /// The expirylist warmup fail-closed contract: with every attempt
    /// failing (unreachable server), the warmup ends `Failed` — never a
    /// guessed expiry, never a resolved set. Real-time (~9 s of constant
    /// backoffs — the workspace tokio has no `test-util` paused clock);
    /// port-1 connection refusals themselves are instant.
    #[tokio::test]
    async fn test_expirylist_warmup_fail_closed_keeps_leg_dormant() {
        // Port 1 is reserved and never listening — a real transport error
        // on every attempt.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(250))
            .build()
            .expect("client");
        let jwt = secrecy::SecretString::from("test-jwt");
        let outcome = resolve_current_expiries(
            &client,
            "http://127.0.0.1:1/",
            &jwt,
            "1106656882",
            d("2026-07-12"),
        )
        .await;
        match outcome {
            WarmupOutcome::Failed(detail) => {
                assert!(
                    detail.contains("sid 13"),
                    "first underlying named: {detail}"
                );
            }
            WarmupOutcome::Resolved(_) => panic!("must not resolve without the API"),
            WarmupOutcome::EntitlementAbsent(_) => {
                panic!("transport failure is not an entitlement verdict")
            }
        }
    }

    // ---- bounded retry + decision ceiling (2026-07-14 hardening) --------------

    /// Test helper: one resolved underlying with a fixed expiry.
    fn ru(symbol: &'static str, sid: u64) -> ResolvedUnderlying {
        ResolvedUnderlying {
            security_id: sid,
            symbol,
            expiry: d("2026-07-16"),
            expiry_str: "2026-07-16".to_string(),
        }
    }

    #[test]
    fn test_chain_retry_allowed_below_ceiling() {
        assert!(chain_retry_allowed(0));
        assert!(chain_retry_allowed(1));
        // The fast path: empties known ~2.8s, retry launch ~5.5s.
        assert!(chain_retry_allowed(5_500));
        // The worst timed-out first attempt (P1): 12.5s is still inside.
        assert!(chain_retry_allowed(12_500));
        assert!(chain_retry_allowed(
            (CHAIN_1M_DECISION_CEILING_SECS as i64) * 1_000 - 1
        ));
    }

    #[test]
    fn test_chain_retry_allowed_refused_at_and_past_ceiling() {
        let ceiling_ms = (CHAIN_1M_DECISION_CEILING_SECS as i64) * 1_000;
        // The ceiling itself is EXCLUSIVE (a 15.000s launch is refused).
        assert!(!chain_retry_allowed(ceiling_ms));
        assert!(!chain_retry_allowed(ceiling_ms + 1));
        assert!(!chain_retry_allowed(ceiling_ms + 60_000));
        assert!(!chain_retry_allowed(i64::MAX));
    }

    #[test]
    fn test_chain_retry_allowed_refused_negative_elapsed() {
        // A clock step-back can only produce a negative elapsed — refuse
        // (the stale-wake machinery owns that shape, never a retry).
        assert!(!chain_retry_allowed(-1));
        assert!(!chain_retry_allowed(-15_000));
        assert!(!chain_retry_allowed(i64::MIN));
    }

    /// The launch gate includes the same-key ≥3s gap wait STILL AHEAD of
    /// the retry: a wall clock inside the ceiling whose pending gap wait
    /// crosses it is REFUSED — the fetch never launches.
    #[tokio::test]
    async fn test_retry_gate_includes_min_gap_wait() {
        let minute_close_ms = 1_000_000_i64;
        // 12.6s after the close: inside the 15s ceiling ON ITS OWN…
        let now_ms = minute_close_ms + 12_600;
        assert!(chain_retry_allowed(now_ms - minute_close_ms));
        // …but the same-key stamp 600ms ago forces a 2.4s gap wait, so
        // the LAUNCH would land at exactly 15.0s — refused.
        let gap_wait = min_gap_wait_ms(Some(now_ms - 600), now_ms);
        assert_eq!(gap_wait, 2_400);
        let elapsed_at_launch = retry_launch_elapsed_ms(now_ms, minute_close_ms, gap_wait);
        assert_eq!(elapsed_at_launch, 15_000);
        assert!(!chain_retry_allowed(elapsed_at_launch));

        // The pass itself refuses: fetch is NEVER invoked, the outcome
        // and the request stamp stay untouched.
        let targets = vec![ru("NIFTY", 13)];
        let mut outcomes = vec![(0_usize, ChainFetchOutcome::Empty)];
        let mut last_request_ms = vec![Some(now_ms - 600)];
        let mut calls = 0_u32;
        chain_retry_pass(
            &mut outcomes,
            &targets,
            &mut last_request_ms,
            minute_close_ms,
            "9:16 AM",
            move || now_ms,
            |_, _| {
                calls += 1;
                async move { (ChainFetchOutcome::Empty, 0_i64) }
            },
        )
        .await;
        assert_eq!(calls, 0, "ceiling-refused retry must never launch");
        assert_eq!(outcomes[0].1, ChainFetchOutcome::Empty);
        assert_eq!(last_request_ms[0], Some(now_ms - 600));
    }

    #[test]
    fn test_retry_only_for_empty_or_failed_never_entitlement_or_found() {
        let launchable = 5_000_i64; // well inside the ceiling
        assert_eq!(
            chain_retry_decision(&ChainFetchOutcome::Empty, launchable),
            RetryDecision::Launch
        );
        assert_eq!(
            chain_retry_decision(
                &ChainFetchOutcome::Failed("send: x".to_string()),
                launchable
            ),
            RetryDecision::Launch
        );
        // Found is final — never re-fetched.
        assert_eq!(
            chain_retry_decision(
                &ChainFetchOutcome::Found {
                    chain: ParsedChain::default(),
                    close_to_data_ms: 1_500,
                },
                launchable
            ),
            RetryDecision::Keep
        );
        // Entitlement is a day-scoped account verdict — retrying it would
        // earn the same reject (CHAIN-01 owns the day).
        assert_eq!(
            chain_retry_decision(
                &ChainFetchOutcome::Entitlement("DH-902".to_string()),
                launchable
            ),
            RetryDecision::Keep
        );
        // Eligible-but-late is the counted refusal.
        assert_eq!(
            chain_retry_decision(&ChainFetchOutcome::Empty, 15_000),
            RetryDecision::SkippedCeiling
        );
    }

    /// A recovered retry REPLACES the first-pass outcome for all
    /// downstream processing, and stamps the same-key request time.
    #[tokio::test]
    async fn test_retry_found_replaces_first_pass_outcome() {
        let minute_close_ms = 1_000_000_i64;
        let targets = vec![ru("NIFTY", 13)];
        let mut outcomes = vec![(0_usize, ChainFetchOutcome::Empty)];
        // Prior request 4s ago — the gap is already clear.
        let mut last_request_ms = vec![Some(minute_close_ms)];
        let mut calls = 0_u32;
        chain_retry_pass(
            &mut outcomes,
            &targets,
            &mut last_request_ms,
            minute_close_ms,
            "9:16 AM",
            move || minute_close_ms + 4_000,
            |_, _| {
                calls += 1;
                async move {
                    (
                        ChainFetchOutcome::Found {
                            chain: ParsedChain::default(),
                            close_to_data_ms: 6_000,
                        },
                        1_007_000_i64,
                    )
                }
            },
        )
        .await;
        assert_eq!(calls, 1);
        assert!(matches!(
            outcomes[0].1,
            ChainFetchOutcome::Found {
                close_to_data_ms: 6_000,
                ..
            }
        ));
        assert_eq!(
            last_request_ms[0],
            Some(1_007_000),
            "the retry stamps the same-key request time"
        );
    }

    /// A retry answering with the ENTITLEMENT class is honored — the
    /// final outcome becomes Entitlement, which the (unchanged) phase-3
    /// match turns into MinuteVerdict::EntitlementStop exactly like a
    /// pass-1 entitlement (CHAIN-01 owns the day).
    #[tokio::test]
    async fn test_retry_entitlement_outcome_becomes_entitlement_stop() {
        let minute_close_ms = 1_000_000_i64;
        let targets = vec![ru("BANKNIFTY", 25)];
        let mut outcomes = vec![(0_usize, ChainFetchOutcome::Failed("http 500 …".to_string()))];
        let mut last_request_ms = vec![None];
        chain_retry_pass(
            &mut outcomes,
            &targets,
            &mut last_request_ms,
            minute_close_ms,
            "9:16 AM",
            move || minute_close_ms + 3_000,
            |_, _| async move {
                (
                    ChainFetchOutcome::Entitlement("DH-902".to_string()),
                    1_003_000_i64,
                )
            },
        )
        .await;
        assert_eq!(
            outcomes[0].1,
            ChainFetchOutcome::Entitlement("DH-902".to_string()),
            "the retry's entitlement verdict replaces the transport failure"
        );
        // And an Entitlement outcome is never itself retried.
        assert_eq!(
            chain_retry_decision(&outcomes[0].1, 3_000),
            RetryDecision::Keep
        );
    }

    /// The retry pass walks each first-pass outcome exactly once — at
    /// most ONE retry per underlying per minute, even when every retry
    /// still fails (a still-failed retry is never re-retried).
    #[tokio::test]
    async fn test_at_most_one_retry_per_underlying_per_minute() {
        let minute_close_ms = 1_000_000_i64;
        let targets = vec![ru("NIFTY", 13), ru("BANKNIFTY", 25), ru("SENSEX", 51)];
        let mut outcomes = vec![
            (0_usize, ChainFetchOutcome::Empty),
            (
                1_usize,
                ChainFetchOutcome::Found {
                    chain: ParsedChain::default(),
                    close_to_data_ms: 1_500,
                },
            ),
            (2_usize, ChainFetchOutcome::Failed("send: y".to_string())),
        ];
        let mut last_request_ms: Vec<Option<i64>> = vec![None, None, None];
        let mut called: Vec<&'static str> = Vec::new();
        chain_retry_pass(
            &mut outcomes,
            &targets,
            &mut last_request_ms,
            minute_close_ms,
            "9:16 AM",
            move || minute_close_ms + 3_000,
            |target, _| {
                called.push(target.symbol);
                async move { (ChainFetchOutcome::Empty, 1_003_000_i64) }
            },
        )
        .await;
        // Exactly the two eligible underlyings, once each; the Found one
        // is never re-fetched.
        assert_eq!(called, vec!["NIFTY", "SENSEX"]);
        // Still-failed retries keep the FINAL (retry) outcome and are not
        // retried again within the pass.
        assert_eq!(outcomes[0].1, ChainFetchOutcome::Empty);
        assert!(matches!(outcomes[1].1, ChainFetchOutcome::Found { .. }));
        assert_eq!(outcomes[2].1, ChainFetchOutcome::Empty);
    }

    // ---- per-underlying not-served tracker (2026-07-14, #1537 mirror) ---------

    const NOT_SERVED_N: u32 = CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD;

    /// The incident shape: ok=2/empty=1 for the full threshold → exactly
    /// ONE page for the empty underlying at the threshold minute.
    #[test]
    fn test_underlying_tracker_pages_at_threshold_with_sibling_ok() {
        let mut tracker = UnderlyingServedTracker::default();
        let minute = [("NIFTY", false), ("BANKNIFTY", true), ("SENSEX", true)];
        for i in 1..NOT_SERVED_N {
            let actions = tracker.record_minute(&minute);
            assert_eq!(
                actions[0],
                ("NIFTY", UnderlyingEdgeAction::None),
                "no page below the threshold (counted minute {i})"
            );
            assert_eq!(actions[1].1, UnderlyingEdgeAction::None);
            assert_eq!(actions[2].1, UnderlyingEdgeAction::None);
        }
        let actions = tracker.record_minute(&minute);
        assert_eq!(
            actions[0],
            (
                "NIFTY",
                UnderlyingEdgeAction::Page {
                    consecutive: NOT_SERVED_N
                }
            )
        );
    }

    /// Global-failure minutes (zero served — the escalation edge's class)
    /// interleaved mid-streak neither count nor reset: the streak
    /// survives the blip and still pages after the SAME total of counted
    /// minutes.
    #[test]
    fn test_underlying_tracker_global_outage_minute_holds() {
        let mut tracker = UnderlyingServedTracker::default();
        let counted = [("NIFTY", false), ("BANKNIFTY", true), ("SENSEX", true)];
        let global = [("NIFTY", false), ("BANKNIFTY", false), ("SENSEX", false)];
        for _ in 1..NOT_SERVED_N {
            assert_eq!(
                tracker.record_minute(&counted)[0].1,
                UnderlyingEdgeAction::None
            );
            // The interleaved global-outage minute: HOLD for everyone.
            for &(_, action) in &tracker.record_minute(&global) {
                assert_eq!(action, UnderlyingEdgeAction::None);
            }
        }
        // The streak survived every HOLD: the NEXT counted minute pages.
        assert_eq!(
            tracker.record_minute(&counted)[0].1,
            UnderlyingEdgeAction::Page {
                consecutive: NOT_SERVED_N
            }
        );
    }

    /// Recovery after the latch → exactly ONE Recover carrying the
    /// episode length, latch cleared, and a NEW streak can page again.
    #[test]
    fn test_underlying_tracker_recovery_resets_and_pings_once() {
        let mut tracker = UnderlyingServedTracker::default();
        let counted = [("NIFTY", false), ("BANKNIFTY", true)];
        for _ in 0..NOT_SERVED_N {
            tracker.record_minute(&counted);
        }
        // Two more counted minutes while latched (episode length grows).
        tracker.record_minute(&counted);
        tracker.record_minute(&counted);
        // NIFTY served again → ONE Recover with the full episode length.
        let actions = tracker.record_minute(&[("NIFTY", true), ("BANKNIFTY", true)]);
        assert_eq!(
            actions[0],
            (
                "NIFTY",
                UnderlyingEdgeAction::Recover {
                    not_served_minutes: NOT_SERVED_N + 2
                }
            )
        );
        // A second served minute is NOT a second recovery.
        let actions = tracker.record_minute(&[("NIFTY", true), ("BANKNIFTY", true)]);
        assert_eq!(actions[0].1, UnderlyingEdgeAction::None);
        // A fresh streak pages again at the full threshold.
        for _ in 1..NOT_SERVED_N {
            assert_eq!(
                tracker.record_minute(&counted)[0].1,
                UnderlyingEdgeAction::None
            );
        }
        assert_eq!(
            tracker.record_minute(&counted)[0].1,
            UnderlyingEdgeAction::Page {
                consecutive: NOT_SERVED_N
            }
        );
    }

    /// Minutes 11+ of a latched episode stay counted but never re-page.
    #[test]
    fn test_underlying_tracker_no_double_page_while_latched() {
        let mut tracker = UnderlyingServedTracker::default();
        let counted = [("SENSEX", false), ("NIFTY", true)];
        for _ in 0..NOT_SERVED_N {
            tracker.record_minute(&counted);
        }
        for _ in 0..5 {
            let actions = tracker.record_minute(&counted);
            assert_eq!(
                actions[0].1,
                UnderlyingEdgeAction::None,
                "latched episode never re-pages"
            );
        }
    }

    /// An underlying ABSENT from a minute's verdicts (the unwind-build
    /// join-failure arm) is untouched — a per-underlying HOLD: the streak
    /// neither advances nor resets.
    #[test]
    fn test_underlying_tracker_missing_verdict_is_hold() {
        let mut tracker = UnderlyingServedTracker::default();
        let counted = [("NIFTY", false), ("BANKNIFTY", true)];
        for _ in 1..NOT_SERVED_N {
            tracker.record_minute(&counted);
        }
        // A minute where NIFTY has NO verdict at all (join failure) —
        // its streak is untouched either way.
        let actions = tracker.record_minute(&[("BANKNIFTY", true), ("SENSEX", false)]);
        assert!(actions.iter().all(|&(u, _)| u != "NIFTY"));
        // The next counted minute completes the ORIGINAL streak: page at
        // exactly the threshold — the hold neither counted nor reset.
        assert_eq!(
            tracker.record_minute(&counted)[0].1,
            UnderlyingEdgeAction::Page {
                consecutive: NOT_SERVED_N
            }
        );
    }

    /// The two paging edges are disjoint for the FETCH-failure class: a
    /// tracker-counted minute requires ok >= 1, which makes the
    /// escalation edge's fully-failed verdict FALSE for the same minute
    /// (absent a persist failure); a global-outage minute (the
    /// escalation's class) is a tracker HOLD.
    #[test]
    fn test_underlying_tracker_disjoint_from_escalation_edge() {
        // Tracker-counted shape: 1 ok + 1 empty → the escalation edge
        // does NOT count it (fetch class).
        assert!(!chain_minute_fully_failed(1, false));
        let mut tracker = UnderlyingServedTracker::default();
        let actions = tracker.record_minute(&[("NIFTY", false), ("BANKNIFTY", true)]);
        assert_eq!(actions[0].1, UnderlyingEdgeAction::None); // counted, sub-threshold
        // Escalation shape: ok == 0 → fully failed, and the tracker HOLDs.
        assert!(chain_minute_fully_failed(0, false));
        let mut hold = UnderlyingServedTracker::default();
        for _ in 0..(NOT_SERVED_N * 2) {
            for &(_, action) in &hold.record_minute(&[("NIFTY", false), ("BANKNIFTY", false)]) {
                assert_eq!(action, UnderlyingEdgeAction::None, "global outage = HOLD");
            }
        }
        // The honest documented overlap: persist-failed with ok >= 1
        // makes the escalation edge count it (M1 gate) WHILE an empty
        // sibling counts here — two distinct wanted signals.
        assert!(chain_minute_fully_failed(1, true));
    }

    // ---- strike-ladder day-max shrink watermark (2026-07-14, log-only) --------

    #[test]
    fn test_ladder_watermark_ratchets_up() {
        let mut ladder = LadderWatermark::default();
        assert_eq!(ladder.observe(100), LadderAction::Normal);
        assert_eq!(ladder.day_max(), 100);
        assert_eq!(ladder.observe(150), LadderAction::Normal);
        assert_eq!(ladder.day_max(), 150);
        // A smaller (but not shrunk) count never lowers the watermark.
        assert_eq!(ladder.observe(120), LadderAction::Normal);
        assert_eq!(ladder.day_max(), 150);
    }

    #[test]
    fn test_ladder_shrunk_below_half_day_max() {
        // The pure predicate: strictly below ⌈day_max/2⌉.
        assert!(!ladder_shrunk(0, 0), "day-first-Found seeds, never flags");
        assert!(!ladder_shrunk(100, 50));
        assert!(ladder_shrunk(100, 49));
        // Odd day-max: ⌈151/2⌉ = 76 — 76 is fine, 75 is shrunk.
        assert!(!ladder_shrunk(151, 76));
        assert!(ladder_shrunk(151, 75));
        // The watermark emits the FIRST shrunk minute of an episode.
        let mut ladder = LadderWatermark::default();
        assert_eq!(ladder.observe(160), LadderAction::Normal);
        assert_eq!(ladder.observe(60), LadderAction::ShrunkFirst);
        assert_eq!(ladder.observe(60), LadderAction::ShrunkLatched);
    }

    /// An all-day-small chain never flags: the day's first Found seeds
    /// the watermark, and a steady small count stays Normal.
    #[test]
    fn test_ladder_day_start_small_never_flags() {
        let mut ladder = LadderWatermark::default();
        for _ in 0..10 {
            assert_eq!(ladder.observe(12), LadderAction::Normal);
        }
        assert_eq!(ladder.day_max(), 12);
    }

    /// One warn per episode: shrunk minutes stay counter-only after the
    /// first, and a silent (non-shrunk) recovery clears the latch so a
    /// LATER shrink is a fresh episode.
    #[test]
    fn test_ladder_shrunk_latch_once_per_episode_clears_on_recovery() {
        let mut ladder = LadderWatermark::default();
        assert_eq!(ladder.observe(200), LadderAction::Normal);
        assert_eq!(ladder.observe(80), LadderAction::ShrunkFirst);
        assert_eq!(ladder.observe(70), LadderAction::ShrunkLatched);
        assert_eq!(ladder.observe(90), LadderAction::ShrunkLatched);
        // Recovery (>= half the day-max) silently clears the latch…
        assert_eq!(ladder.observe(150), LadderAction::Normal);
        // …and a later shrink is a NEW episode (one fresh warn).
        assert_eq!(ladder.observe(80), LadderAction::ShrunkFirst);
    }

    // ---- ParsedChain.strike_count (the watermark's input) ----------------------

    #[test]
    fn test_parse_option_chain_counts_kept_strikes() {
        let chain = parse_option_chain(SAMPLE_CHAIN).expect("parseable chain");
        // SAMPLE_CHAIN carries two parseable strikes (25650 two-sided +
        // 25700.5 with both legs absent — the strike still KEPT).
        assert_eq!(chain.strike_count, 2);
        // A zero-strike chain counts zero.
        let empty = parse_option_chain(r#"{"data": {"last_price": 1.0, "oc": {}}}"#)
            .expect("empty chain parses");
        assert_eq!(empty.strike_count, 0);
    }

    #[test]
    fn test_parse_strike_count_excludes_invalid_and_truncated() {
        // Invalid keys are excluded from the kept count.
        let bad_keys = r#"{"data": {"oc": {
            "abc": {"ce": {"last_price": 1}},
            "-5.000000": {"ce": {"last_price": 1}},
            "100.000000": {"ce": {"last_price": 2}}
        }}}"#;
        let parsed = parse_option_chain(bad_keys).expect("parses");
        assert_eq!(parsed.invalid_strikes, 2);
        assert_eq!(parsed.strike_count, 1);
        // Truncated strikes are excluded too: kept == the cap.
        let mut entries: Vec<String> = Vec::new();
        for i in 0..(MAX_STRIKES_PER_CHAIN + 3) {
            entries.push(format!(
                r#""{}.000000": {{"ce": {{"last_price": 1}}}}"#,
                10_000 + i
            ));
        }
        let body = format!(r#"{{"data": {{"oc": {{ {} }}}}}}"#, entries.join(","));
        let parsed = parse_option_chain(&body).expect("parses");
        assert_eq!(parsed.truncated_strikes, 3);
        assert_eq!(
            parsed.strike_count,
            u32::try_from(MAX_STRIKES_PER_CHAIN).expect("cap fits u32")
        );
    }
}
