//! REST-health canary (operator task DHAN-REST-400, 2026-06-10).
//!
//! On 2026-06-10 every `api.dhan.co` REST call returned HTTP 400 from 08:45
//! IST, but the operator only discovered it at 15:33 when the post-market
//! cross-verify failed 776/776. Operator directive: *"one cheap GET
//! /v2/profile at 09:05 + 12:00 + 15:25 IST; on non-200, page HIGH
//! immediately with the captured body."*
//!
//! Three probes bracket the trading session (just after open, midday, just
//! before close). Each failure pages `Severity::High` via
//! `error!(code = "REST-CANARY-01")` carrying the HTTP status, the EXACT
//! final request URL (token-redacted) and a bounded (≤300 chars)
//! secret-redacted response body — Dhan's `errorType`/`errorCode`/
//! `errorMessage` name the cause on the spot.
//!
//! A send-leg (transport) failure retries ONCE after 30s before paging —
//! mirrors the mid-session watchdog's 2026-04-26 transient-network lesson
//! (a laptop DNS blip must not page HIGH).
//!
//! The task runs the probes remaining for TODAY and then exits — the AWS
//! instance stops at 16:30 IST and cold-boots fresh each trading morning,
//! so a single-day pass is the correct lifetime. Non-trading days skip
//! silently (audit Rule 3).

use std::time::Duration;

use secrecy::ExposeSecret;
use tracing::{error, info, warn};

use tickvault_common::constants::DHAN_USER_PROFILE_PATH;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::{capture_rest_error_body, redact_url_params};
use tickvault_common::url_join::join_api_url;
use tickvault_core::auth::token_manager::TokenHandle;

/// Probe times, IST seconds-of-day: 09:05:00, 12:00:00, 15:25:00.
/// 09:05 = open + 5 min (catches an 08:45-class REST death by 09:05, not
/// 15:33); 15:25 = 5 min before close (last chance to act in-session).
pub const CANARY_PROBE_SECS_OF_DAY_IST: [u32; 3] =
    [9 * 3600 + 5 * 60, 12 * 3600, 15 * 3600 + 25 * 60];

/// Per-probe HTTP timeout.
const PROBE_TIMEOUT_SECS: u64 = 10;

/// Delay before the single send-leg retry (transient-network damping).
const SEND_RETRY_DELAY_SECS: u64 = 30;

/// A probe woken more than this many seconds past its slot is SKIPPED
/// (adversarial review 2026-06-10 MEDIUM-2: a laptop suspend / wall-clock
/// step can fire tokio's monotonic sleep long after the slot — probing
/// post-close or next-day would false-page).
const PROBE_STALE_GRACE_SECS: u32 = 1800;

/// Next probe at-or-after `now_secs_of_day`, as `(probe_index, sleep_secs)`.
/// `None` when all of today's probes are already past (e.g. a post-15:25
/// boot). Pure.
#[must_use]
pub fn next_probe_sleep_secs(now_secs_of_day: u32) -> Option<(usize, u64)> {
    CANARY_PROBE_SECS_OF_DAY_IST
        .iter()
        .enumerate()
        .find(|(_, probe)| **probe >= now_secs_of_day)
        .map(|(idx, probe)| (idx, u64::from(probe - now_secs_of_day)))
}

/// Probe verdict. Pure classification of the HTTP status code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanaryVerdict {
    /// 2xx — the Dhan REST surface answered.
    Pass,
    /// Anything else — page HIGH with the captured body.
    Fail,
}

/// Operator demand is "on non-200, page" — implemented as non-2xx so a 201/
/// 204 (should Dhan ever return one) does not false-page. Pure.
#[must_use]
pub fn classify_canary_status(status: u16) -> CanaryVerdict {
    if (200..=299).contains(&status) {
        CanaryVerdict::Pass
    } else {
        CanaryVerdict::Fail
    }
}

/// Run today's remaining probes. Cold path; one cheap GET per probe.
///
/// `is_trading_day` is resolved by the caller (boot wiring holds the
/// `TradingCalendar`); `now_secs_of_day_ist` seeds the schedule walk.
// TEST-EXEMPT: live-deps async runner — schedule + classification pure fns unit-tested below; the HTTP leg mirrors the tested token-manager pattern.
pub async fn run_rest_canary(
    token_handle: TokenHandle,
    rest_api_base_url: String,
    is_trading_day: bool,
    now_secs_of_day_ist: u32,
) {
    if !is_trading_day {
        info!("rest_canary: non-trading day — skipping all probes");
        return;
    }
    let Some((first_idx, _)) = next_probe_sleep_secs(now_secs_of_day_ist) else {
        info!("rest_canary: booted past 15:25 IST — no probes remain today");
        return;
    };
    let url = join_api_url(&rest_api_base_url, DHAN_USER_PROFILE_PATH);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(
                ?err,
                "rest_canary: HTTP client build failed — canary disabled today"
            );
            return;
        }
    };

    for (idx, probe_secs) in CANARY_PROBE_SECS_OF_DAY_IST.iter().enumerate() {
        if idx < first_idx {
            continue;
        }
        // Re-read the wall clock before each probe so drift across the
        // multi-hour sleeps cannot accumulate.
        let now = ist_secs_of_day_now();
        if *probe_secs > now {
            tokio::time::sleep(Duration::from_secs(u64::from(*probe_secs - now))).await;
        }
        // Staleness gate: after the sleep, re-check the wall clock — a
        // suspend/clock-step can wake us far past the slot (or on the next
        // day, where the spawn-time trading-day verdict is stale). Skip
        // rather than false-page.
        let woke_at = ist_secs_of_day_now();
        if !probe_is_fresh(*probe_secs, woke_at) {
            warn!(
                probe = idx,
                scheduled_secs = *probe_secs,
                woke_at_secs = woke_at,
                "rest_canary: woke too far past the probe slot (suspend/clock step?) — skipping"
            );
            continue;
        }
        probe_once(&client, &url, &token_handle, idx).await;
    }
    info!("rest_canary: all of today's probes complete");
}

/// `true` when a wake at `woke_at_secs_of_day` is within the freshness
/// grace of its scheduled slot. Handles the midnight-wrap case (waking on a
/// LATER day yields a small seconds-of-day — treated as stale because it is
/// below the scheduled slot). Pure.
#[must_use]
pub fn probe_is_fresh(scheduled_secs_of_day: u32, woke_at_secs_of_day: u32) -> bool {
    woke_at_secs_of_day >= scheduled_secs_of_day
        && woke_at_secs_of_day - scheduled_secs_of_day <= PROBE_STALE_GRACE_SECS
}

/// IST seconds-of-day from the wall clock.
fn ist_secs_of_day_now() -> u32 {
    use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
    let now_ist = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // rem_euclid of a positive modulus is < SECONDS_PER_DAY, so the cast is safe.
    now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32
}

/// One probe: GET /v2/profile with the live token. Send-leg failures retry
/// once after [`SEND_RETRY_DELAY_SECS`]; non-2xx pages immediately.
async fn probe_once(client: &reqwest::Client, url: &str, token_handle: &TokenHandle, idx: usize) {
    // Zeroize-on-drop per the token_manager Audit-2026-05-03 H1 pattern —
    // the JWT copy is wiped from the heap when this function returns
    // (`SecretString` zeroizes; no plain `String` copy is retained).
    let jwt: secrecy::SecretString = {
        let guard = token_handle.load();
        match guard.as_ref() {
            Some(state) => state.access_token().clone(),
            None => {
                // REST cannot be healthy without a token — page.
                page_failure(
                    idx,
                    url,
                    "no_token",
                    "no access token available at probe time",
                );
                return;
            }
        }
    };
    let mut last_send_error: Option<String> = None;
    for attempt in 0..2u8 {
        if attempt == 1 {
            tokio::time::sleep(Duration::from_secs(SEND_RETRY_DELAY_SECS)).await;
        }
        match client
            .get(url)
            .header("access-token", jwt.expose_secret())
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                match classify_canary_status(status.as_u16()) {
                    CanaryVerdict::Pass => {
                        metrics::counter!("tv_rest_canary_probes_total", "outcome" => "pass")
                            .increment(1);
                        info!(probe = idx, %status, "rest_canary: probe PASSED — Dhan REST healthy");
                    }
                    CanaryVerdict::Fail => {
                        let body = resp.text().await.unwrap_or_default();
                        page_failure(idx, url, status.as_str(), &capture_rest_error_body(&body));
                    }
                }
                return;
            }
            Err(err) => {
                let reason = redact_url_params(&err.to_string());
                warn!(probe = idx, attempt, %reason, "rest_canary: send failed — will retry once");
                last_send_error = Some(reason);
            }
        }
    }
    page_failure(
        idx,
        url,
        "send_failed",
        last_send_error.as_deref().unwrap_or("unknown send error"),
    );
}

/// The HIGH page: `error!` with code REST-CANARY-01 routes to Telegram via
/// the 5-sink pipeline. Carries status + final URL (token-redacted) + body.
fn page_failure(idx: usize, url: &str, status: &str, body: &str) {
    metrics::counter!("tv_rest_canary_probes_total", "outcome" => "fail").increment(1);
    error!(
        code = ErrorCode::RestCanary01ProbeFailed.code_str(),
        severity = ErrorCode::RestCanary01ProbeFailed.severity().as_str(),
        probe = idx,
        status,
        url = %redact_url_params(url),
        body,
        "REST-CANARY-01: Dhan REST health probe FAILED — REST surface is down or rejecting"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RATCHET (operator directive): probe times pinned to 09:05 / 12:00 /
    /// 15:25 IST.
    #[test]
    fn test_canary_schedule_times_pinned() {
        assert_eq!(CANARY_PROBE_SECS_OF_DAY_IST, [32_700, 43_200, 55_500]);
        // Strictly increasing (the runner walks them in order).
        assert!(CANARY_PROBE_SECS_OF_DAY_IST.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn test_next_probe_sleep_secs_before_all_selects_0905() {
        // 08:30 boot → first probe 09:05, sleep 35 min.
        let (idx, sleep) = next_probe_sleep_secs(8 * 3600 + 30 * 60).expect("probe");
        assert_eq!(idx, 0);
        assert_eq!(sleep, 35 * 60);
    }

    #[test]
    fn test_next_probe_sleep_secs_between_selects_next() {
        // 10:00 → 12:00 probe.
        let (idx, sleep) = next_probe_sleep_secs(10 * 3600).expect("probe");
        assert_eq!(idx, 1);
        assert_eq!(sleep, 2 * 3600);
        // 14:00 → 15:25 probe.
        let (idx, sleep) = next_probe_sleep_secs(14 * 3600).expect("probe");
        assert_eq!(idx, 2);
        assert_eq!(sleep, 3600 + 25 * 60);
    }

    #[test]
    fn test_next_probe_sleep_secs_exactly_at_probe_time_fires_now() {
        let (idx, sleep) = next_probe_sleep_secs(CANARY_PROBE_SECS_OF_DAY_IST[1]).expect("probe");
        assert_eq!(idx, 1);
        assert_eq!(sleep, 0);
    }

    #[test]
    fn test_next_probe_sleep_secs_after_all_returns_none() {
        // 15:25:01 → nothing left today.
        assert_eq!(next_probe_sleep_secs(15 * 3600 + 25 * 60 + 1), None);
        assert_eq!(next_probe_sleep_secs(20 * 3600), None);
    }

    #[test]
    fn test_probe_is_fresh_within_grace() {
        let slot = CANARY_PROBE_SECS_OF_DAY_IST[0];
        assert!(probe_is_fresh(slot, slot));
        assert!(probe_is_fresh(slot, slot + 60));
        assert!(probe_is_fresh(slot, slot + PROBE_STALE_GRACE_SECS));
    }

    /// RATCHET (adversarial review MEDIUM-2): a suspend/clock-step wake far
    /// past the slot — or a midnight wrap onto the next day — must SKIP,
    /// never probe with a stale trading-day verdict.
    #[test]
    fn test_probe_is_fresh_rejects_stale_and_midnight_wrap() {
        let slot = CANARY_PROBE_SECS_OF_DAY_IST[2]; // 15:25
        assert!(!probe_is_fresh(slot, slot + PROBE_STALE_GRACE_SECS + 1));
        // Next-day wake: seconds-of-day wrapped below the slot.
        assert!(!probe_is_fresh(slot, 9 * 3600));
        assert!(!probe_is_fresh(slot, 0));
    }

    #[test]
    fn test_classify_canary_status_2xx_passes() {
        assert_eq!(classify_canary_status(200), CanaryVerdict::Pass);
        assert_eq!(classify_canary_status(204), CanaryVerdict::Pass);
        assert_eq!(classify_canary_status(299), CanaryVerdict::Pass);
    }

    #[test]
    fn test_classify_canary_status_non_2xx_fails() {
        // 400 is the 2026-06-10 incident shape.
        for status in [400, 401, 403, 404, 429, 500, 502, 503, 199, 301] {
            assert_eq!(
                classify_canary_status(status),
                CanaryVerdict::Fail,
                "status {status} must fail"
            );
        }
    }
}
