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
    CHAIN_1M_CONSECUTIVE_FAIL_PAGE_THRESHOLD, CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS,
    CHAIN_1M_FALLBACK_DELAY_MS, CHAIN_1M_MAX_BODY_BYTES, CHAIN_1M_MIN_GAP_SECS,
    CHAIN_1M_REQUEST_TIMEOUT_SECS, CHAIN_1M_UNDERLYING_BUDGET_SECS, CHAIN_1M_UNDERLYINGS,
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
    OPTION_CHAIN_1M_FEED_DHAN, OPTION_CHAIN_1M_LEG_CE, OPTION_CHAIN_1M_LEG_PE,
    OPTION_CHAIN_1M_SEGMENT_IDX_I, OptionChain1mRow, OptionChain1mWriter,
    ensure_option_chain_1m_table,
};
use tickvault_storage::rest_fetch_audit_persistence::{
    REST_FETCH_LEG_CHAIN_1M, RestFetchAuditRow, RestFetchAuditWriter, RestFetchOutcome,
    ensure_rest_fetch_audit_table,
};

use crate::spot_1m_rest_boot::{
    EdgeAction, FailureEdge, accumulation_within_cap, count_missed_boundaries,
    declared_len_within_cap, fire_is_fresh, format_minute_ist_12h, minute_open_ist_nanos,
    next_fire_after, spot_1m_day_is_over, stamp_held_ok_rows,
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

/// The chain edge's "fully failed" verdict for one fired minute — no
/// underlying succeeded, OR the persist leg (append/flush) failed. A
/// fetched-but-never-persisted minute is NOT ok (the spot M1 precedent).
#[must_use]
pub fn chain_minute_fully_failed(ok_count: usize, persist_failed: bool) -> bool {
    ok_count == 0 || persist_failed
}

// ---------------------------------------------------------------------------
// GAP-11 forensics helpers (rest_fetch_audit, leg='chain_1m', feed='dhan')
// ---------------------------------------------------------------------------

/// Build one `rest_fetch_audit` row for a Dhan chain fetch — `leg =
/// chain_1m`, `attempts = 1` for a ran fetch (live-snapshot semantics: the
/// chain has no in-minute re-poll ladder), keyed on the UNDERLYING's
/// stable identity + plain symbol (the Groww chain precedent). The Dhan
/// chain fetch surfaces no per-attempt forensics struct
/// (`fetch_chain_bounded` returns a typed outcome only), so
/// `final_http_status` carries 200 for the 2xx-proven arms (Found/Empty)
/// and the 0 sentinel otherwise, and `fetch_latency_ms` stays the -1
/// sentinel (measured only into the `tv_chain1m_fetch_duration_ms`
/// histogram — threading it out of the ladder is out of scope, stated
/// honestly). Pure.
#[allow(clippy::too_many_arguments)] // APPROVED: private forensics builder — a struct would be pure ceremony
fn build_dhan_chain_audit_row(
    target_minute_ist_nanos: i64,
    trading_date_nanos: i64,
    security_id: u64,
    symbol: &'static str,
    attempts: i64,
    final_http_status: i64,
    close_to_data_ms: i64,
    outcome: RestFetchOutcome,
    error_class: &'static str,
) -> RestFetchAuditRow {
    RestFetchAuditRow {
        ts_ist_nanos: target_minute_ist_nanos,
        trading_date_ist_nanos: trading_date_nanos,
        feed: OPTION_CHAIN_1M_FEED_DHAN,
        leg: REST_FETCH_LEG_CHAIN_1M,
        // Unreachable overflow for the pinned 13/25/51 set — a visible
        // sentinel, never a silent sid=0 (the spot-leg precedent).
        security_id: i64::try_from(security_id).unwrap_or(i64::MAX),
        exchange_segment: OPTION_CHAIN_1M_SEGMENT_IDX_I,
        symbol,
        attempts,
        final_http_status,
        fetch_latency_ms: -1,
        close_to_data_ms,
        close_to_persist_ms: -1,
        rate_limited_count: 0,
        outcome,
        error_class,
    }
}

/// Best-effort forensics append: a failure logs (coded, CHAIN-03
/// `audit_append` stage) + counts and RETURNS — the fetch loop, the
/// verdict and the failure edge are never affected by the forensics leg.
/// Dhan emit sites stay field-less on the CHAIN codes per the rule-file
/// convention (grep-split by `feed="groww"`).
fn chain_audit_append_best_effort(
    audit_writer: &mut RestFetchAuditWriter,
    row: &RestFetchAuditRow,
) {
    if let Err(err) = audit_writer.append_row(row) {
        metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_append")
            .increment(1);
        error!(
            code = ErrorCode::Chain03PersistFailed.code_str(),
            stage = "audit_append",
            ?err,
            "CHAIN-03: rest_fetch_audit (chain_1m) row append failed \
             (forensics only — the fetch loop is unaffected)"
        );
    }
}

/// Best-effort forensics flush (same never-affects-the-loop contract).
fn chain_audit_flush_best_effort(audit_writer: &mut RestFetchAuditWriter) {
    if let Err(err) = audit_writer.flush() {
        metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_flush")
            .increment(1);
        error!(
            code = ErrorCode::Chain03PersistFailed.code_str(),
            stage = "audit_flush",
            ?err,
            "CHAIN-03: rest_fetch_audit (chain_1m) ILP flush failed — \
             pending forensics rows discarded (best-effort; the fetch loop \
             is unaffected)"
        );
    }
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
    audit_writer: &mut RestFetchAuditWriter,
    edge: &mut FailureEdge,
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
    // GAP-11 forensics: one row per (fired minute, underlying); `ok` rows
    // are HELD here until the data flush ACK, then stamped with the real
    // close_to_persist_ms (a failed flush converts them to flush_failed
    // named-gap rows — never a false ok row).
    let mut held_ok_rows: Vec<RestFetchAuditRow> = Vec::new();
    let target_minute_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);

    if let Some(jwt) = jwt {
        let minute_close_ms = i64::from(fire_secs_of_day).saturating_mul(MILLIS_PER_SEC);

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
            let Some(target) = targets.get(idx) else {
                continue;
            };
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
                    let expiry_nanos = minute_open_ist_nanos(target.expiry, 0);
                    let fetched_at = fetched_at_ist_nanos_now();
                    // GAP-11: per-underlying append verdict — a poisoned
                    // batch means the minute is a persist_failed named
                    // gap for this underlying, never an ok row.
                    let mut underlying_append_failed = false;
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
                            underlying_append_failed = true;
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
                    if underlying_append_failed {
                        // GAP-11: fetched-but-lost — a persist failure,
                        // never dressed as vendor absence (the spot-leg
                        // round-2 LOW precedent).
                        chain_audit_append_best_effort(
                            audit_writer,
                            &build_dhan_chain_audit_row(
                                target_minute_nanos,
                                trading_date_nanos,
                                target.security_id,
                                target.symbol,
                                1,
                                200,
                                -1,
                                RestFetchOutcome::NamedGap,
                                "persist_failed",
                            ),
                        );
                    } else {
                        // GAP-11: HOLD the ok row until the data flush ACK
                        // (close_to_persist_ms stamped post-flush).
                        held_ok_rows.push(build_dhan_chain_audit_row(
                            target_minute_nanos,
                            trading_date_nanos,
                            target.security_id,
                            target.symbol,
                            1,
                            200,
                            close_to_data_ms,
                            RestFetchOutcome::Ok,
                            "none",
                        ));
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
                    // GAP-11 forensics: 2xx-proven, zero strikes.
                    chain_audit_append_best_effort(
                        audit_writer,
                        &build_dhan_chain_audit_row(
                            target_minute_nanos,
                            trading_date_nanos,
                            target.security_id,
                            target.symbol,
                            1,
                            200,
                            -1,
                            RestFetchOutcome::Empty,
                            "empty_chain",
                        ),
                    );
                }
                ChainFetchOutcome::Entitlement(detail) => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_chain1m_fetch_total", "outcome" => "entitlement")
                        .increment(1);
                    if entitlement.is_none() {
                        entitlement = Some(detail);
                    }
                    // GAP-11 forensics: the entitlement class (no status
                    // surfaced by the ladder — 0 sentinel).
                    chain_audit_append_best_effort(
                        audit_writer,
                        &build_dhan_chain_audit_row(
                            target_minute_nanos,
                            trading_date_nanos,
                            target.security_id,
                            target.symbol,
                            1,
                            0,
                            -1,
                            RestFetchOutcome::Error,
                            "entitlement",
                        ),
                    );
                }
                ChainFetchOutcome::Failed(reason) => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_chain1m_fetch_total", "outcome" => "error").increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!("{}: {reason}", target.symbol));
                    }
                    // GAP-11 forensics: transport / non-2xx / parse /
                    // budget — the ladder surfaces no status (0 sentinel);
                    // the coalesced verdict log carries the reason text.
                    chain_audit_append_best_effort(
                        audit_writer,
                        &build_dhan_chain_audit_row(
                            target_minute_nanos,
                            trading_date_nanos,
                            target.security_id,
                            target.symbol,
                            1,
                            0,
                            -1,
                            RestFetchOutcome::Error,
                            "error",
                        ),
                    );
                }
            }
        }
        let flush_result = writer.flush();
        if let Err(err) = &flush_result {
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
            // GAP-11: the underlyings whose legs were staged-but-lost at
            // the flush become flush_failed named-gap rows (the fetch
            // itself answered 200); the held ok rows are discarded below
            // — never a false ok row.
            for held in &held_ok_rows {
                chain_audit_append_best_effort(
                    audit_writer,
                    &RestFetchAuditRow {
                        close_to_data_ms: -1,
                        outcome: RestFetchOutcome::NamedGap,
                        error_class: "flush_failed",
                        ..held.clone()
                    },
                );
            }
        }
        // GAP-11: the ok rows land ONLY after (and stamped with) the data
        // flush ACK — discarded on a failed flush.
        for row in stamp_held_ok_rows(
            held_ok_rows,
            flush_result.is_ok(),
            trading_date_nanos,
            ist_millis_of_day_now(),
        ) {
            chain_audit_append_best_effort(audit_writer, &row);
        }
    } else {
        // No token at fire time — REST cannot succeed; the whole minute is
        // a full miss (counted per underlying for honest rate math) + one
        // no_token forensics row per underlying.
        error_count = targets.len();
        sample_failure = Some("no access token available at fire time".to_string());
        for target in targets {
            metrics::counter!("tv_chain1m_fetch_total", "outcome" => "error").increment(1);
            chain_audit_append_best_effort(
                audit_writer,
                &build_dhan_chain_audit_row(
                    target_minute_nanos,
                    trading_date_nanos,
                    target.security_id,
                    target.symbol,
                    0,
                    0,
                    -1,
                    RestFetchOutcome::NoToken,
                    "no_token",
                ),
            );
        }
    }
    // GAP-11 forensics: flush ONCE per fire (best-effort).
    chain_audit_flush_best_effort(audit_writer);

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

/// Loud accounting for minute boundaries that elapsed UNFETCHED (fire
/// overrun / suspend / clock step) — the spot leg's H2 pattern with the
/// chain's own counter + edge. GAP-11: each missed (boundary, underlying)
/// pair also gets a `skipped` forensics row (the Groww chain precedent) —
/// `first_missed_boundary_secs` is the FIRST missed fire boundary, so the
/// rows key on the real missed minutes.
#[allow(clippy::too_many_arguments)] // APPROVED: private accounting sink — a struct would be pure ceremony (Groww chain precedent)
fn record_chain_skipped_boundaries(
    params: &OptionChain1mTaskParams,
    edge: &mut FailureEdge,
    audit_writer: &mut RestFetchAuditWriter,
    targets: &[ResolvedUnderlying],
    skipped: u32,
    first_missed_boundary_secs: u32,
    iter_date: NaiveDate,
) {
    if skipped == 0 {
        return;
    }
    metrics::counter!("tv_chain1m_boundary_skipped_total").increment(u64::from(skipped));
    let around = format_minute_ist_12h(first_missed_boundary_secs);
    error!(
        code = ErrorCode::Chain02FetchDegraded.code_str(),
        stage = "boundary_skipped",
        skipped,
        around = %around,
        "CHAIN-02: minute boundaries elapsed unfetched (fire overrun / \
         suspend) — those minutes stay absent (re-fetchable via backfill)"
    );
    // GAP-11 (the Groww chain item-7 precedent): a wake that crossed IST
    // midnight would stamp the missed PRE-SUSPEND session seconds onto the
    // POST-WAKE date — wrong trading date on every row. The counter + the
    // coalesced log already fired; skip the (mis-dated) forensics rows +
    // edge accounting — the day is over for this run.
    //
    // DELIBERATE edge-accounting change (review MEDIUM 3, 2026-07-14):
    // pre-GAP-11 this fn fed `edge.record_minute(true)` UNCONDITIONALLY for
    // every missed boundary; post-guard a midnight-crossing wake feeds the
    // edge NOTHING for those boundaries. Defensible by design: the trading
    // day those boundaries belonged to is over, and an escalation page for
    // yesterday's tail on the post-wake day would be noise, not signal.
    let trading_date = today_ist();
    if trading_date != iter_date {
        warn!(
            %iter_date,
            %trading_date,
            "option_chain_1m: wake crossed IST midnight — skipping the \
             boundary-skip forensics rows (wrong trading date); the day is \
             over for this run"
        );
        return;
    }
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
    for i in 0..skipped {
        let boundary = first_missed_boundary_secs.saturating_add(i.saturating_mul(60));
        let minute_open_secs = boundary.saturating_sub(60);
        let target_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
        for target in targets {
            chain_audit_append_best_effort(
                audit_writer,
                &build_dhan_chain_audit_row(
                    target_nanos,
                    trading_date_nanos,
                    target.security_id,
                    target.symbol,
                    0,
                    0,
                    -1,
                    RestFetchOutcome::Skipped,
                    "boundary_skipped",
                ),
            );
        }
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
    chain_audit_flush_best_effort(audit_writer);
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
    // GAP-11 forensics: the shared rest_fetch_audit table (idempotent —
    // the spot + Groww legs also ensure it; a second call is harmless).
    ensure_rest_fetch_audit_table(&params.questdb).await;

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
    // GAP-11 forensics: best-effort per-fetch audit rows (never on the
    // fetch/verdict/edge path).
    let mut audit_writer = RestFetchAuditWriter::new(&params.questdb);
    let mut edge = FailureEdge::default();
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
        // GAP-11: the date this iteration's minute math is stamped with —
        // the boundary-skip forensics guard compares it against a
        // post-wake re-read (the Groww chain midnight-cross precedent).
        let iter_date = today_ist();
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
            record_chain_skipped_boundaries(
                &params,
                &mut edge,
                &mut audit_writer,
                &targets,
                missed,
                fire,
                iter_date,
            );
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
            &mut audit_writer,
            &mut edge,
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
        record_chain_skipped_boundaries(
            &params,
            &mut edge,
            &mut audit_writer,
            &targets,
            missed,
            fire + 60,
            iter_date,
        );
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

    // ---- GAP-11 forensics row builder (rest_fetch_audit, leg='chain_1m') ----

    #[test]
    fn test_chain_audit_row_attempts_is_one_and_leg_chain_1m() {
        let row = build_dhan_chain_audit_row(
            1_000,
            0,
            13,
            "NIFTY",
            1,
            200,
            1_042,
            RestFetchOutcome::Ok,
            "none",
        );
        assert_eq!(row.leg, "chain_1m");
        assert_eq!(row.feed, "dhan");
        assert_eq!(row.exchange_segment, "IDX_I");
        // Live-snapshot semantics: ONE request per (minute, underlying) —
        // the chain has no in-minute re-poll ladder.
        assert_eq!(row.attempts, 1);
        assert_eq!(row.security_id, 13);
        assert_eq!(row.close_to_data_ms, 1_042);
        // Held-until-flush semantics: the builder always starts at the -1
        // persist sentinel — `stamp_held_ok_rows` fills the real value
        // post-flush-ACK (GAP-11).
        assert_eq!(row.close_to_persist_ms, -1);
        // The Dhan chain ladder surfaces no per-attempt latency — honest
        // -1 sentinel (never a fabricated number).
        assert_eq!(row.fetch_latency_ms, -1);
        // sid overflow → visible i64::MAX sentinel, never a silent 0.
        let overflow = build_dhan_chain_audit_row(
            0,
            0,
            u64::MAX,
            "X",
            1,
            0,
            -1,
            RestFetchOutcome::Error,
            "error",
        );
        assert_eq!(overflow.security_id, i64::MAX);
        assert_eq!(overflow.final_http_status, 0);
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
}
