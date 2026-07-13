//! Groww per-minute option-chain REST leg — PR-3 of the Groww per-minute
//! REST plan (operator grant 2026-07-13,
//! `.claude/plans/active-plan-groww-rest-1m.md`; authorization
//! `groww-second-feed-scope-2026-06-19.md` §38 +
//! `no-rest-except-live-feed-2026-06-27.md` §9; runbook
//! `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! Every trading-day minute close in session — sequenced immediately AFTER
//! the GROWW spot leg (`groww_spot_1m_boot.rs`) via its `watch` signal,
//! bounded by a fallback timer — this task pulls the FULL current-expiry
//! option chain for the 3 underlyings (NIFTY/BANKNIFTY on NSE, SENSEX on
//! BSE) via Groww
//! `GET /v1/option-chain/exchange/{exchange}/underlying/{underlying}?expiry_date=...`
//! and persists every per-strike per-leg row to the SAME `option_chain_1m`
//! QuestDB table tagged `feed='groww'` (feed-in-key DEDUP — the Dhan rows
//! are untouched; a re-fetch UPSERTs in place). Cold path ONLY: the WS
//! pipelines, tick capture and trading are untouched.
//!
//! ## NO response timestamp (load-bearing honesty note)
//! The Groww chain response carries NO timestamp of any kind
//! (Verified-absence — `docs/groww-ref/14-option-chain.md` §3). The
//! MEASURED close→data latency (`tv_groww_chain1m_close_to_data_ms` +
//! the per-row `close_to_data_ms` column) is therefore the ONLY freshness
//! signal for every persisted snapshot — never asserted, always measured
//! (operator Quote 2).
//!
//! ## Expiry discovery — the instruments master, zero rate cost
//! The CURRENT (nearest ≥ today) expiry per underlying comes from the
//! daily Groww instruments master CSV (`select_current_option_expiry`
//! over the CE/PE FNO rows — `docs/groww-ref/14-option-chain.md` §4 names
//! the master as a documented expiry source; the §9 grant deliberately
//! excludes the `get_expiries` REST endpoint). On expiry day the same-day
//! expiry holds through the session (the house never-roll precedent); the
//! next trading day's warmup rolls naturally. An underlying the master
//! cannot resolve DEGRADES for the day (coded error + forensics row +
//! counter + one HIGH page) — never a guessed expiry.
//!
//! ## Sequencing (spot first, chain right after — best-effort, bounded)
//! The Groww spot leg publishes a `tokio::sync::watch` signal at the END
//! of each of its fires (success or failure). This task sleeps to each
//! minute boundary + [`GROWW_CHAIN_1M_FALLBACK_DELAY_MS`] but wakes EARLY
//! when the spot signal reaches its minute — the Dhan-identical semantics
//! (`option_chain_1m_boot.rs`, whose `wait_for_signal_or_fallback` core is
//! REUSED): a disabled, dead, or slow spot leg never blocks chain capture.
//!
//! ## Rate budget + pacing
//! Groww documents NO chain-specific rate rule
//! (`docs/groww-ref/14-option-chain.md` §4 — the limit family is
//! UNDOCUMENTED; Unknown ≠ unlimited). The 3 underlyings are fetched
//! SEQUENTIALLY (at most ONE in-flight request — the ≤6 req/s
//! minute-boundary pacing ceiling of
//! `docs/groww-ref/15-rate-limits-and-capacity.md`, shared-token bucket
//! co-tenanted with bruteX), one request per underlying per minute (NO
//! in-minute re-poll ladder — the chain is a live snapshot, not a sealing
//! candle), plus a MECHANICAL cross-request
//! [`GROWW_CHAIN_1M_MIN_GAP_MS`] guard: ONE scalar last-request stamp
//! spans consecutive chain requests (any underlying), so within a fire
//! the 2nd/3rd requests each wait out the remaining gap instead of
//! bursting back-to-back at the boundary.
//!
//! Spacing math (hostile-round-1 MEDIUM-1): with a 1,000 ms min gap the
//! 3 requests of one fire spread over ≥ 2 × 1,000 ms = 2 s → the chain
//! leg contributes at most 1 req/s sustained. The fallback wake trails
//! the spot fire by [`GROWW_CHAIN_1M_FALLBACK_DELAY_MS`] (2.5 s), and
//! even in the worst overlap (spot still laddering when the chain's
//! fallback fires) the combined boundary burst is spot's ≤1 in-flight
//! request + the chain's ≤1 req/s — comfortably inside the ≤6 req/s
//! family ceiling and the ~3 req/s bruteX co-tenancy headroom. Every 429
//! is counted + shape-captured (the live-probe (e) requirement); payload
//! bytes + strike/leg counts are histogrammed (the live-probe (d)
//! requirement — chain size is undocumented, U-12).
//!
//! ## Config semantics (mirrors the Dhan `[option_chain_1m]` gate)
//! - `enabled = true` → run the pipeline.
//! - `enabled = false` + `probe_and_report = true` (the default) → ONE
//!   bounded boot-time chain call per underlying, verdict (shape / strike
//!   count / latency / reject class) via an Info Telegram + coded log,
//!   persist NOTHING, exit. The pipeline NEVER auto-runs — the operator
//!   flips the config after the probe verdict.
//!
//! ## Lifetime
//! Single-day pass (the spot-leg rationale): runs today's remaining minute
//! closes, exits after 15:30 IST or on a non-trading day. Supervised
//! respawn wrapper (`tv_groww_chain1m_task_respawn_total{reason}` +
//! bounded backoff) — EXCEPT after a warmup disabled-for-the-day stop
//! (unresolvable expiries), which deliberately stays down (tomorrow's
//! boot re-warms). Panic honesty (the TICK-FLUSH-01 precedent): release
//! `panic = "abort"` — the panic-respawn arm is an unwind-build path only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE, GROWW_CHAIN_1M_FALLBACK_DELAY_MS,
    GROWW_CHAIN_1M_MASTER_RETRY_BACKOFF_SECS, GROWW_CHAIN_1M_MAX_BODY_BYTES,
    GROWW_CHAIN_1M_MIN_GAP_MS, GROWW_CHAIN_1M_REQUEST_TIMEOUT_SECS,
    GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS, GROWW_CHAIN_1M_UNDERLYINGS,
    GROWW_OPTION_CHAIN_URL_PREFIX, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
    SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::capture_rest_error_body;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::feed::groww::instruments::{
    GrowwInstrumentRow, download_groww_master_rows, select_current_option_expiry,
    stable_index_security_id,
};
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::disk_health_watcher::classify_join_exit;
use tickvault_storage::option_chain_1m_persistence::{
    OPTION_CHAIN_1M_FEED_GROWW, OPTION_CHAIN_1M_LEG_CE, OPTION_CHAIN_1M_LEG_PE,
    OPTION_CHAIN_1M_SEGMENT_IDX_I, OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN, OptionChain1mRow,
    OptionChain1mWriter, ensure_option_chain_1m_table,
};
use tickvault_storage::rest_fetch_audit_persistence::{
    REST_FETCH_LEG_CHAIN_1M, RestFetchAuditRow, RestFetchAuditWriter, RestFetchOutcome,
    ensure_rest_fetch_audit_table,
};

use crate::groww_spot_1m_boot::GrowwTokenCache;
// Session-boundary scheduling primitives + edge tracker + body-cap helpers
// are REUSED from the Dhan spot leg (NSE-session facts + pure state
// machines); the sequencing wait core + strike bounds + the fully-failed
// verdict are REUSED from the Dhan chain leg (one implementation, two
// feeds).
use crate::option_chain_1m_boot::{
    MAX_PLAUSIBLE_STRIKE, MAX_STRIKES_PER_CHAIN, chain_minute_fully_failed, stale_wake_backoff_ms,
    wait_for_signal_or_fallback,
};
use crate::spot_1m_rest_boot::{
    EdgeAction, FailureEdge, accumulation_within_cap, count_missed_boundaries,
    declared_len_within_cap, fire_is_fresh, format_minute_ist_12h, minute_open_ist_nanos,
    next_fire_after, spot_1m_day_is_over,
};

/// Backoff before the supervisor respawns a dead/failed scheduler run.
const GROWW_CHAIN_1M_RESPAWN_BACKOFF_SECS: u64 = 30;
/// Milliseconds per second / per day (wall-clock latency math).
const MILLIS_PER_SEC: i64 = 1_000;
const MILLIS_PER_DAY: i64 = 86_400_000;
/// Nanoseconds per second (IST-epoch math).
const NANOS_PER_SEC: i64 = 1_000_000_000;
/// Master-download attempts = first try + the constant backoffs.
const MASTER_DOWNLOAD_ATTEMPTS: usize = GROWW_CHAIN_1M_MASTER_RETRY_BACKOFF_SECS.len() + 1;

/// Everything the scheduler needs, cloneable so the supervisor can respawn
/// the inner run. NO Dhan token handle and NO Dhan base URL — this leg is
/// fully independent of the Dhan lane (token via the shared-minter SSM
/// read; endpoint constant).
#[derive(Clone)]
pub struct GrowwChain1mTaskParams {
    /// Telegram dispatcher for the edge page / recovery ping / probe
    /// verdict / expiry-degrade page.
    pub notifier: Arc<NotificationService>,
    /// Trading calendar (weekday + NSE-holiday aware) — re-checked every
    /// loop iteration (audit-findings Rule 3).
    pub calendar: Arc<TradingCalendar>,
    /// QuestDB target for the `option_chain_1m` + `rest_fetch_audit` tables.
    pub questdb: QuestDbConfig,
    /// GROWW spot-leg "minute completed" signal (the boundary
    /// seconds-of-day the spot leg just finished firing). `None` when the
    /// spot leg is disabled — the fallback timer then paces every fire.
    pub spot_minute_done: Option<tokio::sync::watch::Receiver<Option<u32>>>,
}

// ---------------------------------------------------------------------------
// Pure request building
// ---------------------------------------------------------------------------

/// The Groww chain URL for one (exchange, underlying) — path params on the
/// documented prefix; the `expiry_date` travels as a QUERY param (added by
/// the client, never string-built), the token ONLY in the Authorization
/// header. Inputs come from the pinned [`GROWW_CHAIN_1M_UNDERLYINGS`]
/// constant (static, URL-safe by construction). Pure.
#[must_use]
pub fn groww_chain_url(exchange: &str, underlying: &str) -> String {
    format!("{GROWW_OPTION_CHAIN_URL_PREFIX}/exchange/{exchange}/underlying/{underlying}")
}

// ---------------------------------------------------------------------------
// Pure response parsing (hostile-input hardened)
// ---------------------------------------------------------------------------

/// One parsed Groww option leg (CE or PE) of one strike — the documented
/// per-leg schema (`docs/groww-ref/14-option-chain.md` §2): ltp /
/// open_interest / volume + greeks {delta, gamma, theta, vega, rho, iv}.
/// No previous-day OI and no per-contract numeric id exist in the Groww
/// payload (the persisted row stamps 0 for both — honest absence).
#[derive(Clone, Debug, PartialEq)]
pub struct GrowwParsedLeg {
    /// Strike price (parsed from the strike-string map key).
    pub strike: f64,
    /// `"CE"` or `"PE"`.
    pub leg: &'static str,
    pub ltp: f64,
    pub iv: f64,
    pub delta: f64,
    pub theta: f64,
    pub gamma: f64,
    pub vega: f64,
    /// Rho — Groww serves it per leg (Dhan does not); persisted via the
    /// 2026-07-13 `rho` column.
    pub rho: f64,
    pub oi: i64,
    pub volume: i64,
}

/// One parsed Groww option-chain snapshot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GrowwParsedChain {
    /// The response's own `underlying_ltp` (Groww's view of the underlying
    /// at snapshot time). 0.0 when the field is absent/null/non-numeric —
    /// see `underlying_ltp_missing` (LOW-5: vendor absence must be
    /// distinguishable from a real 0).
    pub underlying_ltp: f64,
    /// `true` when the payload carried NO numeric `underlying_ltp` — the
    /// persisted `underlying_spot = 0.0` then means "vendor omitted it",
    /// not a real price of 0 (counted + one coded warn per fired minute).
    pub underlying_ltp_missing: bool,
    /// Every present leg across every kept strike (a one-sided deep-OTM
    /// strike contributes one leg — CE or PE absent/null is skipped, never
    /// a panic; whether both sides are always present is Unknown, so the
    /// `Option<>` discipline is defensive).
    pub legs: Vec<GrowwParsedLeg>,
    /// Strikes kept (the U-12 chain-size probe input).
    pub strikes_kept: u32,
    /// Strike keys that did not parse as numbers OR parsed to an
    /// implausible value — skipped, counted by the caller, never silent.
    pub invalid_strikes: u32,
    /// Strikes past [`MAX_STRIKES_PER_CHAIN`] — dropped by the parse cap,
    /// counted by the caller (coalesced `error!`), never silent.
    pub truncated_strikes: u32,
}

/// Tolerant numeric read: JSON number (int OR float) → f64; anything else
/// (absent / null / string / object) → 0.0. Pure.
fn val_f64(obj: &Value, key: &str) -> f64 {
    obj.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Tolerant integer read: JSON int → i64; a float (number-type wobble)
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

/// Parse one leg object (`CE`/`PE`) — absent/null/non-object legs return
/// `None` (skipped); field-level type wobble degrades per-field to 0 /
/// 0.0, never a parse failure. Pure.
fn parse_groww_leg(
    strike: f64,
    leg_name: &'static str,
    leg_val: Option<&Value>,
) -> Option<GrowwParsedLeg> {
    let obj = leg_val?;
    if !obj.is_object() {
        return None;
    }
    let greeks = obj.get("greeks").cloned().unwrap_or(Value::Null);
    Some(GrowwParsedLeg {
        strike,
        leg: leg_name,
        ltp: val_f64(obj, "ltp"),
        iv: val_f64(&greeks, "iv"),
        delta: val_f64(&greeks, "delta"),
        theta: val_f64(&greeks, "theta"),
        gamma: val_f64(&greeks, "gamma"),
        vega: val_f64(&greeks, "vega"),
        rho: val_f64(&greeks, "rho"),
        oi: val_i64(obj, "open_interest"),
        volume: val_i64(obj, "volume"),
    })
}

/// Parse a full Groww chain response body. Accepts BOTH documented shapes
/// defensively — the `{"status":"SUCCESS","payload":{...}}` envelope
/// (`docs/groww-ref/14-option-chain.md` §2 verbatim example) AND a bare
/// `{underlying_ltp, strikes}` object (the SDK-unwrapped form). `None` on
/// a malformed top-level shape (not JSON / no strikes map — the CHAIN-02
/// parse-failure arm; a `"status":"FAILURE"` error envelope has no
/// strikes map, so it lands here too); a well-formed response with ZERO
/// strikes parses to empty `legs` (the `outcome="empty"` arm). Strike
/// keys are STRINGS (integer-form `"23400"` in the docs; decimal keys are
/// Unknown — U-11) parsed as f64, never assumed integer; unparsable /
/// implausible keys are skipped + counted. Panic-free on hostile input by
/// construction (`serde_json` value walking, no indexing, no unwrap).
/// Pure.
#[must_use]
pub fn parse_groww_option_chain(body: &str) -> Option<GrowwParsedChain> {
    let v: Value = serde_json::from_str(body).ok()?;
    // Envelope-tolerant: payload-wrapped or bare.
    let data = v.get("payload").filter(|p| p.is_object()).unwrap_or(&v);
    let strikes = data.get("strikes").and_then(Value::as_object)?;
    // Absence-aware read (LOW-5): absent / null / non-numeric persists as
    // 0.0 BUT is flagged so the caller can tell vendor omission from a
    // real 0.
    let underlying_ltp = data.get("underlying_ltp").and_then(Value::as_f64);
    let mut chain = GrowwParsedChain {
        underlying_ltp: underlying_ltp.unwrap_or(0.0),
        underlying_ltp_missing: underlying_ltp.is_none(),
        ..GrowwParsedChain::default()
    };
    for (strike_key, legs) in strikes {
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
        if chain.strikes_kept as usize >= MAX_STRIKES_PER_CHAIN {
            chain.truncated_strikes = chain.truncated_strikes.saturating_add(1);
            continue;
        }
        chain.strikes_kept = chain.strikes_kept.saturating_add(1);
        if let Some(ce) = parse_groww_leg(strike, OPTION_CHAIN_1M_LEG_CE, legs.get("CE")) {
            chain.legs.push(ce);
        }
        if let Some(pe) = parse_groww_leg(strike, OPTION_CHAIN_1M_LEG_PE, legs.get("PE")) {
            chain.legs.push(pe);
        }
    }
    Some(chain)
}

// ---------------------------------------------------------------------------
// Pure pacing / classification helpers
// ---------------------------------------------------------------------------

/// Mechanical cross-request pacing: milliseconds still to wait so ANY two
/// CONSECUTIVE chain requests (across underlyings — one scalar stamp, the
/// hostile-round-1 MEDIUM-1 fix) are ≥ [`GROWW_CHAIN_1M_MIN_GAP_MS`]
/// apart. This is what spreads one fire's 3 requests over ≥ 2 s instead
/// of a back-to-back boundary burst (Groww documents no chain rule, but
/// the ≤6 req/s family ceiling + bruteX co-tenancy demand real spacing).
/// A midnight-wrapped/backwards clock yields 0 (never a long spurious
/// sleep). Pure.
#[must_use]
pub fn groww_min_gap_wait_ms(last_request_ms_of_day: Option<i64>, now_ms_of_day: i64) -> u64 {
    let Some(last) = last_request_ms_of_day else {
        return 0;
    };
    if now_ms_of_day < last {
        return 0;
    }
    let elapsed = now_ms_of_day - last;
    u64::try_from((GROWW_CHAIN_1M_MIN_GAP_MS as i64).saturating_sub(elapsed)).unwrap_or(0)
}

/// Bounded failure slug for the forensics row — NEVER raw body text. Pure.
#[must_use]
fn chain_error_class_for_status(status: u16) -> &'static str {
    match status {
        0 => "transport",
        401 | 403 => "auth",
        429 => "rate_limited",
        400..=499 => "http_4xx",
        500..=599 => "http_5xx",
        _ => "http_other",
    }
}

// ---------------------------------------------------------------------------
// Wall-clock helpers (IST) — module-local copies (the Dhan-chain precedent)
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

/// Retrieval wall-clock instant as IST nanoseconds (`Utc::now()` source ⇒
/// ADD the IST offset per `data-integrity.md`).
fn fetched_at_ist_nanos_now() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS).saturating_mul(NANOS_PER_SEC))
}

// ---------------------------------------------------------------------------
// HTTP leg
// ---------------------------------------------------------------------------

/// One request's typed failure — classification from the REAL
/// `StatusCode`, never a substring scan; `msg` is the bounded
/// secret-redacted capture (DHAN-REST-400 discipline).
#[derive(Clone, Debug, PartialEq)]
struct GrowwChainFetchFailure {
    /// HTTP status (0 = the request never got a response).
    status: u16,
    rate_limited: bool,
    auth_rejected: bool,
    msg: String,
}

/// Read a response body with the [`GROWW_CHAIN_1M_MAX_BODY_BYTES`] cap
/// enforced BOTH on the declared `Content-Length` and on the streamed
/// accumulation (csv_downloader §18 pattern, via the shared pure cap fns).
async fn read_body_capped(mut resp: reqwest::Response) -> Result<String, String> {
    if !declared_len_within_cap(resp.content_length(), GROWW_CHAIN_1M_MAX_BODY_BYTES) {
        return Err(format!(
            "body too large: declared {} bytes > cap {GROWW_CHAIN_1M_MAX_BODY_BYTES}",
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
        if !accumulation_within_cap(buf.len(), chunk.len(), GROWW_CHAIN_1M_MAX_BODY_BYTES) {
            return Err(format!(
                "body exceeded cap {GROWW_CHAIN_1M_MAX_BODY_BYTES} bytes mid-stream"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| "body not valid UTF-8".to_string())
}

/// One Groww chain REST round-trip → the raw 2xx body text. The token
/// travels ONLY in the `Authorization` header (never the URL, never
/// logged); `expiry_date` travels as a query param. A 429 records the
/// live-probe (e) shape (endpoint + Retry-After presence + sanitized
/// body) via one bounded `warn!` per occurrence.
async fn groww_chain_fetch_once(
    client: &reqwest::Client,
    url: &str,
    expiry_date: &str,
    token: &SecretString,
) -> Result<String, GrowwChainFetchFailure> {
    let resp = client
        .get(url)
        .query(&[("expiry_date", expiry_date)])
        .bearer_auth(token.expose_secret())
        .header(GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE)
        .send()
        .await
        .map_err(|e| GrowwChainFetchFailure {
            status: 0,
            rate_limited: false,
            auth_rejected: false,
            msg: format!("send: {}", capture_rest_error_body(&e.to_string())),
        })?;
    let status = resp.status();
    if !status.is_success() {
        let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        let auth_rejected =
            status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN;
        let retry_after_present = resp.headers().contains_key("retry-after");
        let error_body = read_body_capped(resp).await.unwrap_or_default();
        let captured = capture_rest_error_body(&error_body);
        if rate_limited {
            // Live-probe (e): every 429's timestamp (this log's own ts),
            // endpoint, Retry-After presence and body shape — bounded +
            // sanitized, never raw.
            metrics::counter!("tv_groww_chain1m_rate_limited_total").increment(1);
            warn!(
                endpoint = url,
                retry_after_present,
                body = %captured,
                "groww_chain_1m: HTTP 429 rate-limited (undocumented chain \
                 bucket — the live probe) — counted; next boundary re-attempts"
            );
        }
        return Err(GrowwChainFetchFailure {
            status: status.as_u16(),
            rate_limited,
            auth_rejected,
            msg: format!("http {status} retry_after_present={retry_after_present} body={captured}"),
        });
    }
    read_body_capped(resp)
        .await
        .map_err(|msg| GrowwChainFetchFailure {
            status: status.as_u16(),
            rate_limited: false,
            auth_rejected: false,
            msg,
        })
}

// ---------------------------------------------------------------------------
// Warmup — expiry resolution from the instruments master (zero rate cost)
// ---------------------------------------------------------------------------

/// One resolved underlying, ready for per-minute chain fetches.
#[derive(Clone, Debug, PartialEq)]
pub struct GrowwChainTarget {
    /// PLAIN underlying symbol (`NIFTY` — the chain URL path param AND the
    /// canonical the master rows matched on).
    pub underlying: &'static str,
    /// Exchange path param (`NSE` / `BSE`).
    pub exchange: &'static str,
    /// The Groww live-lane stable id (from the `groww_symbol` — the SAME
    /// id space the live ticks use, so persisted rows join).
    pub security_id: i64,
    /// Today's CURRENT expiry (from the instruments master — never guessed).
    pub expiry: NaiveDate,
    /// The expiry as the query-param string (`"YYYY-MM-DD"`).
    pub expiry_str: String,
    /// The chain endpoint URL for this underlying — computed ONCE at
    /// target build (not per minute) via [`groww_chain_url`]; tests point
    /// it at a hermetic mock server (the round-1 coverage battery).
    pub url: String,
}

/// Day-keyed cache of the resolved chain targets, shared across supervisor
/// respawns (hostile-round-1 MEDIUM-4): a flapping ~30s respawn loop must
/// NOT re-download the multi-MB instruments master every cycle. Only the
/// TINY resolved targets (≤3 entries) are cached — never the raw master
/// rows (holding the multi-MB Vec all day would be wasted RAM). Refreshes
/// only on an IST date change; the day-start degrade page also fires only
/// on the first (cache-filling) resolve, never per respawn.
pub type GrowwChainTargetsCache =
    Arc<tokio::sync::Mutex<Option<(NaiveDate, Vec<GrowwChainTarget>)>>>;

/// Resolve the current expiry for every underlying from parsed master
/// rows. Returns `(targets, degraded)` — a `None` expiry degrades that
/// underlying (named), never the whole day unless ALL degrade. Pure.
#[must_use]
pub fn resolve_groww_chain_targets(
    rows: &[GrowwInstrumentRow],
    today: NaiveDate,
) -> (Vec<GrowwChainTarget>, Vec<&'static str>) {
    let mut targets = Vec::with_capacity(GROWW_CHAIN_1M_UNDERLYINGS.len());
    let mut degraded = Vec::new();
    for (underlying, exchange, groww_symbol) in GROWW_CHAIN_1M_UNDERLYINGS {
        match select_current_option_expiry(rows, exchange, underlying, today) {
            Some(expiry) => targets.push(GrowwChainTarget {
                underlying,
                exchange,
                security_id: stable_index_security_id(groww_symbol),
                expiry,
                expiry_str: expiry.format("%Y-%m-%d").to_string(),
                url: groww_chain_url(exchange, underlying),
            }),
            None => degraded.push(underlying),
        }
    }
    (targets, degraded)
}

/// Bounded master download: first try + the constant backoffs (the master
/// is a public static CSV — zero auth, zero rate budget; retrying it never
/// storms anyone).
async fn download_master_bounded() -> Result<Vec<GrowwInstrumentRow>, String> {
    let mut last_err = String::from("master download never attempted");
    for attempt in 0..MASTER_DOWNLOAD_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(
                GROWW_CHAIN_1M_MASTER_RETRY_BACKOFF_SECS[attempt - 1],
            ))
            .await;
        }
        match download_groww_master_rows().await {
            Ok(rows) => return Ok(rows),
            Err(err) => last_err = format!("{err:?}"),
        }
    }
    Err(last_err)
}

/// Plain-English degrade detail for the expiry-unresolved page — bounded,
/// static symbol names only. Pure.
#[must_use]
pub fn expiry_degrade_detail(degraded: &[&'static str], master_rows: usize) -> String {
    format!(
        "{} — no usable option expiry in today's contract list ({master_rows} rows scanned)",
        degraded.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Forensics helpers (rest_fetch_audit, leg='chain_1m')
// ---------------------------------------------------------------------------

/// Map one underlying's fetch verdict to the typed forensics outcome. Pure.
#[must_use]
fn chain_audit_outcome(found: bool, empty: bool, rate_limited: bool) -> RestFetchOutcome {
    if found {
        RestFetchOutcome::Ok
    } else if empty {
        RestFetchOutcome::Empty
    } else if rate_limited {
        RestFetchOutcome::RateLimited
    } else {
        RestFetchOutcome::Error
    }
}

/// Build one `rest_fetch_audit` row for a chain fetch — leg `chain_1m`,
/// attempts=1 when a request ran (no in-minute ladder), 0 otherwise. Pure.
#[allow(clippy::too_many_arguments)] // APPROVED: private forensics builder — a struct would be pure ceremony (spot precedent)
fn build_chain_audit_row(
    target_minute_ist_nanos: i64,
    trading_date_nanos: i64,
    security_id: i64,
    symbol: &'static str,
    attempts: i64,
    final_http_status: i64,
    fetch_latency_ms: i64,
    close_to_data_ms: i64,
    rate_limited_count: i64,
    outcome: RestFetchOutcome,
    error_class: &'static str,
) -> RestFetchAuditRow {
    RestFetchAuditRow {
        ts_ist_nanos: target_minute_ist_nanos,
        trading_date_ist_nanos: trading_date_nanos,
        feed: OPTION_CHAIN_1M_FEED_GROWW,
        leg: REST_FETCH_LEG_CHAIN_1M,
        security_id,
        exchange_segment: OPTION_CHAIN_1M_SEGMENT_IDX_I,
        symbol,
        attempts,
        final_http_status,
        fetch_latency_ms,
        close_to_data_ms,
        rate_limited_count,
        outcome,
        error_class,
    }
}

/// Best-effort forensics append: a failure logs (coded, CHAIN-03) + counts
/// and RETURNS — the fetch loop, the verdict and the failure edge are
/// never affected by the forensics leg.
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
            feed = OPTION_CHAIN_1M_FEED_GROWW,
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
            feed = OPTION_CHAIN_1M_FEED_GROWW,
            ?err,
            "CHAIN-03: rest_fetch_audit (chain_1m) ILP flush failed — pending \
             forensics rows discarded (best-effort; the fetch loop is unaffected)"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-minute fire
// ---------------------------------------------------------------------------

/// One underlying's per-minute chain verdict.
#[derive(Clone, Debug, PartialEq)]
enum GrowwChainFetchOutcome {
    /// A parseable chain with ≥1 leg (`close_to_data_ms` = minute close →
    /// retrieval wall-clock latency — the ONLY freshness signal; the
    /// response carries no timestamp).
    Found {
        chain: GrowwParsedChain,
        close_to_data_ms: i64,
        payload_bytes: usize,
    },
    /// A parseable 2xx whose chain carried ZERO strikes — counted
    /// `outcome="empty"`, included in the failure edge, never silent.
    Empty,
    /// Transport / non-2xx / malformed body / budget overrun.
    Failed(GrowwChainFetchFailure),
}

/// One underlying's bounded per-minute fetch: the defensive min-gap wait,
/// then ONE request (NO in-minute ladder — the chain is a live snapshot),
/// hard-bounded by [`GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS`]. Returns the
/// verdict + the request instant (ms-of-day, min-gap bookkeeping) + the
/// final round-trip latency.
async fn fetch_groww_chain_bounded(
    client: &reqwest::Client,
    url: &str,
    expiry_date: &str,
    token: &SecretString,
    minute_close_ms_of_day: i64,
    last_request_ms: Option<i64>,
) -> (GrowwChainFetchOutcome, i64, i64) {
    let attempt = async {
        let wait_ms = groww_min_gap_wait_ms(last_request_ms, ist_millis_of_day_now());
        if wait_ms > 0 {
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }
        let requested_at_ms = ist_millis_of_day_now();
        let started = std::time::Instant::now();
        let result = groww_chain_fetch_once(client, url, expiry_date, token).await;
        let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
        metrics::histogram!("tv_groww_chain1m_fetch_duration_ms").record(latency_ms as f64);
        let outcome = match result {
            Ok(body_text) => {
                let payload_bytes = body_text.len();
                match parse_groww_option_chain(&body_text) {
                    Some(chain) if chain.legs.is_empty() => GrowwChainFetchOutcome::Empty,
                    Some(chain) => {
                        let close_to_data_ms =
                            (ist_millis_of_day_now() - minute_close_ms_of_day).max(0);
                        GrowwChainFetchOutcome::Found {
                            chain,
                            close_to_data_ms,
                            payload_bytes,
                        }
                    }
                    None => GrowwChainFetchOutcome::Failed(GrowwChainFetchFailure {
                        status: 200,
                        rate_limited: false,
                        auth_rejected: false,
                        msg: "2xx but the body was not a parseable option chain".to_string(),
                    }),
                }
            }
            Err(failure) => GrowwChainFetchOutcome::Failed(failure),
        };
        (outcome, requested_at_ms, latency_ms)
    };
    match tokio::time::timeout(
        Duration::from_secs(GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS),
        attempt,
    )
    .await
    {
        Ok(v) => v,
        Err(_elapsed) => {
            metrics::counter!("tv_groww_chain1m_underlying_budget_exceeded_total").increment(1);
            (
                GrowwChainFetchOutcome::Failed(GrowwChainFetchFailure {
                    status: 0,
                    rate_limited: false,
                    auth_rejected: false,
                    msg: format!(
                        "chain budget exceeded ({GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS}s) — peer stalling"
                    ),
                }),
                ist_millis_of_day_now(),
                -1,
            )
        }
    }
}

/// One minute-close fire: SEQUENTIAL bounded chain fetches for the
/// resolved underlyings (pacing rule — at most one in-flight request) →
/// per-leg rows via `append_row_ext` (rho + measured close→data delay) →
/// one flush → forensics rows → counters → edge accounting. An auth-class
/// reject short-circuits the remaining underlyings for THIS fire (the
/// spot item-12 discipline — no doomed requests with a dead token).
#[allow(clippy::too_many_arguments)] // APPROVED: private fire sink over the run loop's owned state — a struct would be pure ceremony (spot precedent)
async fn fire_one_groww_chain_minute(
    params: &GrowwChain1mTaskParams,
    client: &reqwest::Client,
    targets: &[GrowwChainTarget],
    last_request_ms: &mut Option<i64>,
    writer: &mut OptionChain1mWriter,
    audit_writer: &mut RestFetchAuditWriter,
    edge: &mut FailureEdge,
    token_cache: &mut GrowwTokenCache,
    fire_secs_of_day: u32,
) {
    let minute_open_secs = fire_secs_of_day.saturating_sub(60);
    let minute_label = format_minute_ist_12h(minute_open_secs);
    let trading_date = today_ist();
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
    let target_minute_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
    let minute_close_ms = i64::from(fire_secs_of_day).saturating_mul(MILLIS_PER_SEC);

    let mut ok_count: usize = 0;
    let mut empty_count: usize = 0;
    let mut error_count: usize = 0;
    // Persist-gated OK (the spot M1 discipline): a fetched-but-never-
    // persisted minute is NOT ok — a day-long QuestDB outage must page.
    let mut persist_failed = false;
    let mut sample_failure: Option<String> = None;

    if let Some(token) = token_cache.ensure_token().await {
        for (idx, target) in targets.iter().enumerate() {
            let (outcome, requested_at_ms, latency_ms) = fetch_groww_chain_bounded(
                client,
                &target.url,
                &target.expiry_str,
                &token,
                minute_close_ms,
                // ONE scalar across consecutive requests — the min-gap
                // engages BETWEEN underlyings too (MEDIUM-1: 3 requests
                // spread ≥ 2 s, never a back-to-back boundary burst).
                *last_request_ms,
            )
            .await;
            *last_request_ms = Some(requested_at_ms);
            let mut auth_rejected = false;
            match outcome {
                GrowwChainFetchOutcome::Found {
                    chain,
                    close_to_data_ms,
                    payload_bytes,
                } => {
                    ok_count = ok_count.saturating_add(1);
                    metrics::counter!("tv_groww_chain1m_fetch_total", "outcome" => "ok")
                        .increment(1);
                    // The measured freshness signal + the live-probe (d)
                    // chain-size numbers (strike/leg counts + bytes —
                    // undocumented upstream, U-12).
                    #[allow(clippy::cast_precision_loss)] // APPROVED: histogram samples only
                    {
                        metrics::histogram!("tv_groww_chain1m_close_to_data_ms")
                            .record(close_to_data_ms as f64);
                        metrics::histogram!("tv_groww_chain1m_strikes_per_chain")
                            .record(f64::from(chain.strikes_kept));
                        metrics::histogram!("tv_groww_chain1m_legs_per_chain")
                            .record(chain.legs.len() as f64);
                        metrics::histogram!("tv_groww_chain1m_payload_bytes")
                            .record(payload_bytes as f64);
                    }
                    if chain.underlying_ltp_missing {
                        // LOW-5: vendor omitted the underlying spot — the
                        // persisted underlying_spot = 0.0 is ABSENCE, not
                        // a real 0. One coded warn per underlying per
                        // fired minute (≤3/min) + a static-label counter.
                        metrics::counter!("tv_groww_chain1m_underlying_spot_missing_total")
                            .increment(1);
                        warn!(
                            code = ErrorCode::Chain02FetchDegraded.code_str(),
                            stage = "underlying_ltp_missing",
                            feed = OPTION_CHAIN_1M_FEED_GROWW,
                            symbol = target.underlying,
                            minute = %minute_label,
                            "groww_chain_1m: payload carried no numeric \
                             underlying_ltp — underlying_spot persisted as \
                             0.0 (vendor omission, not a real 0)"
                        );
                    }
                    if chain.invalid_strikes > 0 {
                        metrics::counter!("tv_groww_chain1m_invalid_strikes_total")
                            .increment(u64::from(chain.invalid_strikes));
                        warn!(
                            symbol = target.underlying,
                            invalid_strikes = chain.invalid_strikes,
                            minute = %minute_label,
                            "groww_chain_1m: skipped unparsable/implausible strike keys"
                        );
                    }
                    if chain.truncated_strikes > 0 {
                        // Coalesced ONCE per underlying per fire — a body
                        // past the strike cap is counted, never silent.
                        metrics::counter!("tv_groww_chain1m_strikes_truncated_total")
                            .increment(u64::from(chain.truncated_strikes));
                        error!(
                            code = ErrorCode::Chain02FetchDegraded.code_str(),
                            stage = "strikes_truncated",
                            feed = OPTION_CHAIN_1M_FEED_GROWW,
                            symbol = target.underlying,
                            truncated_strikes = chain.truncated_strikes,
                            cap = MAX_STRIKES_PER_CHAIN,
                            minute = %minute_label,
                            "CHAIN-02: Groww chain response exceeded the strike \
                             cap — extra strikes dropped (counted, never silent)"
                        );
                    }
                    let expiry_nanos = minute_open_ist_nanos(target.expiry, 0);
                    let fetched_at = fetched_at_ist_nanos_now();
                    for leg in &chain.legs {
                        let row = OptionChain1mRow {
                            ts_ist_nanos: target_minute_nanos,
                            trading_date_ist_nanos: trading_date_nanos,
                            underlying_security_id: target.security_id,
                            underlying_symbol: target.underlying,
                            expiry_ist_nanos: expiry_nanos,
                            strike: leg.strike,
                            leg: leg.leg,
                            // No per-contract numeric id exists in the Groww
                            // payload — 0 sentinel (honest absence; contract
                            // identity = (underlying, expiry, strike, leg)).
                            contract_security_id: 0,
                            last_price: leg.ltp,
                            iv: leg.iv,
                            delta: leg.delta,
                            theta: leg.theta,
                            gamma: leg.gamma,
                            vega: leg.vega,
                            oi: leg.oi,
                            volume: leg.volume,
                            // No previous-day OI in the Groww payload.
                            previous_oi: 0,
                            underlying_spot: chain.underlying_ltp,
                            fetched_at_ist_nanos: fetched_at,
                        };
                        if let Err(err) = writer.append_row_ext(&row, leg.rho, close_to_data_ms) {
                            persist_failed = true;
                            metrics::counter!(
                                "tv_groww_chain1m_persist_errors_total", "stage" => "append"
                            )
                            .increment(1);
                            error!(
                                code = ErrorCode::Chain03PersistFailed.code_str(),
                                stage = "append",
                                feed = OPTION_CHAIN_1M_FEED_GROWW,
                                symbol = target.underlying,
                                ?err,
                                "CHAIN-03: option_chain_1m (groww) row append failed"
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
                    let audit_row = build_chain_audit_row(
                        target_minute_nanos,
                        trading_date_nanos,
                        target.security_id,
                        target.underlying,
                        1,
                        200,
                        latency_ms,
                        close_to_data_ms,
                        0,
                        RestFetchOutcome::Ok,
                        "none",
                    );
                    chain_audit_append_best_effort(audit_writer, &audit_row);
                }
                GrowwChainFetchOutcome::Empty => {
                    empty_count = empty_count.saturating_add(1);
                    metrics::counter!("tv_groww_chain1m_fetch_total", "outcome" => "empty")
                        .increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!(
                            "{}: 2xx but the chain carried zero strikes",
                            target.underlying
                        ));
                    }
                    let audit_row = build_chain_audit_row(
                        target_minute_nanos,
                        trading_date_nanos,
                        target.security_id,
                        target.underlying,
                        1,
                        200,
                        latency_ms,
                        -1,
                        0,
                        RestFetchOutcome::Empty,
                        "empty_chain",
                    );
                    chain_audit_append_best_effort(audit_writer, &audit_row);
                }
                GrowwChainFetchOutcome::Failed(failure) => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_groww_chain1m_fetch_total", "outcome" => "error")
                        .increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!("{}: {}", target.underlying, failure.msg));
                    }
                    auth_rejected = failure.auth_rejected;
                    let error_class = if failure.status == 200 {
                        "parse"
                    } else if latency_ms < 0 {
                        "budget_exceeded"
                    } else {
                        chain_error_class_for_status(failure.status)
                    };
                    let audit_row = build_chain_audit_row(
                        target_minute_nanos,
                        trading_date_nanos,
                        target.security_id,
                        target.underlying,
                        1,
                        i64::from(failure.status),
                        latency_ms,
                        -1,
                        i64::from(failure.rate_limited),
                        chain_audit_outcome(false, false, failure.rate_limited),
                        error_class,
                    );
                    chain_audit_append_best_effort(audit_writer, &audit_row);
                }
            }
            if auth_rejected {
                // The spot item-12 discipline: drop the dead token NOW and
                // short-circuit the remaining underlyings for THIS fire —
                // every further request with the same rejected token is a
                // doomed 401. The next fire's ensure_token re-reads SSM at
                // the ≥60s floor; NEVER a mint.
                token_cache.note_auth_rejected();
                let remaining = &targets[idx + 1..];
                if !remaining.is_empty() {
                    error_count = error_count.saturating_add(remaining.len());
                    warn!(
                        skipped_underlyings = remaining.len(),
                        "groww_chain_1m: auth-class reject — remaining \
                         underlyings short-circuited for this fire (no doomed \
                         requests); forensics rows still emitted"
                    );
                    for skipped in remaining {
                        metrics::counter!("tv_groww_chain1m_fetch_total", "outcome" => "error")
                            .increment(1);
                        let row = build_chain_audit_row(
                            target_minute_nanos,
                            trading_date_nanos,
                            skipped.security_id,
                            skipped.underlying,
                            0,
                            0,
                            -1,
                            -1,
                            0,
                            RestFetchOutcome::NoToken,
                            "auth",
                        );
                        chain_audit_append_best_effort(audit_writer, &row);
                    }
                }
                break;
            }
        }
        if let Err(err) = writer.flush() {
            persist_failed = true;
            metrics::counter!("tv_groww_chain1m_persist_errors_total", "stage" => "flush")
                .increment(1);
            error!(
                code = ErrorCode::Chain03PersistFailed.code_str(),
                stage = "flush",
                feed = OPTION_CHAIN_1M_FEED_GROWW,
                ?err,
                "CHAIN-03: option_chain_1m (groww) ILP flush failed — pending \
                 rows discarded (poisoned-buffer defense; the minute stays \
                 absent and DEDUP-idempotent re-fetchable)"
            );
            if sample_failure.is_none() {
                sample_failure = Some(format!("persist flush failed: {err:#}"));
            }
        }
    } else {
        // No token at fire time — nothing can be sent; the whole minute is
        // a full miss (counted per underlying for honest rate math) + one
        // no_token forensics row per underlying.
        error_count = targets.len();
        sample_failure = Some("no shared Groww access token available at fire time".to_string());
        for target in targets {
            metrics::counter!("tv_groww_chain1m_fetch_total", "outcome" => "error").increment(1);
            let row = build_chain_audit_row(
                target_minute_nanos,
                trading_date_nanos,
                target.security_id,
                target.underlying,
                0,
                0,
                -1,
                -1,
                0,
                RestFetchOutcome::NoToken,
                "no_token",
            );
            chain_audit_append_best_effort(audit_writer, &row);
        }
    }
    chain_audit_flush_best_effort(audit_writer);

    record_groww_chain_minute_verdict(
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

/// Coalesced per-minute verdict: ONE coded log per fired minute with any
/// failure, plus the edge-triggered escalation page / recovery ping
/// (typed Groww events).
#[allow(clippy::too_many_arguments)] // APPROVED: private verdict sink — a struct would be pure ceremony (spot precedent)
fn record_groww_chain_minute_verdict(
    params: &GrowwChain1mTaskParams,
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
                feed = OPTION_CHAIN_1M_FEED_GROWW,
                consecutive,
                minute = minute_label,
                sample = sample_failure.unwrap_or("none captured"),
                "CHAIN-02: Groww per-minute chain fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwChain1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label.to_string(),
                });
        }
        EdgeAction::Recover { failed_minutes } => {
            info!(
                failed_minutes,
                minute = minute_label,
                "groww_chain_1m: per-minute fetch recovered after a paged episode"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwChain1mFetchRecovered {
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
                    feed = OPTION_CHAIN_1M_FEED_GROWW,
                    minute = minute_label,
                    ok = ok_count,
                    errors = error_count,
                    empty = empty_count,
                    persist_failed,
                    sample = sample_failure.unwrap_or("none captured"),
                    "CHAIN-02: Groww per-minute chain fetch degraded for this minute"
                );
            }
        }
    }
}

/// Loud accounting for minute boundaries that elapsed UNFETCHED (fire
/// overrun / suspend / clock step): counter + ONE coalesced coded log +
/// each missed minute feeds the failure edge + one `outcome=skipped`
/// forensics row per (minute, underlying) so the hole is queryable
/// (never silent — Rule 11). Mirrors the spot leg incl. the item-7
/// midnight-crossing guard.
fn record_groww_chain_skipped_boundaries(
    params: &GrowwChain1mTaskParams,
    edge: &mut FailureEdge,
    audit_writer: &mut RestFetchAuditWriter,
    targets: &[GrowwChainTarget],
    skipped: u32,
    first_missed_boundary_secs: u32,
    iter_date: NaiveDate,
) {
    if skipped == 0 {
        return;
    }
    metrics::counter!("tv_groww_chain1m_boundary_skipped_total").increment(u64::from(skipped));
    let around = format_minute_ist_12h(first_missed_boundary_secs);
    error!(
        code = ErrorCode::Chain02FetchDegraded.code_str(),
        stage = "boundary_skipped",
        feed = OPTION_CHAIN_1M_FEED_GROWW,
        skipped,
        around = %around,
        "CHAIN-02: Groww chain minute boundaries elapsed unfetched (fire \
         overrun / suspend) — those minutes stay absent (the chain has no \
         backfill; the next boundary re-attempts)"
    );
    // Item 7 (spot precedent): a wake that crossed IST midnight would
    // stamp the missed PRE-SUSPEND session seconds onto the POST-WAKE date
    // — wrong trading date on every row. The counter + coalesced log
    // already fired; skip the (mis-dated) forensics rows + edge accounting.
    let trading_date = today_ist();
    if trading_date != iter_date {
        warn!(
            %iter_date,
            %trading_date,
            "groww_chain_1m: wake crossed IST midnight — skipping the \
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
            let row = build_chain_audit_row(
                target_nanos,
                trading_date_nanos,
                target.security_id,
                target.underlying,
                0,
                0,
                -1,
                -1,
                0,
                RestFetchOutcome::Skipped,
                "boundary_skipped",
            );
            chain_audit_append_best_effort(audit_writer, &row);
        }
        if let EdgeAction::Page { consecutive } = edge.record_minute(true) {
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "escalation",
                feed = OPTION_CHAIN_1M_FEED_GROWW,
                consecutive,
                minute = %around,
                "CHAIN-02: Groww per-minute chain fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwChain1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: around.clone(),
                });
        }
    }
    chain_audit_flush_best_effort(audit_writer);
}

// ---------------------------------------------------------------------------
// Scheduler run + supervisor + probe
// ---------------------------------------------------------------------------

/// Run today's remaining chain minute fires, then return. Never panics;
/// every fault path logs (coded, `feed="groww"`) + counts. Sets
/// `disabled_for_day` before returning when NO underlying resolved an
/// expiry (tomorrow's boot re-warms — the supervisor deliberately does
/// NOT respawn that stop).
// TEST-EXEMPT: live-deps async runner — every scheduling / parsing / expiry-selection / edge decision is a pure fn unit-tested (here + in the reused spot/chain modules + instruments.rs); the HTTP leg mirrors the tested patterns; wiring pinned by crates/app/tests/groww_chain_1m_wiring_guard.rs.
pub async fn run_groww_chain_1m(
    params: GrowwChain1mTaskParams,
    disabled_for_day: Arc<AtomicBool>,
    targets_cache: GrowwChainTargetsCache,
) {
    // Idempotent DDL first (CREATE → ADD COLUMN self-heal — incl. the
    // 2026-07-13 rho/close_to_data_ms additions — → DEDUP ENABLE) for BOTH
    // tables; failures degrade loudly inside and never block.
    ensure_option_chain_1m_table(&params.questdb).await;
    ensure_rest_fetch_audit_table(&params.questdb).await;

    if !params.calendar.is_trading_day_today() {
        info!("groww_chain_1m: non-trading day — skipping all minute fires");
        return;
    }
    // ONE long-lived client for the whole session (per-minute rebuild is
    // the exact TLS/resolver churn HTTP-CLIENT-01 §0 condemns).
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(GROWW_CHAIN_1M_REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "client_build",
                feed = OPTION_CHAIN_1M_FEED_GROWW,
                ?err,
                "CHAIN-02: HTTP client build failed — Groww per-minute chain \
                 fetch degraded; supervisor will retry after backoff"
            );
            metrics::counter!("tv_groww_chain1m_fetch_total", "outcome" => "error").increment(1);
            return;
        }
    };

    // ---- Day-start warmup: expiry per underlying from the instruments
    // master (zero rate cost — NEVER an expiry REST endpoint, NEVER a
    // guessed expiry) ----
    let today = today_ist();
    // Respawn fast-path (MEDIUM-4): today's targets were already resolved
    // by an earlier run in this process — reuse them (no multi-MB master
    // re-download per ~30s respawn cycle); refresh only on a date change.
    let cached = {
        let cell = targets_cache.lock().await;
        cell.as_ref()
            .filter(|(date, _)| *date == today)
            .map(|(_, cached)| cached.clone())
    };
    let targets = if let Some(targets) = cached {
        info!(
            underlyings = targets.len(),
            "groww_chain_1m: reusing today's resolved expiries (supervisor \
             respawn — the instruments master is NOT re-downloaded)"
        );
        Some(targets)
    } else {
        resolve_and_cache_chain_targets(&params, &disabled_for_day, &targets_cache, today).await
    };
    let Some(targets) = targets else {
        // Warmup degraded to disabled-for-the-day (page/logs already
        // emitted inside) — the supervisor deliberately does not respawn.
        return;
    };
    run_groww_chain_minute_loop(&params, &client, targets).await;
}

/// Day-start warmup (the cache-filling slow path): download the
/// instruments master (bounded retries), resolve today's CURRENT expiry
/// per underlying, emit the per-underlying degrade page + forensics, then
/// cache the resolved targets for supervisor respawns (MEDIUM-4). `None`
/// = disabled for the day (page already fired; the supervisor exits).
// TEST-EXEMPT: live-deps warmup shell — the resolution (resolve_groww_chain_targets / select_current_option_expiry) and detail formatting (expiry_degrade_detail) are the unit-tested pure parts.
async fn resolve_and_cache_chain_targets(
    params: &GrowwChain1mTaskParams,
    disabled_for_day: &AtomicBool,
    targets_cache: &GrowwChainTargetsCache,
    today: NaiveDate,
) -> Option<Vec<GrowwChainTarget>> {
    let rows = match download_master_bounded().await {
        Ok(rows) => rows,
        Err(detail) => {
            metrics::counter!("tv_groww_chain1m_expiry_unresolved_total")
                .increment(GROWW_CHAIN_1M_UNDERLYINGS.len() as u64);
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "warmup",
                feed = OPTION_CHAIN_1M_FEED_GROWW,
                detail = %capture_rest_error_body(&detail),
                "CHAIN-02: Groww instruments-master download failed after \
                 bounded retries — the Groww chain leg degrades to DISABLED \
                 for the day (never a guessed expiry)"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwChain1mExpiryUnresolved {
                    detail: format!(
                        "today's contract list could not be downloaded ({})",
                        capture_rest_error_body(&detail)
                    ),
                });
            disabled_for_day.store(true, Ordering::SeqCst);
            return None;
        }
    };
    let (targets, degraded) = resolve_groww_chain_targets(&rows, today);
    if !degraded.is_empty() {
        // Per-underlying degrade (unlike the Dhan whole-day expirylist
        // stop — the master is per-underlying data): coded error + one
        // forensics row per degraded underlying + counter + ONE page
        // naming them. The resolved underlyings keep running.
        metrics::counter!("tv_groww_chain1m_expiry_unresolved_total")
            .increment(degraded.len() as u64);
        let detail = expiry_degrade_detail(&degraded, rows.len());
        error!(
            code = ErrorCode::Chain02FetchDegraded.code_str(),
            stage = "expiry_unresolved",
            feed = OPTION_CHAIN_1M_FEED_GROWW,
            degraded = ?degraded,
            master_rows = rows.len(),
            "CHAIN-02: no usable option expiry in today's Groww instruments \
             master for these underlyings — their chain recording is OFF for \
             the day (never a guessed expiry)"
        );
        let mut audit_writer = RestFetchAuditWriter::new(&params.questdb);
        let trading_date_nanos = minute_open_ist_nanos(today, 0);
        for (underlying, _exchange, groww_symbol) in GROWW_CHAIN_1M_UNDERLYINGS {
            if degraded.contains(&underlying) {
                let row = build_chain_audit_row(
                    trading_date_nanos,
                    trading_date_nanos,
                    stable_index_security_id(groww_symbol),
                    underlying,
                    0,
                    0,
                    -1,
                    -1,
                    0,
                    RestFetchOutcome::Error,
                    "no_expiry",
                );
                chain_audit_append_best_effort(&mut audit_writer, &row);
            }
        }
        chain_audit_flush_best_effort(&mut audit_writer);
        params
            .notifier
            .notify(NotificationEvent::GrowwChain1mExpiryUnresolved { detail });
    }
    drop(rows);
    if targets.is_empty() {
        // Every underlying degraded — nothing to fetch today (the page
        // above already fired).
        disabled_for_day.store(true, Ordering::SeqCst);
        return None;
    }
    for t in &targets {
        info!(
            symbol = t.underlying,
            exchange = t.exchange,
            security_id = t.security_id,
            expiry = %t.expiry_str,
            "groww_chain_1m: current expiry resolved from the instruments master"
        );
    }
    *targets_cache.lock().await = Some((today, targets.clone()));
    Some(targets)
}

/// The armed per-minute fire loop (split from `run_groww_chain_1m` by the
/// MEDIUM-4 warmup-cache refactor — the body is the pre-split loop,
/// unchanged).
// TEST-EXEMPT: live-deps scheduler loop — every decision inside is a unit-tested pure fn (next_fire_after / fire_is_fresh / count_missed_boundaries / stale_wake_backoff_ms / groww_min_gap_wait_ms).
async fn run_groww_chain_minute_loop(
    params: &GrowwChain1mTaskParams,
    client: &reqwest::Client,
    targets: Vec<GrowwChainTarget>,
) {
    let mut writer = OptionChain1mWriter::new_with_feed(
        &params.questdb,
        OPTION_CHAIN_1M_FEED_GROWW,
        OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
    );
    let mut audit_writer = RestFetchAuditWriter::new(&params.questdb);
    let mut edge = FailureEdge::default();
    let mut token_cache = GrowwTokenCache::new_chain();
    let mut last_fired: Option<u32> = None;
    // ONE scalar spanning consecutive chain requests (any underlying) —
    // the cross-request min-gap pacing state (MEDIUM-1).
    let mut last_request_ms: Option<i64> = None;
    let mut spot_rx = params.spot_minute_done.clone();
    info!(
        underlyings = targets.len(),
        "groww_chain_1m: per-minute chain fetch loop armed (fires each \
         minute close 09:16:00-15:30:00 IST, right after the Groww spot leg; \
         sequential underlying pacing)"
    );

    loop {
        // Audit Rule 3: re-read the wall clock + trading-day verdict EVERY
        // iteration (a suspend can cross midnight and stale the verdict).
        if !params.calendar.is_trading_day_today() {
            info!("groww_chain_1m: no longer a trading day — exiting");
            return;
        }
        let iter_date = today_ist();
        let now = ist_secs_of_day_now();
        let Some(fire) = next_fire_after(now, last_fired) else {
            info!("groww_chain_1m: past 15:30 IST — today's minute fires complete");
            return;
        };
        // Sequenced wake: the Groww spot leg's minute-done signal, bounded
        // by the fallback timer (the reused Dhan wait core).
        let sleep_ms = u64::from(fire.saturating_sub(now)).saturating_mul(1_000)
            + GROWW_CHAIN_1M_FALLBACK_DELAY_MS;
        wait_for_signal_or_fallback(sleep_ms, fire, &mut spot_rx).await;

        // Staleness gate (suspend / clock-step defense): skip + recompute,
        // never fetch a long-gone minute.
        let woke = ist_secs_of_day_now();
        if !fire_is_fresh(fire, woke) {
            warn!(
                fire_secs = fire,
                woke_at_secs = woke,
                "groww_chain_1m: woke too far past the minute boundary \
                 (suspend/clock step?) — skipping this minute"
            );
            let missed = count_missed_boundaries(fire.saturating_sub(60), woke.saturating_sub(1));
            record_groww_chain_skipped_boundaries(
                params,
                &mut edge,
                &mut audit_writer,
                &targets,
                missed,
                fire,
                iter_date,
            );
            if woke > fire {
                last_fired = Some((woke.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
            }
            let backoff_ms = stale_wake_backoff_ms(fire, woke);
            if backoff_ms > 0 {
                // Clock stepped BACK across the boundary: the already-
                // satisfied spot signal would make the next wait return
                // with ZERO awaits — sleep up to the fire moment instead
                // of busy-spinning (the Dhan chain H1 defense).
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }
            continue;
        }

        fire_one_groww_chain_minute(
            params,
            client,
            &targets,
            &mut last_request_ms,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut token_cache,
            fire,
        )
        .await;
        last_fired = Some(fire);
        // Overrun accounting: boundaries that fully elapsed DURING the
        // fire can never be fetched — count them loudly + feed the edge.
        let after = ist_secs_of_day_now();
        let missed = count_missed_boundaries(fire, after.saturating_sub(1));
        record_groww_chain_skipped_boundaries(
            params,
            &mut edge,
            &mut audit_writer,
            &targets,
            missed,
            fire + 60,
            iter_date,
        );
        if missed > 0 {
            last_fired = Some((after.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
        }
    }
}

/// Spawn the supervised Groww per-minute chain scheduler. The supervisor
/// respawns a dead/failed run after a bounded backoff, and exits cleanly
/// once today's window is over, on graceful-shutdown cancel, or after a
/// warmup disabled-for-the-day stop (deliberately NOT respawned — the
/// pipeline stays down; tomorrow's boot re-warms).
// TEST-EXEMPT: tokio supervisor wiring over the unit-tested pure decisions (spot_1m_day_is_over / classify_join_exit / the disabled-for-day latch); spawn site pinned by crates/app/tests/groww_chain_1m_wiring_guard.rs.
pub fn spawn_supervised_groww_chain_1m(
    params: GrowwChain1mTaskParams,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let disabled_for_day = Arc::new(AtomicBool::new(false));
        // Shared across respawns (MEDIUM-4): the day's resolved targets —
        // a flapping backoff loop never re-downloads the multi-MB master.
        let targets_cache: GrowwChainTargetsCache = Arc::new(tokio::sync::Mutex::new(None));
        loop {
            let inner = tokio::spawn(run_groww_chain_1m(
                params.clone(),
                Arc::clone(&disabled_for_day),
                Arc::clone(&targets_cache),
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
                        "groww_chain_1m: pipeline disabled for the day \
                         (unresolvable expiries) — supervisor exiting"
                    );
                    return;
                }
                Ok(()) if day_over => {
                    info!("groww_chain_1m: day complete — supervisor exiting");
                    return;
                }
                Err(join_err) if join_err.is_cancelled() => {
                    // Graceful shutdown teardown — not an abort.
                    return;
                }
                _ => {}
            }
            metrics::counter!("tv_groww_chain1m_task_respawn_total", "reason" => reason)
                .increment(1);
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "task_respawn",
                feed = OPTION_CHAIN_1M_FEED_GROWW,
                reason,
                "CHAIN-02: Groww per-minute chain fetch task died mid-window \
                 — respawning after backoff"
            );
            tokio::time::sleep(Duration::from_secs(GROWW_CHAIN_1M_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Boot-time probe (pipeline OFF, probe-and-report ON)
// ---------------------------------------------------------------------------

/// One underlying's probe measurement (pure formatting input).
#[derive(Clone, Debug, PartialEq)]
pub struct GrowwChainProbeResult {
    /// PLAIN underlying symbol.
    pub underlying: &'static str,
    /// `Some((strikes, legs, latency_ms, payload_bytes))` on a parseable
    /// chain; `None` = the bounded failure slug in `failure`.
    pub measured: Option<(u32, usize, i64, usize)>,
    /// Bounded failure description (already redacted) when not measured.
    pub failure: Option<String>,
}

/// Is the probe instant inside the in-session window `[09:16, 15:30)`
/// IST where a live chain payload is expected? Outside it a thin / empty
/// / stale payload is NOT a failure verdict — the endpoint legitimately
/// serves stale-or-nothing off-hours (LOW-3 probe honesty). Pure.
#[must_use]
pub fn probe_in_session(secs_of_day: u32) -> bool {
    (9 * 3600 + 16 * 60..15 * 3600 + 30 * 60).contains(&secs_of_day)
}

/// The out-of-hours caveat prefixed to a NON-passing probe verdict when
/// the probe ran outside [`probe_in_session`] — an off-hours miss must
/// never read as a false "did NOT pass" (LOW-3). Pure.
#[must_use]
pub fn probe_out_of_hours_caveat(ok: bool, in_session: bool) -> Option<&'static str> {
    (!ok && !in_session).then_some(
        "measured OUTSIDE market hours — payload may be stale/empty; re-probe in session",
    )
}

/// Plain-English probe verdict detail — bounded, measured numbers only
/// (operator Quote 2: show the MEASURED close-to-data/latency, never
/// assert). Pure.
#[must_use]
pub fn format_probe_detail(results: &[GrowwChainProbeResult]) -> (bool, String) {
    let mut ok = !results.is_empty();
    let mut parts = Vec::with_capacity(results.len());
    for r in results {
        match (&r.measured, &r.failure) {
            (Some((strikes, legs, latency_ms, bytes)), _) => {
                parts.push(format!(
                    "{}: {strikes} strikes / {legs} contract prices in {:.1}s ({} KB)",
                    r.underlying,
                    (*latency_ms).max(0) as f64 / 1_000.0,
                    bytes / 1_024
                ));
            }
            (None, Some(failure)) => {
                ok = false;
                parts.push(format!("{}: {failure}", r.underlying));
            }
            (None, None) => {
                ok = false;
                parts.push(format!("{}: not measured", r.underlying));
            }
        }
    }
    (ok, parts.join("; "))
}

/// Probe-only path (`enabled = false` + `probe_and_report = true`): resolve
/// today's expiries from the instruments master, then ONE bounded chain
/// call per underlying, verdict via an Info Telegram + coded log, persist
/// NOTHING, exit. NEVER runs the pipeline.
// TEST-EXEMPT: live-deps async runner — the classification/formatting (parse_groww_option_chain / resolve_groww_chain_targets / format_probe_detail) is unit-tested below; wiring pinned by crates/app/tests/groww_chain_1m_wiring_guard.rs.
pub async fn run_groww_chain_1m_probe(params: GrowwChain1mTaskParams) {
    if !params.calendar.is_trading_day_today() {
        info!("groww_chain_1m: non-trading day — chain probe skipped");
        return;
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(GROWW_CHAIN_1M_REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "probe",
                feed = OPTION_CHAIN_1M_FEED_GROWW,
                ?err,
                "CHAIN-02: HTTP client build failed — the Groww chain probe \
                 did not run today"
            );
            return;
        }
    };
    let rows = match download_master_bounded().await {
        Ok(rows) => rows,
        Err(detail) => {
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "probe",
                feed = OPTION_CHAIN_1M_FEED_GROWW,
                detail = %capture_rest_error_body(&detail),
                "CHAIN-02: Groww instruments-master download failed — the \
                 chain probe is inconclusive today (tomorrow's boot re-probes)"
            );
            return;
        }
    };
    let (targets, degraded) = resolve_groww_chain_targets(&rows, today_ist());
    drop(rows);
    let mut token_cache = GrowwTokenCache::new_chain();
    let Some(token) = token_cache.ensure_token().await else {
        error!(
            code = ErrorCode::Chain02FetchDegraded.code_str(),
            stage = "probe",
            feed = OPTION_CHAIN_1M_FEED_GROWW,
            "CHAIN-02: no shared Groww access token at probe time — the \
             chain probe is inconclusive today (tomorrow's boot re-probes)"
        );
        return;
    };
    let mut results: Vec<GrowwChainProbeResult> =
        Vec::with_capacity(targets.len() + degraded.len());
    for underlying in &degraded {
        results.push(GrowwChainProbeResult {
            underlying,
            measured: None,
            failure: Some("no usable option expiry in today's contract list".to_string()),
        });
    }
    for (idx, target) in targets.iter().enumerate() {
        let url = &target.url;
        // Sequential + min-gap paced (one boot-time call per underlying).
        tokio::time::sleep(Duration::from_millis(GROWW_CHAIN_1M_MIN_GAP_MS)).await;
        let started = std::time::Instant::now();
        let result = groww_chain_fetch_once(&client, url, &target.expiry_str, &token).await;
        let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        match result {
            Ok(body) => {
                let payload_bytes = body.len();
                match parse_groww_option_chain(&body) {
                    Some(chain) if !chain.legs.is_empty() => {
                        results.push(GrowwChainProbeResult {
                            underlying: target.underlying,
                            measured: Some((
                                chain.strikes_kept,
                                chain.legs.len(),
                                latency_ms,
                                payload_bytes,
                            )),
                            failure: None,
                        });
                    }
                    Some(_) => results.push(GrowwChainProbeResult {
                        underlying: target.underlying,
                        measured: None,
                        failure: Some("answered but the chain carried zero strikes".to_string()),
                    }),
                    None => results.push(GrowwChainProbeResult {
                        underlying: target.underlying,
                        measured: None,
                        failure: Some(
                            "answered but the body was not a parseable option chain".to_string(),
                        ),
                    }),
                }
            }
            Err(failure) => {
                let auth_rejected = failure.auth_rejected;
                if auth_rejected {
                    token_cache.note_auth_rejected();
                }
                results.push(GrowwChainProbeResult {
                    underlying: target.underlying,
                    measured: None,
                    // Already secret-redacted + bounded at capture.
                    failure: Some(failure.msg),
                });
                if auth_rejected {
                    // LOW-3: the item-12 short-circuit, mirrored into the
                    // probe — every further request with the same rejected
                    // token is a doomed 401; report the rest as skipped.
                    for skipped in &targets[idx + 1..] {
                        results.push(GrowwChainProbeResult {
                            underlying: skipped.underlying,
                            measured: None,
                            failure: Some(
                                "skipped — an earlier underlying hit an auth-class \
                                 reject (no doomed requests)"
                                    .to_string(),
                            ),
                        });
                    }
                    break;
                }
            }
        }
    }
    let (ok, detail) = format_probe_detail(&results);
    let in_session = probe_in_session(ist_secs_of_day_now());
    let detail = match probe_out_of_hours_caveat(ok, in_session) {
        Some(caveat) => format!("{caveat}: {detail}"),
        None => detail,
    };
    if ok {
        info!(
            detail = %detail,
            config_key = "[groww_option_chain_1m].enabled",
            "groww_chain_1m: probe PASSED — chain data answered for every \
             underlying; pipeline stays OFF until the config is flipped \
             (the Telegram body carries the plain-English action; the exact \
             key lives HERE)"
        );
    } else if in_session {
        error!(
            code = ErrorCode::Chain02FetchDegraded.code_str(),
            stage = "probe",
            feed = OPTION_CHAIN_1M_FEED_GROWW,
            detail = %detail,
            "CHAIN-02: Groww chain probe did NOT pass — verdict reported; \
             pipeline stays OFF (tomorrow's boot re-probes)"
        );
    } else {
        // LOW-3 probe honesty: an off-hours miss is NOT a failure verdict
        // — the endpoint legitimately serves stale/empty payloads outside
        // the session. The Telegram carries the same caveated detail; no
        // coded failure is asserted.
        warn!(
            detail = %detail,
            "groww_chain_1m: probe measured outside market hours — payload \
             may be stale/empty; re-probe in session before reading this \
             as a failure"
        );
    }
    params
        .notifier
        .notify(NotificationEvent::GrowwChain1mProbeVerdict { ok, detail });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- URL builder -----------------------------------------------------

    #[test]
    fn test_groww_chain_url_shape_plain_symbol_no_token() {
        let url = groww_chain_url("NSE", "NIFTY");
        assert_eq!(
            url,
            "https://api.groww.in/v1/option-chain/exchange/NSE/underlying/NIFTY"
        );
        // The PLAIN symbol travels in the path — never the groww_symbol,
        // never the token, never the expiry (query param via the client).
        assert!(!url.contains("NSE-NIFTY"));
        assert!(!url.contains("token"));
        assert!(!url.contains("expiry"));
        assert_eq!(
            groww_chain_url("BSE", "SENSEX"),
            "https://api.groww.in/v1/option-chain/exchange/BSE/underlying/SENSEX"
        );
    }

    // ---- response parsing -------------------------------------------------

    /// The docs-verbatim payload-wrapped shape (14-option-chain.md §2).
    const SAMPLE_WRAPPED: &str = r#"{
        "status": "SUCCESS",
        "payload": {
            "underlying_ltp": 25641.7,
            "strikes": {
                "23400": {
                    "CE": {
                        "greeks": {"delta": 0.9936, "gamma": 0, "theta": -1.0787,
                                   "vega": 0.6943, "rho": 5.1802, "iv": 25.3409},
                        "trading_symbol": "NIFTY25N1823400CE",
                        "ltp": 2200, "open_interest": 7, "volume": 5
                    },
                    "PE": {
                        "greeks": {"delta": -0.0064, "gamma": 0, "theta": -1.0787,
                                   "vega": 0.6943, "rho": -0.0373, "iv": 25.3409},
                        "trading_symbol": "NIFTY25N1823400PE",
                        "ltp": 2.05, "open_interest": 7453, "volume": 9339
                    }
                }
            }
        }
    }"#;

    #[test]
    fn test_parse_groww_option_chain_payload_wrapped_docs_verbatim() {
        let chain = parse_groww_option_chain(SAMPLE_WRAPPED).expect("parses");
        assert!((chain.underlying_ltp - 25_641.7).abs() < 1e-9);
        assert_eq!(chain.strikes_kept, 1);
        assert_eq!(chain.legs.len(), 2);
        assert_eq!(chain.invalid_strikes, 0);
        let ce = &chain.legs[0];
        assert_eq!(ce.leg, "CE");
        assert!((ce.strike - 23_400.0).abs() < 1e-9);
        assert!((ce.ltp - 2_200.0).abs() < 1e-9);
        assert!((ce.rho - 5.1802).abs() < 1e-9, "rho parsed from greeks");
        assert!((ce.iv - 25.3409).abs() < 1e-9);
        assert_eq!(ce.oi, 7);
        assert_eq!(ce.volume, 5);
        let pe = &chain.legs[1];
        assert_eq!(pe.leg, "PE");
        assert!((pe.rho - (-0.0373)).abs() < 1e-9);
        assert_eq!(pe.oi, 7_453);
    }

    #[test]
    fn test_parse_groww_option_chain_bare_shape_and_one_sided_strikes() {
        // The SDK-unwrapped bare shape; a one-sided strike (CE null / PE
        // absent) contributes only the present legs — Option<> discipline.
        let body = r#"{
            "underlying_ltp": 81234.5,
            "strikes": {
                "81000": { "CE": null, "PE": {"ltp": 12.5, "open_interest": 10, "volume": 3} },
                "81100": { "CE": {"ltp": 300.0} }
            }
        }"#;
        let chain = parse_groww_option_chain(body).expect("parses");
        assert_eq!(chain.strikes_kept, 2);
        assert_eq!(chain.legs.len(), 2, "null CE + absent PE are skipped");
        // Field wobble: a leg without greeks degrades per-field to 0.0.
        let bare_ce = chain
            .legs
            .iter()
            .find(|l| l.leg == "CE")
            .expect("CE present");
        assert!((bare_ce.ltp - 300.0).abs() < 1e-9);
        assert!((bare_ce.rho).abs() < 1e-9, "missing greeks → 0.0");
        assert_eq!(bare_ce.oi, 0, "missing open_interest → 0");
        assert!(
            !chain.underlying_ltp_missing,
            "a present numeric underlying_ltp is not flagged missing"
        );
    }

    #[test]
    fn test_parse_groww_option_chain_underlying_ltp_absence_is_flagged() {
        // LOW-5: absent / null / non-numeric underlying_ltp → 0.0 WITH the
        // missing flag set (vendor omission ≠ a real 0); a present numeric
        // value (even 0.0) is NOT flagged.
        for body in [
            r#"{"payload":{"strikes":{"100":{"CE":{"ltp":1.0}}}}}"#,
            r#"{"payload":{"underlying_ltp":null,"strikes":{"100":{"CE":{"ltp":1.0}}}}}"#,
            r#"{"payload":{"underlying_ltp":"25641.7","strikes":{"100":{"CE":{"ltp":1.0}}}}}"#,
        ] {
            let chain = parse_groww_option_chain(body).expect("parses");
            assert!(chain.underlying_ltp_missing, "flagged for {body}");
            assert!(chain.underlying_ltp.abs() < 1e-12);
        }
        let real_zero = parse_groww_option_chain(
            r#"{"underlying_ltp":0.0,"strikes":{"100":{"CE":{"ltp":1.0}}}}"#,
        )
        .expect("parses");
        assert!(
            !real_zero.underlying_ltp_missing,
            "a PRESENT numeric 0.0 is a real value, never flagged as absence"
        );
    }

    #[test]
    fn test_parse_groww_option_chain_hostile_shapes_never_panic() {
        // Malformed top-level shapes → None (the parse-failure arm).
        assert_eq!(parse_groww_option_chain("not json"), None);
        assert_eq!(parse_groww_option_chain("{}"), None);
        assert_eq!(parse_groww_option_chain(r#"{"strikes": []}"#), None);
        assert_eq!(parse_groww_option_chain(r#"{"strikes": "x"}"#), None);
        // The FAILURE error envelope has no strikes map → None too.
        assert_eq!(
            parse_groww_option_chain(
                r#"{"status":"FAILURE","error":{"code":"GA001","message":"x"}}"#
            ),
            None
        );
        // A well-formed chain with ZERO strikes → empty legs (the
        // outcome="empty" arm), never None.
        let empty = parse_groww_option_chain(r#"{"payload":{"underlying_ltp":1.0,"strikes":{}}}"#)
            .expect("empty chain parses");
        assert!(empty.legs.is_empty());
        assert_eq!(empty.strikes_kept, 0);
    }

    #[test]
    fn test_parse_groww_option_chain_invalid_and_absurd_strikes_counted() {
        let body = r#"{
            "payload": {
                "underlying_ltp": 100.0,
                "strikes": {
                    "not-a-number": { "CE": {"ltp": 1.0} },
                    "-5": { "CE": {"ltp": 1.0} },
                    "1e300": { "CE": {"ltp": 1.0} },
                    "25650.5": { "CE": {"ltp": 1.0} }
                }
            }
        }"#;
        let chain = parse_groww_option_chain(body).expect("parses");
        // Decimal strike keys parse (the U-11 Unknown, handled); the
        // hostile keys are skipped + counted, never a row.
        assert_eq!(chain.strikes_kept, 1);
        assert_eq!(chain.invalid_strikes, 3);
        assert_eq!(chain.legs.len(), 1);
        assert!((chain.legs[0].strike - 25_650.5).abs() < 1e-9);
    }

    #[test]
    fn test_parse_groww_option_chain_strike_cap_truncates_and_counts() {
        // A hostile/corrupt body inside the byte cap can never mint
        // unbounded rows: strikes past MAX_STRIKES_PER_CHAIN are dropped
        // + counted (the Dhan chain cap, reused).
        let mut strikes = String::new();
        for i in 0..(MAX_STRIKES_PER_CHAIN + 25) {
            if i > 0 {
                strikes.push(',');
            }
            strikes.push_str(&format!(r#""{}": {{"CE": {{"ltp": 1.0}}}}"#, 10_000 + i));
        }
        let body = format!(r#"{{"payload":{{"underlying_ltp":1.0,"strikes":{{{strikes}}}}}}}"#);
        let chain = parse_groww_option_chain(&body).expect("parses");
        assert_eq!(chain.strikes_kept as usize, MAX_STRIKES_PER_CHAIN);
        assert_eq!(chain.truncated_strikes, 25);
        assert_eq!(chain.legs.len(), MAX_STRIKES_PER_CHAIN);
    }

    // ---- pacing / classification ------------------------------------------

    #[test]
    fn test_groww_min_gap_wait_ms_engages_only_inside_gap() {
        // Never requested → no wait.
        assert_eq!(groww_min_gap_wait_ms(None, 1_000), 0);
        // Well past the gap (the normal ~60s cadence) → no wait.
        assert_eq!(groww_min_gap_wait_ms(Some(1_000), 62_000), 0);
        // Inside the gap → the remainder.
        assert_eq!(groww_min_gap_wait_ms(Some(1_000), 1_400), 600);
        // Exactly at the gap boundary → no wait.
        assert_eq!(
            groww_min_gap_wait_ms(Some(1_000), 1_000 + GROWW_CHAIN_1M_MIN_GAP_MS as i64),
            0
        );
        // Backwards clock (midnight wrap) → never a long spurious sleep.
        assert_eq!(groww_min_gap_wait_ms(Some(50_000), 1_000), 0);
    }

    #[test]
    fn test_chain_error_class_slugs_bounded() {
        assert_eq!(chain_error_class_for_status(0), "transport");
        assert_eq!(chain_error_class_for_status(401), "auth");
        assert_eq!(chain_error_class_for_status(403), "auth");
        assert_eq!(chain_error_class_for_status(429), "rate_limited");
        assert_eq!(chain_error_class_for_status(404), "http_4xx");
        assert_eq!(chain_error_class_for_status(504), "http_5xx");
        assert_eq!(chain_error_class_for_status(302), "http_other");
    }

    #[test]
    fn test_chain_audit_outcome_mapping() {
        assert_eq!(
            chain_audit_outcome(true, false, false),
            RestFetchOutcome::Ok
        );
        assert_eq!(
            chain_audit_outcome(false, true, false),
            RestFetchOutcome::Empty
        );
        assert_eq!(
            chain_audit_outcome(false, false, true),
            RestFetchOutcome::RateLimited
        );
        assert_eq!(
            chain_audit_outcome(false, false, false),
            RestFetchOutcome::Error
        );
    }

    #[test]
    fn test_build_chain_audit_row_stamps_groww_chain_leg() {
        let row = build_chain_audit_row(
            1_770_000_900_000_000_000,
            1_769_990_400_000_000_000,
            4_611_686_018_427_387_905,
            "NIFTY",
            1,
            200,
            143,
            1_042,
            0,
            RestFetchOutcome::Ok,
            "none",
        );
        assert_eq!(row.feed, "groww");
        assert_eq!(row.leg, "chain_1m");
        assert_eq!(row.exchange_segment, "IDX_I");
        assert_eq!(row.attempts, 1, "the chain has no in-minute ladder");
        assert_eq!(row.close_to_data_ms, 1_042);
    }

    // ---- warmup target resolution ------------------------------------------

    fn master_option_row(
        exchange: &str,
        underlying: &str,
        instrument_type: &str,
        expiry: &str,
    ) -> GrowwInstrumentRow {
        GrowwInstrumentRow {
            exchange: exchange.to_string(),
            exchange_token: "66751".to_string(),
            groww_symbol: format!("{exchange}-{underlying}-opt"),
            name: String::new(),
            instrument_type: instrument_type.to_string(),
            segment: "FNO".to_string(),
            series: String::new(),
            isin: String::new(),
            underlying_symbol: underlying.to_string(),
            expiry_date: expiry.to_string(),
        }
    }

    #[test]
    fn test_resolve_groww_chain_targets_full_and_partial_degrade() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date");
        let rows = vec![
            master_option_row("NSE", "NIFTY", "CE", "2026-07-16"),
            master_option_row("NSE", "BANKNIFTY", "PE", "2026-07-30"),
            master_option_row("BSE", "SENSEX", "CE", "2026-07-14"),
        ];
        let (targets, degraded) = resolve_groww_chain_targets(&rows, d("2026-07-13"));
        assert_eq!(targets.len(), 3);
        assert!(degraded.is_empty());
        assert_eq!(targets[0].underlying, "NIFTY");
        assert_eq!(targets[0].exchange, "NSE");
        assert_eq!(targets[0].expiry_str, "2026-07-16");
        // The stable id joins the Groww live lane's index-id space.
        assert_eq!(
            targets[0].security_id,
            stable_index_security_id("NSE-NIFTY")
        );
        assert_eq!(targets[2].underlying, "SENSEX");
        assert_eq!(targets[2].expiry_str, "2026-07-14");

        // Master lacking SENSEX (BSE) option rows → SENSEX degrades BY
        // NAME; NIFTY/BANKNIFTY keep running (per-underlying degrade).
        let partial = vec![
            master_option_row("NSE", "NIFTY", "CE", "2026-07-16"),
            master_option_row("NSE", "BANKNIFTY", "PE", "2026-07-30"),
        ];
        let (targets, degraded) = resolve_groww_chain_targets(&partial, d("2026-07-13"));
        assert_eq!(targets.len(), 2);
        assert_eq!(degraded, vec!["SENSEX"]);
        let detail = expiry_degrade_detail(&degraded, partial.len());
        assert!(detail.contains("SENSEX"), "got: {detail}");
        assert!(detail.contains("no usable option expiry"), "got: {detail}");

        // Empty master → everything degrades (the whole-day stop).
        let (none, all_degraded) = resolve_groww_chain_targets(&[], d("2026-07-13"));
        assert!(none.is_empty());
        assert_eq!(all_degraded, vec!["NIFTY", "BANKNIFTY", "SENSEX"]);
    }

    // ---- failure edge (chain-specific instance of the shared machine) ------

    #[test]
    fn test_chain_failure_edge_pages_at_three_and_recovers_once() {
        let mut edge = FailureEdge::default();
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        assert_eq!(
            edge.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        );
        // Sustained failure never re-pages (edge-triggered).
        assert_eq!(edge.record_minute(true), EdgeAction::None);
        // First success after a paged episode → one recovery ping.
        assert_eq!(
            edge.record_minute(false),
            EdgeAction::Recover { failed_minutes: 4 }
        );
        assert_eq!(edge.record_minute(false), EdgeAction::None);
    }

    // ---- probe verdict formatting -------------------------------------------

    #[test]
    fn test_format_probe_detail_measured_and_failed() {
        let results = vec![
            GrowwChainProbeResult {
                underlying: "NIFTY",
                measured: Some((102, 200, 812, 148_480)),
                failure: None,
            },
            GrowwChainProbeResult {
                underlying: "SENSEX",
                measured: None,
                failure: Some("http 403".to_string()),
            },
        ];
        let (ok, detail) = format_probe_detail(&results);
        assert!(!ok, "any failure → not ok");
        assert!(detail.contains("NIFTY: 102 strikes"), "got: {detail}");
        assert!(detail.contains("0.8s"), "latency rendered: {detail}");
        assert!(detail.contains("145 KB"), "bytes rendered: {detail}");
        assert!(detail.contains("SENSEX: http 403"), "got: {detail}");

        let all_ok = vec![GrowwChainProbeResult {
            underlying: "NIFTY",
            measured: Some((90, 178, 1_204, 102_400)),
            failure: None,
        }];
        let (ok, detail) = format_probe_detail(&all_ok);
        assert!(ok);
        assert!(detail.contains("1.2s"), "got: {detail}");

        // No measurements at all is never a pass (Rule 11 — no false-OK).
        let (ok, _) = format_probe_detail(&[]);
        assert!(!ok);
    }

    #[test]
    fn test_probe_in_session_window_boundaries() {
        // [09:16:00, 15:30:00) IST — the first minute close with a live
        // chain through the session's last minute close.
        assert!(!probe_in_session(9 * 3600 + 15 * 60 + 59));
        assert!(probe_in_session(9 * 3600 + 16 * 60));
        assert!(probe_in_session(12 * 3600));
        assert!(probe_in_session(15 * 3600 + 29 * 60 + 59));
        assert!(!probe_in_session(15 * 3600 + 30 * 60));
        assert!(!probe_in_session(0));
        assert!(!probe_in_session(23 * 3600));
    }

    #[test]
    fn test_probe_out_of_hours_caveat_gates_on_verdict_and_session() {
        // LOW-3: ONLY a non-pass measured OUTSIDE the session carries the
        // stale/empty caveat; a pass never does, and an in-session miss is
        // a genuine failure verdict.
        let caveat = probe_out_of_hours_caveat(false, false).expect("caveated");
        assert!(caveat.contains("OUTSIDE market hours"), "got: {caveat}");
        assert!(caveat.contains("re-probe in session"), "got: {caveat}");
        assert!(probe_out_of_hours_caveat(false, true).is_none());
        assert!(probe_out_of_hours_caveat(true, false).is_none());
        assert!(probe_out_of_hours_caveat(true, true).is_none());
    }

    // ---- sequencing (the reused wait core with the GROWW fallback) ---------

    /// The GROWW chain leg reuses the Dhan wait core with its OWN fallback
    /// constant: no receiver (spot disabled) → the fallback timer paces;
    /// a spot signal for the minute wakes it early.
    #[tokio::test]
    async fn test_groww_chain_wait_fallback_paces_and_signal_wakes_early() {
        use tokio::sync::watch;
        let fire = 10 * 3600;
        const FALLBACK_MS: u64 = 150;
        let fb = Duration::from_millis(FALLBACK_MS);

        // (a) No receiver: the fallback timer owns pacing.
        let started = std::time::Instant::now();
        wait_for_signal_or_fallback(FALLBACK_MS, fire, &mut None).await;
        assert!(started.elapsed() >= fb, "must wait the full fallback");

        // (b) The Groww spot leg signals this minute → early wake.
        let (tx, rx) = watch::channel::<Option<u32>>(None);
        let mut rx_opt = Some(rx);
        let waiter = tokio::spawn(async move {
            let started = std::time::Instant::now();
            wait_for_signal_or_fallback(FALLBACK_MS, fire, &mut rx_opt).await;
            started.elapsed()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send_replace(Some(fire));
        let waited = waiter.await.expect("waiter completes");
        assert!(
            waited < fb,
            "signal must wake before the fallback: {waited:?}"
        );
    }

    // ---- HTTP leg (hermetic mock server — round-1 coverage battery) --------

    /// Minimal one-shot HTTP mock: accepts connections forever, answers
    /// each request with the fixed response bytes (the spot module's
    /// mock-401 precedent).
    async fn spawn_chain_mock(response: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        format!("http://127.0.0.1:{port}/v1/option-chain/exchange/NSE/underlying/NIFTY")
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client")
    }

    #[tokio::test]
    async fn test_fetch_bounded_found_empty_and_parse_failure_via_mock() {
        let token = SecretString::from("test-token");

        // (a) A parseable chain → Found with the measured freshness signal
        // + payload bytes; the request stamp + latency are real readings.
        let body = r#"{"payload":{"underlying_ltp":100.5,"strikes":{"100":{"CE":{"ltp":1.5},"PE":{"ltp":2.5}}}}}"#;
        // Leak one response string per case — test-only, bounded.
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let url = spawn_chain_mock(resp).await;
        let (outcome, requested_at_ms, latency_ms) =
            fetch_groww_chain_bounded(&test_client(), &url, "2026-07-16", &token, 0, None).await;
        match outcome {
            GrowwChainFetchOutcome::Found {
                chain,
                close_to_data_ms,
                payload_bytes,
            } => {
                assert_eq!(chain.legs.len(), 2);
                assert!(!chain.underlying_ltp_missing);
                assert!(close_to_data_ms >= 0);
                assert_eq!(payload_bytes, body.len());
            }
            other => panic!("expected Found, got {other:?}"),
        }
        assert!(requested_at_ms >= 0);
        assert!(latency_ms >= 0);

        // (b) A parseable 2xx with ZERO strikes → Empty (never silent).
        let empty_body = r#"{"payload":{"underlying_ltp":1.0,"strikes":{}}}"#;
        let empty_resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{empty_body}",
                empty_body.len()
            )
            .into_boxed_str(),
        );
        let url = spawn_chain_mock(empty_resp).await;
        let (outcome, _, _) =
            fetch_groww_chain_bounded(&test_client(), &url, "2026-07-16", &token, 0, None).await;
        assert_eq!(outcome, GrowwChainFetchOutcome::Empty);

        // (c) A 2xx that is not a parseable option chain → Failed(200).
        let url = spawn_chain_mock("HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnot-json").await;
        let (outcome, _, _) =
            fetch_groww_chain_bounded(&test_client(), &url, "2026-07-16", &token, 0, None).await;
        match outcome {
            GrowwChainFetchOutcome::Failed(f) => {
                assert_eq!(f.status, 200);
                assert!(!f.rate_limited && !f.auth_rejected);
                assert!(f.msg.contains("not a parseable"), "got: {}", f.msg);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_fetch_once_classifies_auth_rate_limit_5xx_and_transport() {
        let token = SecretString::from("test-token");
        let client = test_client();

        // 401 + 403 → auth-class (status-typed, never a substring scan).
        for resp in [
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 2\r\n\r\n{}",
        ] {
            let url = spawn_chain_mock(resp).await;
            let failure = groww_chain_fetch_once(&client, &url, "2026-07-16", &token)
                .await
                .expect_err("non-2xx must fail");
            assert!(failure.auth_rejected, "got: {failure:?}");
            assert!(!failure.rate_limited);
        }

        // 429 with Retry-After → rate_limited + the live-probe (e) shape
        // (Retry-After presence captured into the bounded msg).
        let url = spawn_chain_mock(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        let failure = groww_chain_fetch_once(&client, &url, "2026-07-16", &token)
            .await
            .expect_err("429 must fail");
        assert!(failure.rate_limited);
        assert!(!failure.auth_rejected);
        assert_eq!(failure.status, 429);
        assert!(
            failure.msg.contains("retry_after_present=true"),
            "got: {}",
            failure.msg
        );

        // 500 → neither flag; the bounded body capture rides in msg.
        let url =
            spawn_chain_mock("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2\r\n\r\n{}")
                .await;
        let failure = groww_chain_fetch_once(&client, &url, "2026-07-16", &token)
            .await
            .expect_err("5xx must fail");
        assert_eq!(failure.status, 500);
        assert!(!failure.rate_limited && !failure.auth_rejected);

        // Transport (closed port) → status 0, "send:" prefix.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let dead_port = dead.local_addr().expect("addr").port();
        drop(dead);
        let url = format!("http://127.0.0.1:{dead_port}/v1/option-chain");
        let failure = groww_chain_fetch_once(&client, &url, "2026-07-16", &token)
            .await
            .expect_err("closed port must fail");
        assert_eq!(failure.status, 0);
        assert!(failure.msg.starts_with("send:"), "got: {}", failure.msg);
    }

    #[tokio::test]
    async fn test_fetch_once_rejects_oversized_declared_body() {
        // Declared Content-Length past the 8 MiB cap → refused BEFORE the
        // stream is read (§18 body-cap discipline); surfaces as a 200-class
        // failure whose msg names the cap.
        let declared = GROWW_CHAIN_1M_MAX_BODY_BYTES + 1;
        let resp: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\n\r\n").into_boxed_str(),
        );
        let url = spawn_chain_mock(resp).await;
        let token = SecretString::from("test-token");
        let failure = groww_chain_fetch_once(&test_client(), &url, "2026-07-16", &token)
            .await
            .expect_err("oversized declared body must fail");
        assert_eq!(failure.status, 200);
        assert!(
            failure.msg.contains("body too large"),
            "got: {}",
            failure.msg
        );
    }

    // ---- fire / verdict / boundary sinks (hermetic — no-token + edges) -----

    fn test_params() -> GrowwChain1mTaskParams {
        use tickvault_common::config::{NseHolidayEntry, TradingConfig};
        let cfg = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-01-26".to_string(),
                name: "Republic Day".to_string(),
            }],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        GrowwChain1mTaskParams {
            notifier: NotificationService::disabled(),
            calendar: Arc::new(
                TradingCalendar::from_config(&cfg).expect("synthetic calendar builds"),
            ),
            questdb: QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 8812,
                ilp_port: 9009,
            },
            spot_minute_done: None,
        }
    }

    fn test_target(underlying: &'static str, exchange: &'static str) -> GrowwChainTarget {
        test_target_with_url(underlying, exchange, groww_chain_url(exchange, underlying))
    }

    fn test_target_with_url(
        underlying: &'static str,
        exchange: &'static str,
        url: String,
    ) -> GrowwChainTarget {
        GrowwChainTarget {
            underlying,
            exchange,
            security_id: 1,
            expiry: NaiveDate::from_ymd_opt(2026, 7, 16).expect("valid date"),
            expiry_str: "2026-07-16".to_string(),
            url,
        }
    }

    fn epoch_ms_now() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("post-epoch clock")
                .as_millis(),
        )
        .expect("fits i64")
    }

    /// The no-token fire arm: every underlying is a counted full miss +
    /// one `no_token` forensics row each; nothing is persisted to the
    /// chain table and NO network request happens (the paced-out cache
    /// returns None without an SSM read).
    #[tokio::test]
    async fn test_fire_no_token_arm_counts_misses_and_writes_forensics() {
        let params = test_params();
        let targets = vec![test_target("NIFTY", "NSE"), test_target("SENSEX", "BSE")];
        let mut last_request_ms: Option<i64> = None;
        let mut writer = OptionChain1mWriter::for_test_with_feed(
            OPTION_CHAIN_1M_FEED_GROWW,
            OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
        );
        let mut audit_writer = RestFetchAuditWriter::for_test();
        let mut edge = FailureEdge::default();
        let mut token_cache = GrowwTokenCache::for_test_paced_out(epoch_ms_now());
        fire_one_groww_chain_minute(
            &params,
            &test_client(),
            &targets,
            &mut last_request_ms,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut token_cache,
            9 * 3600 + 16 * 60,
        )
        .await;
        assert_eq!(writer.pending(), 0, "no chain rows without a token");
        // The sink flushes at the end of every fire; the disconnected test
        // writer discards on flush — so pending is 0 AND the appends are
        // proven via the edge below (one fully-failed minute recorded).
        assert_eq!(audit_writer.pending(), 0, "flush ran (best-effort discard)");
        assert!(
            last_request_ms.is_none(),
            "no request happened — the pacing stamp must stay unset"
        );
        // The fire fed the edge EXACTLY one fully-failed minute: two more
        // failed minutes reach the 3-threshold page on the third total.
        assert!(matches!(edge.record_minute(true), EdgeAction::None));
        assert!(matches!(
            edge.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        ));
    }

    fn non_trading_params() -> GrowwChain1mTaskParams {
        use tickvault_common::config::{NseHolidayEntry, TradingConfig};
        let mut params = test_params();
        let cfg = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            // TODAY is a declared holiday → is_trading_day_today() is
            // false regardless of the weekday the test runs on.
            nse_holidays: vec![NseHolidayEntry {
                date: today_ist().format("%Y-%m-%d").to_string(),
                name: "synthetic test holiday".to_string(),
            }],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        params.calendar =
            Arc::new(TradingCalendar::from_config(&cfg).expect("synthetic calendar builds"));
        params
    }

    /// A non-trading day: the run degrades the DDL ensures loudly against
    /// the unreachable QuestDB, gates BEFORE any warmup/fetch, and the
    /// supervisor classifies the clean exit as day-over and stops (no
    /// respawn loop on a holiday).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_and_supervisor_exit_on_non_trading_day() {
        let params = non_trading_params();
        let cache: GrowwChainTargetsCache = Arc::new(tokio::sync::Mutex::new(None));
        let disabled = Arc::new(AtomicBool::new(false));
        tokio::time::timeout(
            Duration::from_secs(30),
            run_groww_chain_1m(params.clone(), Arc::clone(&disabled), Arc::clone(&cache)),
        )
        .await
        .expect("run must return promptly on a non-trading day");
        assert!(
            cache.lock().await.is_none(),
            "no warmup / master download on a non-trading day"
        );
        assert!(!disabled.load(Ordering::SeqCst));

        let handle = spawn_supervised_groww_chain_1m(params);
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("supervisor must exit day-over, never respawn on a holiday")
            .expect("supervisor join");
    }

    /// The token-path fire against a hermetic mock (target-carried URL):
    /// a Found underlying (with a vendor-omitted underlying_ltp — the
    /// LOW-5 warn arm) + an Empty underlying. The disconnected test
    /// writer's flush fails, so the persist gate counts the minute fully
    /// failed (M1) even though the fetch found a chain.
    #[tokio::test]
    async fn test_fire_token_path_found_and_empty_via_mock() {
        let params = test_params();
        // Found body: one strike, both legs, NO underlying_ltp (LOW-5 arm),
        // one invalid strike key (counted-skip arm).
        let body = r#"{"payload":{"strikes":{"100":{"CE":{"ltp":1.5},"PE":{"ltp":2.5}},"bogus":{"CE":{"ltp":9.0}}}}}"#;
        let found_resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let found_url = spawn_chain_mock(found_resp).await;
        let empty_body = r#"{"payload":{"underlying_ltp":1.0,"strikes":{}}}"#;
        let empty_resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{empty_body}",
                empty_body.len()
            )
            .into_boxed_str(),
        );
        let empty_url = spawn_chain_mock(empty_resp).await;
        let targets = vec![
            test_target_with_url("NIFTY", "NSE", found_url),
            test_target_with_url("SENSEX", "BSE", empty_url),
        ];
        let mut last_request_ms: Option<i64> = None;
        let mut writer = OptionChain1mWriter::for_test_with_feed(
            OPTION_CHAIN_1M_FEED_GROWW,
            OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
        );
        let mut audit_writer = RestFetchAuditWriter::for_test();
        let mut edge = FailureEdge::default();
        let mut token_cache = GrowwTokenCache::for_test_with_token(SecretString::from("t"));
        fire_one_groww_chain_minute(
            &params,
            &test_client(),
            &targets,
            &mut last_request_ms,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut token_cache,
            9 * 3600 + 16 * 60,
        )
        .await;
        assert!(
            last_request_ms.is_some(),
            "requests happened — the cross-request pacing stamp advanced"
        );
        // The fetch side found a chain (ok_count = 1) BUT the disconnected
        // test writer's flush fails → the persist gate (the spot M1
        // discipline) honestly counts the minute FULLY FAILED: the edge
        // holds 1, so two more failed minutes reach the 3-threshold page.
        assert!(matches!(edge.record_minute(true), EdgeAction::None));
        assert!(matches!(
            edge.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        ));
    }

    /// The token-path fire's auth short-circuit (the item-12 mirror): a
    /// 401 on the FIRST underlying drops the token, skips the remaining
    /// underlyings (no doomed requests), and the minute counts fully
    /// failed.
    #[tokio::test]
    async fn test_fire_token_path_auth_reject_short_circuits_via_mock() {
        let params = test_params();
        let url_401 =
            spawn_chain_mock("HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}").await;
        // The second target's port is CLOSED — a request there would be a
        // transport error; the short-circuit must never send it at all
        // (nothing observable would distinguish — the skip is proven by
        // the forensics count in the no-token test's pattern + coverage).
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let dead_port = dead.local_addr().expect("addr").port();
        drop(dead);
        let targets = vec![
            test_target_with_url("NIFTY", "NSE", url_401),
            test_target_with_url(
                "SENSEX",
                "BSE",
                format!("http://127.0.0.1:{dead_port}/v1/option-chain"),
            ),
        ];
        let mut last_request_ms: Option<i64> = None;
        let mut writer = OptionChain1mWriter::for_test_with_feed(
            OPTION_CHAIN_1M_FEED_GROWW,
            OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
        );
        let mut audit_writer = RestFetchAuditWriter::for_test();
        let mut edge = FailureEdge::default();
        let mut token_cache = GrowwTokenCache::for_test_with_token(SecretString::from("t"));
        fire_one_groww_chain_minute(
            &params,
            &test_client(),
            &targets,
            &mut last_request_ms,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut token_cache,
            9 * 3600 + 17 * 60,
        )
        .await;
        // Fully failed (401 + skipped) → the edge holds ONE failed minute:
        // the page fires on the third total.
        assert!(matches!(edge.record_minute(true), EdgeAction::None));
        assert!(matches!(
            edge.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        ));
    }

    /// The verdict sink's three edge arms execute without panicking and
    /// the edge state machine advances exactly as the pure FailureEdge
    /// contract says (page after the threshold, recover on the next ok).
    #[test]
    fn test_record_minute_verdict_page_recover_and_sub_edge_arms() {
        let params = test_params();
        let mut edge = FailureEdge::default();
        // Sub-edge failure (ok_count > 0) → the coalesced minute_failed arm.
        record_groww_chain_minute_verdict(&params, &mut edge, "9:16 AM", 2, 1, 0, false, None);
        // Fully-failed minutes up to the threshold → the Page arm fires
        // inside (edge-triggered — the FOLLOWING failed minute is None).
        for _ in 0..tickvault_common::constants::GROWW_CHAIN_1M_CONSECUTIVE_FAIL_PAGE_THRESHOLD {
            record_groww_chain_minute_verdict(
                &params,
                &mut edge,
                "9:17 AM",
                0,
                2,
                0,
                false,
                Some("NIFTY: http 500"),
            );
        }
        assert!(matches!(edge.record_minute(true), EdgeAction::None));
        // An ok minute AFTER the paged episode → the Recover arm.
        record_groww_chain_minute_verdict(&params, &mut edge, "9:21 AM", 2, 0, 0, false, None);
        assert!(matches!(edge.record_minute(true), EdgeAction::None));
        // A persist-gated failure counts fully-failed even with fetch OKs
        // (the spot M1 discipline — pure fn already pins it; this pins the
        // sink wiring).
        record_groww_chain_minute_verdict(&params, &mut edge, "9:22 AM", 2, 0, 0, true, None);
    }

    /// Boundary-skip accounting: zero skips is a no-op; a real skip writes
    /// one `outcome=skipped` forensics row per (minute, underlying); a wake
    /// that crossed IST midnight (iter_date ≠ today) skips the mis-dated
    /// rows (the item-7 guard) after the counter + coalesced log.
    #[test]
    fn test_record_skipped_boundaries_rows_and_midnight_guard() {
        let params = test_params();
        let targets = vec![test_target("NIFTY", "NSE"), test_target("SENSEX", "BSE")];
        let today = today_ist();

        let mut edge = FailureEdge::default();
        let mut audit_writer = RestFetchAuditWriter::for_test();
        record_groww_chain_skipped_boundaries(
            &params,
            &mut edge,
            &mut audit_writer,
            &targets,
            0,
            9 * 3600 + 17 * 60,
            today,
        );
        assert_eq!(audit_writer.pending(), 0, "zero skips → no rows");
        assert!(
            matches!(edge.record_minute(false), EdgeAction::None),
            "zero skips must not feed the edge"
        );

        // 3 skipped minutes: each feeds the edge (the internal Page fires
        // at the threshold) — provable via the Recover on the next ok.
        record_groww_chain_skipped_boundaries(
            &params,
            &mut edge,
            &mut audit_writer,
            &targets,
            3,
            9 * 3600 + 17 * 60,
            today,
        );
        assert!(
            matches!(
                edge.record_minute(false),
                EdgeAction::Recover { failed_minutes: 3 }
            ),
            "3 skipped minutes must have fed the edge to the paged state"
        );
        // The sink flushes best-effort at the end (the disconnected test
        // writer discards) — the edge assert above proves the rows ran.
        assert_eq!(audit_writer.pending(), 0, "flushed (best-effort discard)");

        // Midnight-crossing guard: iter_date behind today → counter + log
        // only, NO (mis-dated) forensics rows, NO edge feeding.
        let mut fresh_writer = RestFetchAuditWriter::for_test();
        let mut fresh_edge = FailureEdge::default();
        record_groww_chain_skipped_boundaries(
            &params,
            &mut fresh_edge,
            &mut fresh_writer,
            &targets,
            2,
            9 * 3600 + 17 * 60,
            today.pred_opt().expect("yesterday exists"),
        );
        assert_eq!(fresh_writer.pending(), 0, "midnight guard skips the rows");
        assert!(
            matches!(fresh_edge.record_minute(false), EdgeAction::None),
            "midnight guard must not feed the edge"
        );
    }

    /// The expiry-unresolved degrade detail is plain-English, bounded, and
    /// carries every degraded symbol + the scanned row count (pure fn).
    #[test]
    fn test_expiry_degrade_detail_names_symbols_and_row_count() {
        let detail = expiry_degrade_detail(&["NIFTY", "SENSEX"], 421);
        assert_eq!(
            detail,
            "NIFTY, SENSEX — no usable option expiry in today's contract list (421 rows scanned)"
        );
        let single = expiry_degrade_detail(&["BANKNIFTY"], 0);
        assert!(single.starts_with("BANKNIFTY — "));
        assert!(single.contains("(0 rows scanned)"));
    }
}
