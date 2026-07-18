//! Off-hours Groww RATE PROBE — the escalating-burst measurement that
//! gates promotion of the `seven_concurrent` burst tier (operator
//! approval 2026-07-14, relayed via the coordinator session: *"approved
//! and go ahead with the recommendation"*; contract:
//! `no-rest-except-live-feed-2026-06-27.md` §9.7(5)).
//!
//! ENV-GATED (`TICKVAULT_GROWW_RATE_PROBE=1`, default OFF): escalating
//! 1-second bursts of [`GROWW_RATE_PROBE_STEPS_RPS`] (4 → 6 → 8 → 11
//! requests/second) against the GRANTED candles endpoint (the §9 KEEP
//! class — NIFTY, 1-day window; no new endpoint), with a
//! [`GROWW_RATE_PROBE_PAUSE_SECS`] pause between steps and at most
//! [`GROWW_RATE_PROBE_ROUNDS`] rounds — 58 requests total, bounded by
//! construction. The 11 rps top step was sized for the PRE-jitter
//! `seven_concurrent` vendor-lag worst shape (initial 7 + a lockstep
//! first rung wave of 4 = 11 in one rolling second); with the
//! 2026-07-14 HIGH-1 per-target rung jitter that worst shape is 9 —
//! the 11 step is KEPT as deliberate over-test margin.
//!
//! REFUSED inside the [08:30, 16:00) IST wall-clock blackout window on
//! ANY day ([`GROWW_RATE_PROBE_BLACKOUT_START_SECS_OF_DAY_IST`],
//! [`GROWW_RATE_PROBE_BLACKOUT_END_SECS_OF_DAY_IST`]) so probe bursts can
//! never share the pooled Live-Data bucket with live capture (incl. the
//! 15:31–15:33 sweep/cross-verify window). SECURITY-MEDIUM fix
//! 2026-07-14: the gate deliberately takes NO trading-calendar input —
//! the pre-fix `trading_day && [09:00, 15:35)` gate could fire the
//! 58-request burst mid-session off a stale holiday list; a pure
//! wall-clock window on every day has nothing to fail-open on.
//! CO-TENANCY (MEDIUM-2, §9.7): the probe is OPERATOR-TRIGGERED
//! (env var) and shares the pooled Live-Data bucket with BruteX, whose
//! bulk pulls are nightly/post-market (§9.3) — the operator MUST
//! coordinate the probe run with BruteX's nightly window; the blackout
//! only protects OUR in-session capture, it cannot see BruteX's
//! schedule.
//!
//! OUTPUT: structured `info!` lines per step (rps, sent, ok, 429s,
//! latency min/avg/max ms) + counters
//! (`tv_groww_rate_probe_requests_total{outcome}`,
//! `tv_groww_rate_probe_rate_limited_total`). Writes NO data tables —
//! log-sink only. The token is the shared-minter SSM READ-ONLY value
//! (never minted — `groww-shared-token-minter-2026-07-02.md`); a read
//! failure aborts the probe with one warn (no partial bursts).

use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration};
use secrecy::{ExposeSecret, SecretString};
use tracing::{info, warn};

use tickvault_common::constants::{
    GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE, GROWW_HISTORICAL_CANDLES_URL,
    GROWW_RATE_PROBE_BLACKOUT_END_SECS_OF_DAY_IST, GROWW_RATE_PROBE_BLACKOUT_START_SECS_OF_DAY_IST,
    GROWW_RATE_PROBE_PAUSE_SECS, GROWW_RATE_PROBE_ROUNDS, GROWW_RATE_PROBE_STEPS_RPS,
    GROWW_SPOT_1M_REQUEST_TIMEOUT_MS, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
};
use tickvault_core::auth::secret_manager::fetch_groww_access_token;

use crate::groww_spot_1m_boot::groww_candles_query;

/// The env var that arms the probe (`=1`). Default OFF.
pub const GROWW_RATE_PROBE_ENV: &str = "TICKVAULT_GROWW_RATE_PROBE";

/// Milliseconds per second (spacing math).
const MILLIS_PER_SEC: u64 = 1_000;

/// Is the probe REFUSED right now? True inside the [08:30, 16:00) IST
/// wall-clock blackout window on ANY day — trading day or not
/// (SECURITY-MEDIUM fix 2026-07-14: no calendar input means a stale
/// holiday list can never fire the burst mid-session; the cost is that
/// weekend/holiday probes must also run outside the window — acceptable
/// for an operator-triggered off-hours tool). Pure.
#[must_use]
pub fn rate_probe_refused(secs_of_day_ist: u32) -> bool {
    (GROWW_RATE_PROBE_BLACKOUT_START_SECS_OF_DAY_IST..GROWW_RATE_PROBE_BLACKOUT_END_SECS_OF_DAY_IST)
        .contains(&secs_of_day_ist)
}

/// Intra-burst spacing (ms) for a `rps`-requests-in-one-second step:
/// request k starts k × (1000 / rps) ms into the step's second. Pure.
#[must_use]
pub fn probe_request_offset_ms(rps: u32, slot: u32) -> u64 {
    if rps == 0 {
        return 0;
    }
    u64::from(slot).saturating_mul(MILLIS_PER_SEC / u64::from(rps))
}

/// One step's measured verdict (pure formatting input).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProbeStepVerdict {
    sent: u32,
    ok: u32,
    rate_limited: u32,
    other_failures: u32,
    latency_min_ms: i64,
    latency_avg_ms: i64,
    latency_max_ms: i64,
}

/// Fold per-request (status_ok, rate_limited, latency_ms) samples into a
/// step verdict. Pure.
fn fold_step_verdict(samples: &[(bool, bool, i64)]) -> ProbeStepVerdict {
    let mut verdict = ProbeStepVerdict {
        sent: samples.len() as u32,
        latency_min_ms: i64::MAX,
        ..ProbeStepVerdict::default()
    };
    let mut latency_sum: i64 = 0;
    for &(ok, rate_limited, latency_ms) in samples {
        if ok {
            verdict.ok = verdict.ok.saturating_add(1);
        } else if rate_limited {
            verdict.rate_limited = verdict.rate_limited.saturating_add(1);
        } else {
            verdict.other_failures = verdict.other_failures.saturating_add(1);
        }
        latency_sum = latency_sum.saturating_add(latency_ms);
        verdict.latency_min_ms = verdict.latency_min_ms.min(latency_ms);
        verdict.latency_max_ms = verdict.latency_max_ms.max(latency_ms);
    }
    if samples.is_empty() {
        verdict.latency_min_ms = -1;
        verdict.latency_avg_ms = -1;
        verdict.latency_max_ms = -1;
    } else {
        verdict.latency_avg_ms = latency_sum / samples.len() as i64;
    }
    verdict
}

/// IST seconds-of-day from the wall clock (the rest_canary helper shape).
fn ist_secs_of_day_now() -> u32 {
    let now_ist = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // rem_euclid of a positive modulus is < SECONDS_PER_DAY; the cast is safe.
    now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32
}

/// IST calendar date for "now".
fn today_ist() -> chrono::NaiveDate {
    let utc = DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0).unwrap_or_default();
    (utc + ChronoDuration::seconds(i64::from(IST_UTC_OFFSET_SECONDS))).date_naive()
}

/// One probe request → (2xx?, 429?, latency_ms). Token in the
/// Authorization header only; failures are folded into the step verdict
/// (never logged raw — the step line carries only counts + latencies).
async fn probe_request_once(
    client: &reqwest::Client,
    query: &[(&'static str, String); 6],
    token: &SecretString,
) -> (bool, bool, i64) {
    let started = std::time::Instant::now();
    let result = client
        .get(GROWW_HISTORICAL_CANDLES_URL)
        .query(query)
        .bearer_auth(token.expose_secret())
        .header(GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE)
        .send()
        .await;
    let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    match result {
        Ok(resp) => {
            let status = resp.status();
            let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
            (status.is_success(), rate_limited, latency_ms)
        }
        Err(_) => (false, false, latency_ms),
    }
}

/// Run the whole escalating probe. Spawned from main.rs ONLY when the
/// [`GROWW_RATE_PROBE_ENV`] env var is `1`; re-checks the blackout window
/// before EVERY step (a probe started at 08:58 must stop at 09:00).
// TEST-EXEMPT: live-network async runner — the schedule (probe_request_offset_ms), the refusal gate (rate_probe_refused) and the verdict fold (fold_step_verdict) are the unit-tested pure parts; the spawn site is env-gated in main.rs.
pub async fn run_groww_rate_probe() {
    if rate_probe_refused(ist_secs_of_day_now()) {
        warn!(
            "groww_rate_probe: refused — inside the [08:30, 16:00) IST \
             wall-clock blackout window (ANY day — no calendar \
             dependency; run it outside the window; probe bursts must \
             never share the pooled bucket with live capture, and the \
             operator must coordinate with BruteX's nightly bulk window)"
        );
        return;
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(GROWW_SPOT_1M_REQUEST_TIMEOUT_MS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(
                ?err,
                "groww_rate_probe: HTTP client build failed — probe aborted"
            );
            return;
        }
    };
    let token = match fetch_groww_access_token().await {
        Ok(token) => token,
        Err(err) => {
            warn!(
                ?err,
                "groww_rate_probe: shared Groww access token SSM read failed \
                 — probe aborted (no partial bursts; NEVER minted)"
            );
            return;
        }
    };
    let query = groww_candles_query("NSE-NIFTY", "NSE", "CASH", today_ist());
    info!(
        steps = ?GROWW_RATE_PROBE_STEPS_RPS,
        rounds = GROWW_RATE_PROBE_ROUNDS,
        "groww_rate_probe: starting escalating off-hours burst probe \
         (the seven_concurrent promotion gate — §9.7(5))"
    );
    for round in 1..=GROWW_RATE_PROBE_ROUNDS {
        for &rps in &GROWW_RATE_PROBE_STEPS_RPS {
            // Re-check the gate before every step: a probe started just
            // before the blackout opens must stop at its edge.
            if rate_probe_refused(ist_secs_of_day_now()) {
                warn!(
                    round,
                    rps,
                    "groww_rate_probe: blackout window opened mid-probe — \
                     stopping (remaining steps skipped)"
                );
                return;
            }
            let mut set: tokio::task::JoinSet<(bool, bool, i64)> = tokio::task::JoinSet::new();
            for slot in 0..rps {
                let client = client.clone();
                let query = query.clone();
                let token = token.clone();
                let offset_ms = probe_request_offset_ms(rps, slot);
                set.spawn(async move {
                    if offset_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(offset_ms)).await;
                    }
                    probe_request_once(&client, &query, &token).await
                });
            }
            let mut samples: Vec<(bool, bool, i64)> = Vec::with_capacity(rps as usize);
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(sample) => samples.push(sample),
                    // Unwind-build only (release aborts on panic): count
                    // the lost sample as an other-failure with no latency.
                    Err(_join_err) => samples.push((false, false, -1)),
                }
            }
            let verdict = fold_step_verdict(&samples);
            metrics::counter!("tv_groww_rate_probe_requests_total", "outcome" => "ok")
                .increment(u64::from(verdict.ok));
            metrics::counter!("tv_groww_rate_probe_requests_total", "outcome" => "rate_limited")
                .increment(u64::from(verdict.rate_limited));
            metrics::counter!("tv_groww_rate_probe_requests_total", "outcome" => "error")
                .increment(u64::from(verdict.other_failures));
            metrics::counter!("tv_groww_rate_probe_rate_limited_total")
                .increment(u64::from(verdict.rate_limited));
            info!(
                round,
                rps,
                sent = verdict.sent,
                ok = verdict.ok,
                rate_limited = verdict.rate_limited,
                other_failures = verdict.other_failures,
                latency_min_ms = verdict.latency_min_ms,
                latency_avg_ms = verdict.latency_avg_ms,
                latency_max_ms = verdict.latency_max_ms,
                "groww_rate_probe: step verdict (429s here = the burst \
                 ceiling — the seven_concurrent promotion evidence)"
            );
            tokio::time::sleep(Duration::from_secs(GROWW_RATE_PROBE_PAUSE_SECS)).await;
        }
    }
    info!("groww_rate_probe: complete — read the step verdict lines above");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blackout gate (SECURITY-MEDIUM 2026-07-14): [08:30, 16:00)
    /// IST refused on ANY day — no calendar input, so a stale holiday
    /// list can never open the gate mid-session; window edges honored
    /// (half-open).
    #[test]
    fn test_rate_probe_blackout_window() {
        // Inside the window → refused, session or not.
        assert!(rate_probe_refused(8 * 3600 + 30 * 60));
        assert!(rate_probe_refused(9 * 3600));
        assert!(rate_probe_refused(12 * 3600));
        assert!(rate_probe_refused(15 * 3600 + 34 * 60));
        assert!(rate_probe_refused(15 * 3600 + 59 * 60 + 59));
        // Window edges: end is exclusive, pre-window allowed.
        assert!(!rate_probe_refused(16 * 3600));
        assert!(!rate_probe_refused(8 * 3600 + 29 * 60));
        assert!(!rate_probe_refused(23 * 3600));
        assert!(!rate_probe_refused(0));
    }

    /// The step plan is bounded by construction: 4/6/8/11 × 2 rounds = 58
    /// requests, and intra-step spacing spreads a step across its second.
    #[test]
    fn test_probe_step_plan_bounds() {
        let per_round: u32 = GROWW_RATE_PROBE_STEPS_RPS.iter().sum();
        assert_eq!(per_round * GROWW_RATE_PROBE_ROUNDS, 58);
        // Spacing: slot k of an rps-step starts k*(1000/rps) ms in — the
        // LAST slot still starts inside the second.
        for &rps in &GROWW_RATE_PROBE_STEPS_RPS {
            assert_eq!(probe_request_offset_ms(rps, 0), 0);
            assert!(probe_request_offset_ms(rps, rps - 1) < 1_000);
        }
        // Degenerate rps=0 never divides by zero.
        assert_eq!(probe_request_offset_ms(0, 5), 0);
    }

    /// The verdict fold: counts route by (ok, 429, other) and latency
    /// min/avg/max come from the samples; empty input yields -1 sentinels.
    #[test]
    fn test_fold_step_verdict_counts_and_latency() {
        let verdict =
            fold_step_verdict(&[(true, false, 100), (false, true, 300), (false, false, 200)]);
        assert_eq!(verdict.sent, 3);
        assert_eq!(verdict.ok, 1);
        assert_eq!(verdict.rate_limited, 1);
        assert_eq!(verdict.other_failures, 1);
        assert_eq!(verdict.latency_min_ms, 100);
        assert_eq!(verdict.latency_avg_ms, 200);
        assert_eq!(verdict.latency_max_ms, 300);

        let empty = fold_step_verdict(&[]);
        assert_eq!(empty.sent, 0);
        assert_eq!(empty.latency_min_ms, -1);
        assert_eq!(empty.latency_avg_ms, -1);
        assert_eq!(empty.latency_max_ms, -1);
    }
}
