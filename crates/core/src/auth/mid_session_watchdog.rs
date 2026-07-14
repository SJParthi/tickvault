//! Mid-session profile watchdog.
//!
//! # Why this exists (queue item I7, 2026-04-21)
//!
//! On 2026-04-21 the app booted during market hours, WebSocket reported
//! all 5 connections "connected", but Dhan was not actually streaming
//! data — likely a `dataPlan` / `activeSegment` / token-validity issue
//! that appeared DURING the session. PR #309 added a pre-market profile
//! HALT at boot, but once the app is already running the invariant goes
//! unchecked for the rest of the day.
//!
//! This watchdog closes that gap. Every [`MID_SESSION_CHECK_INTERVAL_SECS`]
//! during market hours, it calls [`TokenManager::pre_market_check`]
//! (same validation the boot-time HALT uses) and — since AUTH-GAP-05 —
//! self-heals a sustained REAL auth failure with a forced re-mint.
//!
//! # SILENT since 2026-07-14 (operator Dhan noise lock)
//!
//! Per `dhan-rest-only-noise-lock-2026-07-14.md`: the probe + the
//! AUTH-GAP-05 forced re-mint run SILENTLY (coded `error!` + counters
//! only — the `MidSessionProfileInvalidated` Critical and
//! `TokenForcedRemintTriggered` High Telegram pages are DELETED). The
//! ONLY Telegram from this module is the family-(3) token-unobtainable
//! Critical (the `AuthenticationFailed` event — the full constructor
//! path is deliberately NOT spelled here so the silence ratchet's
//! call-form needle can never be satisfied by this doc comment), fired
//! AT MOST ONCE PER FAILING EPISODE (H1, 2026-07-14 fix round) when
//! EITHER (a) a forced re-mint fails TERMINALLY — the token could not
//! be obtained at all — OR (b) [`REMINT_MAX_ATTEMPTS_PER_EPISODE`]
//! re-mints all "succeeded" yet the profile stayed REAL-invalid (the
//! dead-dataPlan class: the broker accepts the login but the account
//! entitlement is broken). Everything else self-heals: the GAP-04
//! latch re-arm retries the re-mint every
//! [`REMINT_REARM_FAILING_CYCLES`] failing cycles (~30 min) while the
//! episode persists, STOPPING at the per-episode attempt cap.
//!
//! # Design
//!
//! - **Background tokio task** spawned at boot, runs forever.
//! - **Market-hours gated** (Rule 3 — `tickvault_common::market_hours`).
//! - **Edge-triggered** (Rule 4 — `currently_failing: bool`). Rising
//!   edge fires one coded `error!` (no Telegram — 2026-07-14 noise
//!   lock). Falling edge fires INFO (recovery).
//! - **Interval**: 15 minutes. Tight enough to catch a mid-session
//!   `dataPlan` invalidation within one check window; loose enough to
//!   not pound Dhan's `/v2/profile` endpoint (1 req/sec Quote rate
//!   limit shared).
//! - **Cold path**: one HTTP round-trip every 15 minutes. Zero impact
//!   on the tick hot path.
//!
//! # Not a HALT
//!
//! The pre-market HALT (PR #309) is intentional — booting into a known-
//! bad state is a no-op that costs nothing. A MID-SESSION HALT would
//! cost the retained Dhan REST legs (spot-1m / option-chain) for
//! nothing. Remediation is automatic (AUTH-GAP-05 + GAP-04 + the
//! renewal loop); only a token that cannot be obtained at all pages.
//!
//! # Tests
//!
//! - `evaluate_transition_first_failure` — rising edge
//! - `evaluate_transition_recovery_after_failure` — falling edge
//! - `evaluate_transition_steady_fail_no_duplicate_alert` — idempotent
//! - `evaluate_transition_steady_ok_no_op` — idempotent

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tickvault_common::constants::DHAN_TOKEN_GENERATION_COOLDOWN_SECS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::market_hours::is_within_market_hours_ist;
use tracing::{error, info, warn};

// AG5-R2-1 (2026-07-06): the classification wrappers are SHARED constants
// owned by the PRODUCER (`token_manager.rs` interpolates them at its
// `format!` sites) and imported here — a reword in token_manager.rs can no
// longer silently drift the classifier back to the fail-safe RealAuthFail
// default (which walks TOWARD a destructive mint during a pure REST-surface
// outage — the AG5-R1-3 CRITICAL this file fixed). Local aliases keep the
// pre-existing names used throughout this module.
use crate::auth::token_manager::{
    MINT_COOLDOWN_REFUSAL_REASON_PREFIX, PROFILE_HTTP_RESPONSE_WRAPPER as HTTP_RESPONSE_WRAPPER,
    PROFILE_PARSE_ERROR_WRAPPER as PARSE_ERROR_WRAPPER,
    PROFILE_SEND_LEG_WRAPPER as SEND_LEG_WRAPPER, RESILIENCE03_MINT_REFUSAL_REASON_PREFIX,
    TokenManager,
};
use crate::notification::events::NotificationEvent;
use crate::notification::service::NotificationService;
use tickvault_common::sanitize::capture_rest_error_body;

/// Check cadence (seconds). 15 minutes = 900 s. Loose enough to stay
/// under the 1 req/sec Quote API rate limit even under pathological
/// burst conditions; tight enough that a mid-session `dataPlan`
/// invalidation is detected within one cycle.
pub const MID_SESSION_CHECK_INTERVAL_SECS: u64 = 900;

/// AUTH-GAP-05 (2026-07-06): two consecutive REAL `/v2/profile` auth
/// failures (~2 × 900s ≈ 30 min of a Dhan-rejected token) before forcing
/// a re-mint. One failure could be a Dhan-side blip — minting on a blip
/// risks the ~125s cooldown ping-pong the operator lock forbids.
pub const CONSECUTIVE_INVALID_REMINT_THRESHOLD: u32 = 2;

/// GAP-04 (2026-07-14, Dhan noise lock): while a failing episode
/// PERSISTS past the first forced re-mint, the retry-once latch is
/// re-armed every this-many additional REAL failing cycles (2 × 900s ≈
/// 30 min retry cadence) — so a token that stays dead re-mints roughly
/// every half hour instead of exactly once per episode. The ~125s Dhan
/// mint cooldown + the RESILIENCE-03 lock refusal still gate every
/// attempt; a clean `Ok` cycle resets everything. SILENT by design —
/// no Telegram from this path (`dhan-rest-only-noise-lock-2026-07-14.md`).
pub const REMINT_REARM_FAILING_CYCLES: u32 = 2;

/// GAP-04. Pure. True iff the consecutive-REAL-failure counter sits on a
/// re-arm boundary STRICTLY past the first-trigger threshold — i.e.
/// `n > threshold` and `(n - threshold)` is a positive multiple of
/// [`REMINT_REARM_FAILING_CYCLES`]. With threshold 2 and cadence 2 the
/// re-mints fire at consecutive counts 2 (the normal trigger), 4, 6, …
#[must_use]
pub fn should_rearm_remint_latch(consecutive_invalid: u32) -> bool {
    consecutive_invalid > CONSECUTIVE_INVALID_REMINT_THRESHOLD
        && (consecutive_invalid - CONSECUTIVE_INVALID_REMINT_THRESHOLD)
            .is_multiple_of(REMINT_REARM_FAILING_CYCLES)
}

/// H1b (2026-07-14 fix round): hard cap on forced re-mints per failing
/// episode. After this many mints in ONE episode the GAP-04 re-arm STOPS
/// (no further mint until a clean `Ok` cycle clears the episode) and the
/// once-per-episode family-(3) Critical fires if the profile is STILL
/// REAL-invalid — closing the silent dead-dataPlan loop where every
/// re-mint "succeeds" (the broker accepts the login) yet the profile
/// stays invalid: previously ~48 silent mints/day with zero pages.
pub const REMINT_MAX_ATTEMPTS_PER_EPISODE: u32 = 3;

/// H1b. Pure. True iff the per-episode mint budget is spent.
#[must_use]
pub fn remint_cap_reached(attempts_this_episode: u32) -> bool {
    attempts_this_episode >= REMINT_MAX_ATTEMPTS_PER_EPISODE
}

/// Spawns the mid-session profile watchdog as an independent tokio task.
///
/// Consumers should keep the returned [`tokio::task::JoinHandle`] around
/// and abort it on graceful shutdown. The task runs forever otherwise.
///
/// # Arguments
/// * `token_manager` — shared `TokenManager` (uses `pre_market_check`;
///   AUTH-GAP-05 additionally uses `dual_instance_lock_held` +
///   `force_renewal` for the once-per-episode forced re-mint).
/// * `notifier` — `NotificationService` for the family-(3)
///   token-unobtainable Critical on a TERMINAL forced-re-mint failure
///   (the ONLY Telegram this module may send — 2026-07-14 noise lock).
///   `None` disables Telegram; logs still fire.
/// * `profile_valid` — shared flag written every in-session cycle
///   (`true` on a clean check, `false` on a REAL auth failure) and read
///   by the token-health gauge poller for the `tv_token_valid` composite.
// TEST-EXEMPT: thin tokio::spawn wrapper around `run_watchdog_loop`. The behavioural surface (`evaluate_transition` + `apply_transition` + `classify_cycle` + `report_cycle` + `apply_cycle_outcome` + `decide_remint` + `apply_remint_decision`) is fully covered by the unit tests in this module; the spawn wrapper itself has no decision-bearing state mutation (COV-R6-1: the retry-once episode latch + cooldown stamp moved into the pure `apply_remint_decision`).
pub fn spawn_mid_session_profile_watchdog(
    token_manager: Arc<TokenManager>,
    notifier: Option<Arc<NotificationService>>,
    profile_valid: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_watchdog_loop(token_manager, notifier, profile_valid))
}

/// Internal loop. Extracted so the unit tests below can drive it via
/// `evaluate_transition` + `apply_transition` + `apply_cycle_outcome` +
/// `decide_remint` + `apply_remint_decision` without spawning a task.
async fn run_watchdog_loop(
    token_manager: Arc<TokenManager>,
    notifier: Option<Arc<NotificationService>>,
    profile_valid: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(MID_SESSION_CHECK_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick — we want to wait one full
    // interval before the first check so this doesn't fire at boot
    // (the pre-market HALT already covers boot-time).
    interval.tick().await;
    let mut state = WatchdogState::default();
    // AUTH-GAP-05: loop-local last re-mint instant — feeds the pure
    // `cooldown_elapsed` input of `decide_remint` (~125s Dhan mint
    // cooldown; structurally moot at the 900s cadence but hard-stopped
    // here anyway). AG5-R2-1 HONEST SCOPE: this gates ONLY the watchdog's
    // own mints — `acquire_token` has NO mint-cooldown gate (the 125s
    // cooldown in token_manager lives solely in the boot-time `initialize`
    // TOTP retry loop), so an uncoordinated concurrent caller (the ~23h
    // renewal loop's failure retries, the AUTH-GAP-03 ws-wake force
    // renewal, and — SEC-R4-1 — the lane-owned 4h token sweep's
    // `force_renewal_if_stale`) can still mint inside Dhan's ~125s window
    // during the same dead-token episode. Bounded + self-correcting (Dhan
    // rejects / rate-limits the second mint; the loops keep retrying) —
    // moving the cooldown stamp into TokenManager so ALL
    // renew_with_fallback callers share one gate is a flagged follow-up,
    // NOT claimed here.
    let mut last_remint_at: Option<Instant> = None;
    loop {
        interval.tick().await;
        if !is_within_market_hours_ist() {
            // Off-hours: skip the check entirely. Do NOT touch state —
            // if we were alerting at 15:30 market close, the next
            // market open (09:15 tomorrow) should fire fresh if the
            // condition persists.
            continue;
        }
        let check_result = token_manager.pre_market_check().await;
        // R6-CPLX-1 (2026-07-07): classify ONCE per cycle — `classify_cycle`
        // renders the error Display once and runs the 4-way classifier once;
        // `report_cycle` derives the rising-edge boolean (+ the transient /
        // REST-degraded warn!/counter side effects) from that single verdict
        // instead of re-classifying the same check result.
        let (outcome, rendered_reason) = classify_cycle(&check_result);
        let (is_real_auth_failing, reason) = report_cycle(outcome, rendered_reason);

        // AUTH-GAP-05: counter + episode latch + shared profile-truth flag.
        apply_cycle_outcome(&mut state, outcome, &profile_valid);

        // Rising/falling edge bookkeeping — SILENT since 2026-07-14 (the
        // MidSessionProfileInvalidated Critical page is deleted per the
        // Dhan noise lock; the coded error! + counter remain). M1 (fix
        // round): the outcome rides along so the rising-edge wording is
        // class-accurate (a REST-surface degrade never engages a re-mint).
        let transition = evaluate_transition(&state, is_real_auth_failing);
        apply_transition(transition, &mut state, reason, outcome);

        // H1b (2026-07-14 fix round): the attempt-cap page — the episode
        // spent all REMINT_MAX_ATTEMPTS_PER_EPISODE re-mints (each
        // "succeeded" or failed non-terminally) and the profile is STILL
        // REAL-invalid on this cycle. This is the dead-dataPlan /
        // dead-segment class: the broker accepts the login but the
        // account entitlement stays broken, so more re-logins cannot
        // help. ONE family-(3) Critical per episode (shared latch with
        // the terminal-failure arm below).
        if outcome == CycleOutcome::RealAuthFail
            && remint_cap_exhausted(&state)
            && take_terminal_page(&mut state)
        {
            error!(
                code = ErrorCode::AuthGap05ForcedRemintTriggered.code_str(),
                attempts = state.remint_attempts_this_episode,
                consecutive = state.consecutive_invalid,
                "AUTH-GAP-05 re-mint attempt cap reached with the profile STILL invalid — \
                 dataPlan/segment class; paging once per episode, no further re-mints \
                 until the episode clears"
            );
            metrics::counter!("tv_token_forced_remint_total", "outcome" => "cap_exhausted")
                .increment(1);
            if let Some(n) = notifier.as_ref() {
                n.notify(NotificationEvent::AuthenticationFailed {
                    reason: format!(
                        "the Dhan login profile stayed INVALID after {} automatic \
                         re-logins (~30 minutes apart) — the broker accepts the login \
                         but the account check keeps failing (data plan / segment \
                         class). The Dhan spot-1m + option-chain pulls are blocked \
                         until this is fixed on the Dhan portal",
                        state.remint_attempts_this_episode
                    ),
                });
            }
        }

        // AUTH-GAP-05: forced re-mint decision (pure) + action (bounded —
        // once per failing episode, NEVER while the dual-instance lock is
        // lost, NEVER inside the ~125s Dhan mint cooldown).
        let cooldown_elapsed = last_remint_at
            .is_none_or(|at| at.elapsed().as_secs() >= DHAN_TOKEN_GENERATION_COOLDOWN_SECS);
        let decision = decide_remint(
            state.consecutive_invalid,
            state.remint_attempted_this_episode,
            cooldown_elapsed,
            token_manager.dual_instance_lock_held(),
        );
        // COV-R6-1 (2026-07-07): the retry-once episode latch + cooldown
        // stamp are applied through the pure, unit-tested
        // `apply_remint_decision` — never as loose assignments inside this
        // TEST-EXEMPT loop (deleting the latch would previously have left
        // every unit test green while the watchdog re-minted / re-paged
        // RESILIENCE-01 every 900s cycle).
        apply_remint_decision(&mut state, &mut last_remint_at, decision);
        match decision {
            RemintDecision::Trigger => {
                // Retry-once + edge latch (applied above via
                // `apply_remint_decision`; the GAP-04 re-arm retries a
                // persisting episode every ~30 min): SILENT self-heal —
                // coded error! + counters only, no Telegram
                // (dhan-rest-only-noise-lock-2026-07-14.md).
                error!(
                    code = ErrorCode::AuthGap05ForcedRemintTriggered.code_str(),
                    consecutive = state.consecutive_invalid,
                    "AUTH-GAP-05 sustained token-invalid — forcing re-mint via existing renewal machinery"
                );
                metrics::counter!("tv_token_forced_remint_total", "outcome" => "triggered")
                    .increment(1);
                match token_manager.force_renewal().await {
                    Ok(()) => {
                        info!(
                            code = ErrorCode::AuthGap05ForcedRemintTriggered.code_str(),
                            "AUTH-GAP-05 forced re-mint issued — next cycle re-verifies the profile"
                        );
                        metrics::counter!("tv_token_forced_remint_total", "outcome" => "ok")
                            .increment(1);
                    }
                    Err(e) => {
                        // RESILIENCE-03 refusal inside force_renewal (the
                        // stale-true-flag race) is PERMANENT — no retry
                        // helps; other failures are covered by the 23h
                        // renewal-loop circuit breaker + the standing
                        // CRITICAL page. NO local retry loop either way.
                        //
                        // SEC-R2-2: PREFIX-anchored on OUR shared refusal
                        // literal, never a substring scan of the rendered
                        // error — the mint-HTTP-failure shape concatenates
                        // a server-controlled `body=` that could forge
                        // "RESILIENCE-03" and misdirect triage toward a
                        // peer-ownership investigation that does not exist.
                        // The in-flight refusal also gets its OWN outcome
                        // label (`refused_lock_lost_inflight`) so the
                        // counter series never conflates it with the
                        // pre-mint gate refusal below.
                        let rendered = format!("{e}");
                        let permanent = is_inflight_lock_refusal(&rendered);
                        // H3 (2026-07-14 fix round): a TokenManager-level
                        // mint-cooldown skip (another caller minted < ~125s
                        // ago) is NOT a terminal failure — the next re-arm
                        // window retries after the cooldown; it must never
                        // page or burn the episode's single page.
                        let cooldown_skip = is_mint_cooldown_refusal(&rendered);
                        error!(
                            code = ErrorCode::AuthGap05ForcedRemintTriggered.code_str(),
                            permanent,
                            cooldown_skip,
                            error = %e,
                            "AUTH-GAP-05 forced re-mint failed — no local retry (episode latch holds; GAP-04 re-arms in ~30 min while the attempt cap lasts)"
                        );
                        metrics::counter!(
                            "tv_token_forced_remint_total",
                            "outcome" => if permanent {
                                "refused_lock_lost_inflight"
                            } else if cooldown_skip {
                                "cooldown_skipped"
                            } else {
                                "failed"
                            }
                        )
                        .increment(1);
                        // 2026-07-14 Dhan noise lock, family-(3): a forced
                        // re-mint that FAILED means the token could not be
                        // obtained — the ONE Dhan token condition that still
                        // pages, AT MOST ONCE PER EPISODE (H1a latch — the
                        // GAP-04 re-arm previously re-paged the same episode
                        // every ~30 min). The RESILIENCE-03 in-flight lock
                        // refusal is deliberately NOT paged FROM HERE (a peer
                        // owns the session; its token is alive — not
                        // "unobtainable") — though note `acquire_token`'s own
                        // tripwire already paged its family-(3) refusal
                        // before returning this error (pre-existing
                        // token_manager behavior; see the L-fix comment on
                        // the RefuseLockLost arm below). M2: the rendered
                        // error is redacted + truncated (≤300 chars) before
                        // it reaches the Telegram body — the mint-HTTP shape
                        // embeds a raw server-controlled `body=`.
                        if !permanent
                            && !cooldown_skip
                            && take_terminal_page(&mut state)
                            && let Some(n) = notifier.as_ref()
                        {
                            let sanitized = capture_rest_error_body(&rendered);
                            n.notify(NotificationEvent::AuthenticationFailed {
                                reason: format!(
                                    "the automatic Dhan token re-mint failed after ~30 minutes \
                                     of the broker rejecting our login ({sanitized}) — the Dhan \
                                     spot-1m + option-chain pulls are blocked until the token \
                                     is fixed"
                                ),
                            });
                        }
                    }
                }
            }
            RemintDecision::RefuseLockLost => {
                // Latched above via `apply_remint_decision` so this refusal
                // is not re-evaluated every cycle; a clean Ok cycle re-arms
                // — and the GAP-04 re-arm means a PERSISTING lock-lost
                // episode re-logs this error roughly every ~30 minutes
                // (bounded log cadence, no Telegram FROM THIS ARM; noted in
                // the wave-4-error-codes.md AUTH-GAP-05 banner). Truth note
                // (L-fix, 2026-07-14 fix round): this PRE-mint gate never
                // calls force_renewal, so nothing pages here — but the
                // IN-FLIGHT RESILIENCE-03 tripwire inside `acquire_token`
                // DOES page its own family-(3) AuthenticationFailed when a
                // mint that already started loses the lock (pre-existing
                // token_manager behavior, unchanged by the noise lock).
                // NEVER calls force_renewal — zero external side effects (a
                // peer owns the session and its active token must never be
                // destroyed).
                error!(
                    code = ErrorCode::Resilience01DualInstanceDetected.code_str(),
                    "AUTH-GAP-05 re-mint REFUSED — dual-instance lock not held; a peer owns the Dhan session (fail-closed, zero external side effects)"
                );
                metrics::counter!("tv_token_forced_remint_total", "outcome" => "refused_lock_lost")
                    .increment(1);
            }
            RemintDecision::HoldCooldown => {
                metrics::counter!("tv_token_forced_remint_total", "outcome" => "cooldown")
                    .increment(1);
            }
            RemintDecision::AlreadyAttempted | RemintDecision::Wait => {}
        }
    }
}

/// Classify a `pre_market_check` result into "real auth failure" vs
/// "transient network failure".
///
/// **Why this exists** (2026-04-26 incident): the original watchdog
/// fired CRITICAL Telegram on ANY `pre_market_check` error. On a Sunday
/// at 13:09 IST the operator's laptop hit a 30-second DNS hiccup
/// (`failed to lookup address information`) and the watchdog paged
/// CRITICAL — but nothing was actually wrong with the Dhan token or
/// data plan. False CRITICAL pages erode the operator's trust in real
/// CRITICAL pages, so we now distinguish:
///
/// * **Transient network failure** (DNS lookup failed, connect refused,
///   connection reset, request timeout, no route to host) → log WARN,
///   increment `tv_mid_session_profile_transient_failure_total`, no
///   rising edge. The next 15-minute cycle will re-check.
/// * **REST-surface degradation** (HTTP 400/429/5xx/WAF responses, or a
///   200 with an unparseable body) → log WARN + counter, DRIVE the
///   rising edge (since 2026-07-14 a coded `error!` + counter, no
///   Telegram) — but this is NOT evidence the token is invalid, so it
///   NEVER escalates toward the AUTH-GAP-05 forced re-mint (see
///   [`CycleOutcome::RestSurfaceDegraded`]).
/// * **Real auth failure** (HTTP 401/403, dataPlan inactive,
///   activeSegment missing Derivative, token expiry < 4h) →
///   drive the rising edge AND count toward the forced re-mint
///   threshold (the self-heal path).
///
/// Returns `(is_real_auth_failing, reason)`. The boolean drives the
/// state machine; transient failures return `false` so they never
/// touch `state.currently_failing`. This means a transient blip
/// followed by a real auth failure on the NEXT cycle still fires the
/// rising edge — which is the property we want.
#[cfg(test)]
fn classify_check_result(
    result: &Result<(), tickvault_common::error::ApplicationError>,
) -> (bool, Option<String>) {
    // R6-CPLX-1 (2026-07-07): single-pass — classify once, then derive the
    // boolean + side effects from that one verdict. The watchdog loop calls
    // `classify_cycle` + `report_cycle` directly with the SAME semantics;
    // this wrapper is kept (test-only) for the 2026-04-26 ratchet tests
    // below, which pin the exact truth table the loop now derives via
    // `classify_cycle` + `report_cycle`.
    let (outcome, reason) = classify_cycle(result);
    report_cycle(outcome, reason)
}

/// R6-CPLX-1 (2026-07-07): single-pass classification — renders the error
/// Display ONCE and runs [`classify_failure_reason`] ONCE, returning both
/// the 4-way [`CycleOutcome`] and the rendered reason so the watchdog loop
/// never classifies the same check result twice (previously `cycle_outcome`
/// + `classify_check_result` each re-ran the classifier and the Display was
/// formatted up to three times per failing cycle).
fn classify_cycle(
    result: &Result<(), tickvault_common::error::ApplicationError>,
) -> (CycleOutcome, Option<String>) {
    match result {
        Ok(()) => (CycleOutcome::Ok, None),
        Err(err) => {
            let reason = format!("{err}");
            let outcome = classify_failure_reason(&reason);
            (outcome, Some(reason))
        }
    }
}

/// R6-CPLX-1: derives the rising-edge boolean from an ALREADY-classified
/// cycle and fires the transient / REST-degraded warn!/counter side effects
/// exactly once. Consumes the [`classify_cycle`] output — the classifier is
/// never re-run here.
fn report_cycle(outcome: CycleOutcome, reason: Option<String>) -> (bool, Option<String>) {
    match outcome {
        CycleOutcome::Ok => (false, reason),
        CycleOutcome::Transient => {
            warn!(
                reason = %reason.as_deref().unwrap_or_default(),
                "mid-session profile check encountered transient network error — not paging (will retry next cycle)"
            );
            metrics::counter!("tv_mid_session_profile_transient_failure_total").increment(1);
            (false, reason)
        }
        CycleOutcome::RestSurfaceDegraded => {
            // Pages via the pre-existing rising-edge CRITICAL (the boolean
            // below is `true`) but never mints — a 5xx is not a rejection
            // of OUR login.
            warn!(
                reason = %reason.as_deref().unwrap_or_default(),
                "mid-session profile check hit a non-auth REST failure (5xx/429/400/parse) — rising edge (silent), NOT counting toward re-mint"
            );
            metrics::counter!("tv_mid_session_profile_rest_degraded_total").increment(1);
            (true, reason)
        }
        CycleOutcome::RealAuthFail => (true, reason),
    }
}

/// AUTH-GAP-05 (2026-07-06): pure 4-way cycle classification. The clean
/// split (vs the boolean `classify_check_result` returns) exists so the
/// consecutive-failure counter can treat a transient network blip — AND a
/// non-auth REST-surface failure — as evidence NEITHER way: neither must
/// escalate toward a re-mint nor clear a genuine failing episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CycleOutcome {
    /// `/v2/profile` check passed.
    Ok,
    /// REAL auth failure (HTTP 401/403, dataPlan inactive, token expiry).
    RealAuthFail,
    /// AG5-R1-3 / EDGE-3 (2026-07-06): a REST-SURFACE failure that is NOT
    /// evidence the token is invalid — HTTP 400/429/5xx/WAF responses, or
    /// a 200 whose body failed to parse. The 2026-06-10 incident class
    /// (`dhan-rest-canary-error-codes.md` §0: every api.dhan.co REST call
    /// returned 400 for a full day while the live WS was HEALTHY) lands
    /// here: it keeps driving the rising edge (a coded error!, silent since
    /// the 2026-07-14 noise lock) but must NEVER trigger
    /// a `generateAccessToken` mint — the mint (via the likely-healthy
    /// auth.dhan.co) would invalidate the token the healthy live main-feed
    /// WS is riding (Dhan: one active token at a time).
    RestSurfaceDegraded,
    /// Transient network blip (DNS / reset / timeout on the send leg).
    Transient,
}

/// Pure. Classifies one `pre_market_check` result into the 4-way
/// [`CycleOutcome`]. Both this and `classify_check_result` delegate to
/// [`classify_cycle`] so the 2026-04-26 transient ratchet tests keep
/// pinning ONE classifier (R6-CPLX-1: single classification pass; this
/// wrapper is test-only — production uses `classify_cycle` directly).
#[cfg(test)]
fn cycle_outcome(result: &Result<(), tickvault_common::error::ApplicationError>) -> CycleOutcome {
    classify_cycle(result).0
}

/// The `ApplicationError::AuthenticationFailed` Display prefix
/// (`#[error("Dhan authentication failed: {reason}")]`) — stripped so the
/// wrapper checks below anchor on the reason's true start whether the
/// caller passes the raw reason or the Display-rendered error.
///
/// AG5-R2-1: this ONE literal cannot be shared with its producer (it lives
/// inside a cross-crate `thiserror` attribute in
/// `crates/common/src/error.rs`, which requires a literal), so it is pinned
/// by the BEHAVIOURAL ratchet
/// `auth_failed_display_prefix_matches_application_error_display` below —
/// a reword of the Display attribute fails that test. The three
/// `get_user_profile` wrappers (`SEND_LEG_WRAPPER` / `HTTP_RESPONSE_WRAPPER`
/// / `PARSE_ERROR_WRAPPER`) are shared constants imported from
/// `token_manager.rs` (the producer) — see the `use` block at the top.
const AUTH_FAILED_DISPLAY_PREFIX: &str = "Dhan authentication failed: ";

/// Pure. Strips the optional [`AUTH_FAILED_DISPLAY_PREFIX`] so wrapper
/// matching is prefix-anchored on the underlying reason.
///
/// `pub(crate)` since 2026-07-08 for the AUTH-GAP-06 fast-boot cached-token
/// classifier (its module was deleted 2026-07-14 per the Dhan noise lock;
/// the visibility is kept — a future auth classifier reuses this exact
/// prefix-anchoring primitive rather than redefining it).
pub(crate) fn reason_core(reason: &str) -> &str {
    reason
        .strip_prefix(AUTH_FAILED_DISPLAY_PREFIX)
        .unwrap_or(reason)
}

/// SEC-R2-2 (2026-07-06). Pure. True iff a rendered `force_renewal` error
/// IS the RESILIENCE-03 in-flight mint refusal from `acquire_token`'s lock
/// tripwire — PREFIX-anchored on the shared
/// [`RESILIENCE03_MINT_REFUSAL_REASON_PREFIX`] literal (produced by our own
/// code at the reason's start), never a `contains` scan of the whole
/// string: the mint-HTTP-failure shape
/// (`"generateAccessToken HTTP {status} url=… body={captured_body}"`)
/// embeds a server-controlled body that could otherwise plant
/// "RESILIENCE-03" and forge the permanent classification (SEC-R1-3
/// principle: never substring-scan text that concatenates a server
/// response body).
fn is_inflight_lock_refusal(rendered_error: &str) -> bool {
    reason_core(rendered_error).starts_with(RESILIENCE03_MINT_REFUSAL_REASON_PREFIX)
}

/// H3 (2026-07-14 fix round). Pure. True iff a rendered `force_renewal`
/// error IS the TokenManager mint-cooldown skip (`renew_with_fallback`
/// refused the `generateAccessToken` fallback because ANOTHER caller
/// minted within the ~125s Dhan cooldown). PREFIX-anchored on our own
/// shared [`MINT_COOLDOWN_REFUSAL_REASON_PREFIX`] literal — same SEC-R1-3
/// discipline as [`is_inflight_lock_refusal`] (never substring-scan text
/// that concatenates a server-controlled body).
fn is_mint_cooldown_refusal(rendered_error: &str) -> bool {
    reason_core(rendered_error).starts_with(MINT_COOLDOWN_REFUSAL_REASON_PREFIX)
}

/// Pure. Parses the HTTP status from the FIXED position immediately after
/// the [`HTTP_RESPONSE_WRAPPER`] prefix. SEC-R1-3: the status token is
/// produced by OUR `format!` before any server body is appended, so it is
/// the only part of the string an upstream cannot forge.
///
/// `pub(crate)` since 2026-07-08 for the AUTH-GAP-06 fast-boot cached-token
/// classifier (its module was deleted 2026-07-14 per the Dhan noise lock;
/// the visibility is kept so any future auth classifier parses the status
/// from the SAME fixed, unforgeable position).
pub(crate) fn http_response_status(core: &str) -> Option<u16> {
    let rest = core.strip_prefix(HTTP_RESPONSE_WRAPPER)?;
    let digits: &str = rest
        .find(|c: char| !c.is_ascii_digit())
        .map_or(rest, |end| &rest[..end]);
    digits.parse().ok()
}

/// Pure, prefix-anchored failure classification (SEC-R1-3: never
/// substring-scan text that concatenates a server response body).
///
/// Order (each wrapper is checked as a PREFIX of the reason core):
/// 1. HTTP-response wrapper → status-aware: 401/403 = the broker rejected
///    our login → [`CycleOutcome::RealAuthFail`]; every other status
///    (400/429/5xx/WAF) = REST surface degraded, token NOT implicated →
///    [`CycleOutcome::RestSurfaceDegraded`].
/// 2. Parse-error wrapper (200 with garbage body) → RestSurfaceDegraded.
/// 3. Send-leg wrapper + a known transient needle → Transient; send-leg
///    wrapper WITHOUT a known needle → RestSurfaceDegraded (F12,
///    2026-07-08): a send-leg failure means NO response was ever received,
///    so it can never be evidence the TOKEN is bad — it must page (via the
///    rising-edge CRITICAL) but never walk the counter toward a
///    DESTRUCTIVE mint. Previously an unrecognized send-leg error string
///    (a new reqwest wording, a proxy-injected message) fell through to
///    RealAuthFail and two such cycles minted — destroying a possibly
///    healthy token over a pure network-layer novelty.
/// 4. Anything else (dataPlan inactive, activeSegment missing, token
///    expiry, unknown shapes) → RealAuthFail — fail-safe default: page AND
///    count toward the re-mint threshold.
fn classify_failure_reason(reason: &str) -> CycleOutcome {
    let core = reason_core(reason);
    if let Some(status) = http_response_status(core) {
        return if status == 401 || status == 403 {
            CycleOutcome::RealAuthFail
        } else {
            CycleOutcome::RestSurfaceDegraded
        };
    }
    if core.starts_with(PARSE_ERROR_WRAPPER) {
        return CycleOutcome::RestSurfaceDegraded;
    }
    if core.starts_with(SEND_LEG_WRAPPER) {
        // The send-leg wrapper proves the request never got a response —
        // network-side by construction (a real HTTP response uses the
        // HTTP-response wrapper). Known-transient needles stay Transient
        // (no page); an UNKNOWN send-leg remainder pages but never mints.
        return if is_transient_network_failure(core) {
            CycleOutcome::Transient
        } else {
            CycleOutcome::RestSurfaceDegraded
        };
    }
    CycleOutcome::RealAuthFail
}

/// Pure. Next value of the consecutive REAL-failure counter.
///
/// * `Ok` → 0 (episode over).
/// * `RealAuthFail` → prev + 1 (saturating).
/// * `RestSurfaceDegraded` → prev unchanged — a REST-surface outage is no
///   evidence the token is invalid; it must never walk the counter toward
///   a destructive mint (AG5-R1-3), and it must not clear a real episode
///   either (the token could genuinely be dead BEHIND the outage).
/// * `Transient` → prev unchanged — a blip is evidence neither way, so a
///   transient BETWEEN two real failures still reaches the threshold.
fn next_consecutive_failures(prev: u32, outcome: CycleOutcome) -> u32 {
    match outcome {
        CycleOutcome::Ok => 0,
        CycleOutcome::RealAuthFail => prev.saturating_add(1),
        CycleOutcome::RestSurfaceDegraded | CycleOutcome::Transient => prev,
    }
}

/// AUTH-GAP-05: applies one cycle's outcome to the watchdog state + the
/// shared profile-truth flag. Extracted from the loop so the reset /
/// increment / hold semantics are unit-testable without I/O.
///
/// * `Ok` — counter + re-mint latch reset (episode over); flag → `true`.
/// * `RealAuthFail` — counter increments; flag → `false`.
/// * `RestSurfaceDegraded` — state AND flag untouched (a 5xx/429/400/parse
///   failure is not evidence the token is invalid — it must neither flip
///   `tv_token_valid` to a false 0.0 nor escalate/clear the re-mint
///   episode; the rising-edge coded error! still fires via
///   `classify_check_result`).
/// * `Transient` — state AND flag untouched (a blip never flips
///   `tv_token_valid` to a false 0.0, and never clears a real episode).
fn apply_cycle_outcome(
    state: &mut WatchdogState,
    outcome: CycleOutcome,
    profile_valid: &AtomicBool,
) {
    match outcome {
        CycleOutcome::Ok => {
            state.consecutive_invalid = 0;
            state.remint_attempted_this_episode = false;
            state.remint_attempts_this_episode = 0;
            state.terminal_paged_this_episode = false;
            profile_valid.store(true, Ordering::Release);
        }
        CycleOutcome::RealAuthFail => {
            state.consecutive_invalid =
                next_consecutive_failures(state.consecutive_invalid, outcome);
            profile_valid.store(false, Ordering::Release);
            // GAP-04 (2026-07-14): a PERSISTING episode re-arms the
            // retry-once latch every REMINT_REARM_FAILING_CYCLES failing
            // cycles (~30 min), so a still-dead token keeps re-minting
            // (silently) instead of stalling after the single attempt —
            // BOUNDED by the H1b per-episode attempt cap: once the budget
            // is spent the re-arm STOPS (the cap page fires instead) until
            // a clean Ok cycle clears the episode.
            if state.remint_attempted_this_episode
                && !remint_cap_reached(state.remint_attempts_this_episode)
                && should_rearm_remint_latch(state.consecutive_invalid)
            {
                state.remint_attempted_this_episode = false;
            }
        }
        CycleOutcome::RestSurfaceDegraded | CycleOutcome::Transient => {}
    }
}

/// AUTH-GAP-05: verdict of the pure re-mint decision — the unit-tested
/// heart of the forced re-mint trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemintDecision {
    /// Below the consecutive-failure threshold — keep observing.
    Wait,
    /// Already re-minted this failing episode (retry-once latch).
    AlreadyAttempted,
    /// Dual-instance lock NOT held — refuse fail-closed, never mint.
    RefuseLockLost,
    /// Inside the ~125s Dhan mint cooldown — hold this cycle.
    HoldCooldown,
    /// Fire exactly one forced re-mint.
    Trigger,
}

/// Pure. Decision order (each check strictly before the next):
/// threshold → retry-once latch → LOCK (fail-closed — the safety refusal
/// BEATS the cooldown so it can never be masked by a cooldown hold) →
/// cooldown → Trigger.
///
/// `lock_held == None` (fast-boot crash-recovery arm / tests) is treated
/// as held — the tripwire is disarmed, matching `acquire_token`'s own
/// gate on that arm (documented residual, `dual-instance-lock-2026-07-04.md` §3).
fn decide_remint(
    consecutive_invalid: u32,
    remint_already_attempted: bool,
    cooldown_elapsed: bool,
    lock_held: Option<bool>,
) -> RemintDecision {
    if consecutive_invalid < CONSECUTIVE_INVALID_REMINT_THRESHOLD {
        return RemintDecision::Wait;
    }
    if remint_already_attempted {
        return RemintDecision::AlreadyAttempted;
    }
    match lock_held {
        Some(false) => RemintDecision::RefuseLockLost,
        _ => {
            if cooldown_elapsed {
                RemintDecision::Trigger
            } else {
                RemintDecision::HoldCooldown
            }
        }
    }
}

/// COV-R6-1 (2026-07-07): the retry-once episode latch + Dhan mint cooldown
/// stamp, extracted from the loop body so the exactly-once-per-episode
/// bound (the AUTH-GAP-05 runbook's "retry-once latch" contract in
/// `wave-4-error-codes.md` §AUTH-GAP-05) is a pure, unit-tested mutation
/// instead of two untested assignments inside the TEST-EXEMPT loop —
/// deleting either assignment previously left every unit test AND both
/// source-scan ratchets green while the watchdog re-minted (or re-paged
/// RESILIENCE-01) every 900s cycle.
///
/// * [`RemintDecision::Trigger`] — latch the episode AND stamp the ~125s
///   Dhan mint cooldown (a mint is about to be attempted).
/// * [`RemintDecision::RefuseLockLost`] — latch the episode (so the
///   RESILIENCE-01 refusal page fires at most once per episode) WITHOUT
///   stamping the cooldown — no mint was attempted, so no Dhan cooldown
///   applies.
/// * `Wait` / `AlreadyAttempted` / `HoldCooldown` — no state change.
///
/// A clean `Ok` cycle re-arms via [`apply_cycle_outcome`] (latch reset).
fn apply_remint_decision(
    state: &mut WatchdogState,
    last_remint_at: &mut Option<Instant>,
    decision: RemintDecision,
) {
    match decision {
        RemintDecision::Trigger => {
            state.remint_attempted_this_episode = true;
            // H1b: one unit of the per-episode mint budget is spent the
            // moment a mint is ATTEMPTED (success or failure — a
            // "successful" mint that leaves the profile invalid is the
            // dead-dataPlan class the cap page exists for).
            state.remint_attempts_this_episode =
                state.remint_attempts_this_episode.saturating_add(1);
            *last_remint_at = Some(Instant::now());
        }
        RemintDecision::RefuseLockLost => {
            state.remint_attempted_this_episode = true;
        }
        RemintDecision::Wait | RemintDecision::AlreadyAttempted | RemintDecision::HoldCooldown => {}
    }
}

/// Returns true if the failure reason is a transient network error
/// (laptop offline / DNS flap / Dhan socket reset) rather than a
/// real Dhan-side auth or data-plan invalidation.
///
/// The reason strings come from [`crate::auth::token_manager::TokenManager::get_user_profile`]
/// and reach this classifier via `format!("{err}")` on the
/// `ApplicationError::AuthenticationFailed` Display, which prepends
/// `"Dhan authentication failed: "` to the inner reason. So the
/// strings we see here look like one of:
///
/// * `"Dhan authentication failed: profile request failed: <reqwest::Error>"`
///   — the HTTP send leg failed before any response was received.
///   ALL such cases are network-side. We require BOTH the
///   `"profile request failed:"` wrapper (proves we're on the send
///   leg, not a real HTTP 4xx response which uses the
///   `"profile request HTTP {status}"` wrapper) AND one of the
///   transient substrings below.
/// * `"Dhan authentication failed: profile request HTTP 401 ..."` —
///   real auth failure; NOT transient.
/// * `"Dhan authentication failed: data plan is 'Inactive' ..."` —
///   real data-plan failure; NOT transient.
///
/// New substrings should be added here only after observing them in
/// `tv_mid_session_profile_transient_failure_total` not incrementing
/// during a known transient incident — i.e. the ratchet tests in this
/// module prove which cases are covered.
fn is_transient_network_failure(reason: &str) -> bool {
    // Must be the HTTP-send-leg wrapper, NOT the HTTP-response wrapper.
    // SEC-R1-3 (2026-07-06): PREFIX-anchored (`strip_prefix` on the
    // Display-prefix-stripped core), never `contains` — a hostile HTTP
    // response body (embedded after `body=` in the HTTP-response wrapper)
    // could otherwise plant the send-leg marker + a transient needle and
    // make a REAL 401 classify as Transient, silently suppressing both the
    // CRITICAL page and the AUTH-GAP-05 self-heal. The needles below scan
    // ONLY the send-leg remainder, which is client-generated reqwest error
    // text — with ONE server-influenced exception, closed at the source
    // (SEC-C2-2, 2026-07-07): under reqwest's DEFAULT redirect policy a
    // redirect-loop/limit failure surfaces on the SEND leg carrying the
    // FINAL redirect URL from the server-controlled Location header, whose
    // path can plant a space-free needle ("connecterror" / "timedout" —
    // no %-encoding needed) and downgrade a paging RestSurfaceDegraded
    // outage to a silent Transient. The TokenManager HTTP client therefore
    // pins `redirect(reqwest::redirect::Policy::none())`
    // (token_manager.rs, both production builders — the §18 CSV-downloader
    // precedent; Dhan's auth + API endpoints never legitimately redirect),
    // so a 3xx is an ordinary non-2xx RESPONSE → the HTTP-response wrapper
    // → RestSurfaceDegraded (correct + paging), and no server-controlled
    // URL can reach this send-leg scan. Ratchet:
    // token_manager.rs::tests::test_production_http_clients_pin_redirect_policy_none.
    let Some(send_leg_rest) = reason_core(reason).strip_prefix(SEND_LEG_WRAPPER) else {
        return false;
    };
    let lower = send_leg_rest.to_lowercase();
    [
        // Bare reqwest send-leg error: produced when the inner cause is
        // a generic Request/Connect failure that doesn't surface a more
        // specific substring. Observed on 2026-05-11 14:44 IST production
        // page where the entire reason was just
        // "profile request failed: error sending request for url
        //  (https://api.dhan.co/v2/profile)" with no further qualifier.
        // This is a transport-level failure by construction (an HTTP
        // response would use the "profile request HTTP {status}"
        // wrapper instead), so matching the bare prefix is safe.
        "error sending request for url",
        "dns error",
        "failed to lookup address",
        "connection refused",
        "connection reset",
        "operation timed out",
        "timedout",
        "request timeout",
        "connecterror",
        "no route to host",
        "network is unreachable",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Internal state for the edge-triggered watchdog.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct WatchdogState {
    currently_failing: bool,
    /// AUTH-GAP-05: consecutive REAL `/v2/profile` auth failures observed
    /// (transient blips neither escalate nor clear — see
    /// [`next_consecutive_failures`]).
    consecutive_invalid: u32,
    /// AUTH-GAP-05: DH-901 retry-once latch — set on Trigger OR
    /// RefuseLockLost so at most ONE re-mint fires per re-arm window; a
    /// clean Ok cycle resets it, and GAP-04 re-arms it every
    /// [`REMINT_REARM_FAILING_CYCLES`] failing cycles of a persisting
    /// episode (~30 min retry cadence, silent) — until the H1b
    /// per-episode cap is spent.
    remint_attempted_this_episode: bool,
    /// H1b (2026-07-14 fix round): total forced re-mints fired THIS
    /// episode. The GAP-04 re-arm stops at
    /// [`REMINT_MAX_ATTEMPTS_PER_EPISODE`]; a clean Ok cycle resets.
    remint_attempts_this_episode: u32,
    /// H1a (2026-07-14 fix round): once-per-episode Telegram latch for
    /// the family-(3) `AuthenticationFailed` Critical — set by
    /// [`take_terminal_page`] the first time the episode pages (terminal
    /// mint failure OR the H1b attempt-cap page); a clean Ok cycle
    /// resets. Without it the GAP-04 re-arm re-paged the SAME dead-token
    /// episode every ~30 minutes.
    terminal_paged_this_episode: bool,
}

/// H1a. Pure. Returns `true` exactly once per failing episode — the
/// caller may fire the family-(3) Critical iff this returns `true`. Sets
/// the latch as a side effect; [`apply_cycle_outcome`]'s `Ok` arm resets
/// it when the episode clears.
fn take_terminal_page(state: &mut WatchdogState) -> bool {
    if state.terminal_paged_this_episode {
        return false;
    }
    state.terminal_paged_this_episode = true;
    true
}

/// H1b. Pure. True iff the episode has reached the state where the
/// attempt-cap page is due: the failing threshold was crossed AND the
/// per-episode mint budget is spent (all mints fired, profile still
/// REAL-invalid on the current cycle — the caller additionally gates on
/// `outcome == RealAuthFail` so a REST-surface/transient cycle can never
/// fire it). Once-per-episode delivery is [`take_terminal_page`]'s job.
fn remint_cap_exhausted(state: &WatchdogState) -> bool {
    state.consecutive_invalid >= CONSECUTIVE_INVALID_REMINT_THRESHOLD
        && remint_cap_reached(state.remint_attempts_this_episode)
}

/// Pure verdict — isolates the decision logic from IO so tests can
/// exercise every edge without a live `TokenManager` + `Notifier`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Transition {
    /// No-op — state unchanged (steady ok or steady fail).
    NoOp,
    /// Rising edge — first failure detected. One coded `error!` +
    /// counter (silent — no Telegram since the 2026-07-14 noise lock).
    FirstFailure,
    /// Falling edge — recovery after a failing episode. Fire INFO
    /// (no Telegram — operator already knows from the rising edge).
    Recovery,
}

fn evaluate_transition(state: &WatchdogState, is_failing_now: bool) -> Transition {
    match (state.currently_failing, is_failing_now) {
        (false, true) => Transition::FirstFailure,
        (true, false) => Transition::Recovery,
        _ => Transition::NoOp,
    }
}

fn apply_transition(
    transition: Transition,
    state: &mut WatchdogState,
    reason: Option<String>,
    outcome: CycleOutcome,
) {
    match transition {
        Transition::NoOp => {}
        Transition::FirstFailure => {
            let reason = reason.unwrap_or_else(|| "no reason provided".to_string());
            // SILENT since 2026-07-14 (Dhan noise lock): coded error! +
            // counter only — the MidSessionProfileInvalidated Critical
            // Telegram page is DELETED. The AUTH-GAP-05 re-mint machinery
            // self-heals a REAL auth failure; only a TERMINAL re-mint
            // failure / the H1b attempt-cap pages (the family-(3) arms in
            // the loop above). M1 (fix round): the wording is
            // class-accurate — a REST-surface degrade NEVER engages a
            // re-mint, so its rising edge must not claim a self-heal.
            if outcome == CycleOutcome::RestSurfaceDegraded {
                error!(
                    code = ErrorCode::AuthGap05ForcedRemintTriggered.code_str(),
                    reason = %reason,
                    "mid-session profile check FAILED — REST surface degraded (transport-failure class: 5xx/429/400/parse/unknown-send-leg); NO Dhan re-login attempted (this is not evidence the token is bad)"
                );
            } else {
                error!(
                    code = ErrorCode::AuthGap05ForcedRemintTriggered.code_str(),
                    reason = %reason,
                    "mid-session profile check FAILED — dataPlan / activeSegment / token may be invalid (silent; AUTH-GAP-05 self-heal engaged)"
                );
            }
            metrics::counter!("tv_mid_session_profile_alert_fired_total").increment(1);
            state.currently_failing = true;
        }
        Transition::Recovery => {
            let reason_context = reason.unwrap_or_default();
            info!(
                last_failure_reason = %reason_context,
                "mid-session profile check recovered (silent falling edge)"
            );
            metrics::counter!("tv_mid_session_profile_recovery_total").increment(1);
            state.currently_failing = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_transition_first_failure() {
        let state = WatchdogState::default();
        let t = evaluate_transition(&state, true);
        assert_eq!(t, Transition::FirstFailure);
    }

    #[test]
    fn evaluate_transition_recovery_after_failure() {
        let state = WatchdogState {
            currently_failing: true,
            ..WatchdogState::default()
        };
        let t = evaluate_transition(&state, false);
        assert_eq!(t, Transition::Recovery);
    }

    #[test]
    fn evaluate_transition_steady_fail_no_duplicate_alert() {
        let state = WatchdogState {
            currently_failing: true,
            ..WatchdogState::default()
        };
        let t = evaluate_transition(&state, true);
        assert_eq!(t, Transition::NoOp);
    }

    #[test]
    fn evaluate_transition_steady_ok_no_op() {
        let state = WatchdogState::default();
        let t = evaluate_transition(&state, false);
        assert_eq!(t, Transition::NoOp);
    }

    /// Rising-edge followed by falling-edge in the same test — proves
    /// the state flips correctly across a full failure episode.
    #[test]
    fn apply_transition_episode_round_trip() {
        let mut state = WatchdogState::default();
        apply_transition(
            Transition::FirstFailure,
            &mut state,
            Some("test failure".to_string()),
            CycleOutcome::RealAuthFail,
        );
        assert!(state.currently_failing);
        apply_transition(Transition::Recovery, &mut state, None, CycleOutcome::Ok);
        assert!(!state.currently_failing);
    }

    #[test]
    fn interval_constant_is_sane() {
        // At least 5 min (so we don't pound Dhan).
        const _: () = assert!(MID_SESSION_CHECK_INTERVAL_SECS >= 300);
        // At most 30 min (so mid-session invalidation is caught fast).
        const _: () = assert!(MID_SESSION_CHECK_INTERVAL_SECS <= 1800);
    }

    // ---------------------------------------------------------------
    // 2026-04-26 transient-vs-real classifier ratchet tests.
    //
    // Every substring matched by `is_transient_network_failure` MUST
    // have a covering test below. Adding a new substring without a
    // test will let a real auth failure slip through the classifier
    // unnoticed.
    // ---------------------------------------------------------------

    #[test]
    fn transient_dns_lookup_failure_is_transient() {
        // Verbatim from the 2026-04-26 13:09 IST production incident.
        let reason = "profile request failed: error sending request for url \
                      (https://api.dhan.co/v2/profile): client error (Connect): \
                      dns error: failed to lookup address information: nodename \
                      nor servname provided, or not known";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn transient_connection_refused_is_transient() {
        let reason = "profile request failed: error sending request for url \
                      (https://api.dhan.co/v2/profile): \
                      Connection refused (os error 111)";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn transient_connection_reset_is_transient() {
        // Mirrors the order-update WS reset error pattern.
        let reason = "profile request failed: error sending request for url \
                      (https://api.dhan.co/v2/profile): \
                      Connection reset by peer (os error 54)";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn transient_request_timeout_is_transient() {
        let reason = "profile request failed: \
                      reqwest::Error { kind: Request, source: TimedOut }";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn transient_bare_reqwest_send_error_is_transient() {
        // Verbatim from the 2026-05-11 14:44 IST production page.
        // The reqwest::Error Display did not include a DNS / Connect /
        // Timeout sub-cause in the rendered string, so the previous
        // substring whitelist let it slip through and CRITICAL-paged.
        let reason = "profile request failed: error sending request for url \
                      (https://api.dhan.co/v2/profile)";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn transient_no_route_to_host_is_transient() {
        let reason = "profile request failed: error sending request \
                      for url (https://api.dhan.co/v2/profile): \
                      No route to host (os error 65)";
        assert!(is_transient_network_failure(reason));
    }

    #[test]
    fn real_auth_http_401_is_not_transient() {
        // Genuine auth failure — must drive the rising edge.
        let reason = "profile request HTTP 401 Unauthorized — see server logs for details";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn real_auth_http_403_is_not_transient() {
        let reason = "profile request HTTP 403 Forbidden — see server logs for details";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn real_data_plan_inactive_is_not_transient() {
        let reason = "data plan is 'Inactive', must be 'Active' for market data access";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn real_active_segment_missing_is_not_transient() {
        let reason = "activeSegment does not contain 'Derivative' — F&O trading not enabled";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn real_token_expiring_is_not_transient() {
        let reason = "token validity 2h12m — must have > 4h before market open";
        assert!(!is_transient_network_failure(reason));
    }

    #[test]
    fn empty_reason_is_not_transient() {
        // Defensive — never page on an empty string but never falsely
        // classify it as transient either. Returning false here means
        // the state machine still treats it as a real failure (and
        // pages on the rising edge), which is the safe-by-default
        // behaviour for an unknown error shape.
        assert!(!is_transient_network_failure(""));
    }

    #[test]
    fn reason_without_profile_request_failed_prefix_is_not_transient() {
        // A reason like "dns error" with no prefix MUST NOT be
        // classified as transient — it could equally come from a
        // future error path we haven't audited.
        let reason = "dns error: failed to lookup address information";
        assert!(!is_transient_network_failure(reason));
    }

    /// Classifier returns (is_real_auth_failing, reason). Transient
    /// inputs MUST set `is_real_auth_failing = false` so the state
    /// machine never enters the failing state — that's the property
    /// that prevents false CRITICAL pages.
    #[test]
    fn classify_check_result_transient_does_not_count_as_failing() {
        use tickvault_common::error::ApplicationError;
        let err = ApplicationError::AuthenticationFailed {
            reason: "profile request failed: dns error: failed to lookup address \
                     information"
                .to_string(),
        };
        let result: Result<(), ApplicationError> = Err(err);
        let (is_failing, reason) = classify_check_result(&result);
        assert!(
            !is_failing,
            "transient DNS failure must NOT count as failing"
        );
        assert!(reason.is_some());
    }

    #[test]
    fn classify_check_result_real_auth_counts_as_failing() {
        use tickvault_common::error::ApplicationError;
        let err = ApplicationError::AuthenticationFailed {
            reason: "data plan is 'Inactive', must be 'Active' for market data \
                     access"
                .to_string(),
        };
        let result: Result<(), ApplicationError> = Err(err);
        let (is_failing, reason) = classify_check_result(&result);
        assert!(is_failing, "real auth failure must count as failing");
        assert!(reason.is_some());
    }

    #[test]
    fn classify_check_result_ok_is_not_failing() {
        let result: Result<(), tickvault_common::error::ApplicationError> = Ok(());
        let (is_failing, reason) = classify_check_result(&result);
        assert!(!is_failing);
        assert!(reason.is_none());
    }

    /// Round-trip property: a transient blip followed by a real auth
    /// failure on the very next cycle MUST still fire the rising edge.
    /// This is the headline property the 2026-04-26 hotfix preserves.
    #[test]
    fn transient_blip_then_real_failure_still_pages_critical() {
        use tickvault_common::error::ApplicationError;

        let mut state = WatchdogState::default();

        // Cycle 1: transient (DNS hiccup) — must NOT flip state.
        let r1: Result<(), ApplicationError> = Err(ApplicationError::AuthenticationFailed {
            reason: "profile request failed: dns error: failed to lookup address \
                     information"
                .to_string(),
        });
        let (failing_1, _) = classify_check_result(&r1);
        let t1 = evaluate_transition(&state, failing_1);
        assert_eq!(
            t1,
            Transition::NoOp,
            "transient must NOT trigger the FirstFailure transition"
        );
        apply_transition(t1, &mut state, None, CycleOutcome::Transient);
        assert!(
            !state.currently_failing,
            "state must stay clean after a transient"
        );

        // Cycle 2: real auth failure — MUST trigger FirstFailure
        // (rising edge → coded error!).
        let r2: Result<(), ApplicationError> = Err(ApplicationError::AuthenticationFailed {
            reason: "data plan is 'Inactive', must be 'Active' for market data \
                     access"
                .to_string(),
        });
        let (failing_2, _) = classify_check_result(&r2);
        let t2 = evaluate_transition(&state, failing_2);
        assert_eq!(
            t2,
            Transition::FirstFailure,
            "real auth failure after transient must fire the rising edge"
        );
    }

    // ---------------------------------------------------------------
    // AUTH-GAP-05 (2026-07-06) — forced-re-mint decision core.
    // ---------------------------------------------------------------

    fn transient_err() -> Result<(), tickvault_common::error::ApplicationError> {
        Err(
            tickvault_common::error::ApplicationError::AuthenticationFailed {
                reason: "profile request failed: dns error: failed to lookup address \
                         information"
                    .to_string(),
            },
        )
    }

    fn real_auth_err() -> Result<(), tickvault_common::error::ApplicationError> {
        Err(
            tickvault_common::error::ApplicationError::AuthenticationFailed {
                reason: "profile request HTTP 401 Unauthorized — see server logs for details"
                    .to_string(),
            },
        )
    }

    #[test]
    fn cycle_outcome_ok_is_ok() {
        let r: Result<(), tickvault_common::error::ApplicationError> = Ok(());
        assert_eq!(cycle_outcome(&r), CycleOutcome::Ok);
    }

    #[test]
    fn cycle_outcome_transient_wrapper_is_transient() {
        assert_eq!(cycle_outcome(&transient_err()), CycleOutcome::Transient);
    }

    #[test]
    fn cycle_outcome_real_http_401_is_real_auth_fail() {
        assert_eq!(cycle_outcome(&real_auth_err()), CycleOutcome::RealAuthFail);
    }

    // ---------------------------------------------------------------
    // AG5-R1-3 / EDGE-3 (2026-07-06) — status-aware REST-surface
    // classification: non-auth HTTP failures NEVER escalate to a mint.
    // ---------------------------------------------------------------

    fn http_status_err(status_line: &str) -> Result<(), tickvault_common::error::ApplicationError> {
        Err(
            tickvault_common::error::ApplicationError::AuthenticationFailed {
                reason: format!(
                    "profile request HTTP {status_line} url=https://api.dhan.co/v2/profile \
                     body=<html>gateway error</html>"
                ),
            },
        )
    }

    #[test]
    fn cycle_outcome_http_5xx_is_rest_surface_degraded() {
        // The 2026-06-10 incident class: api.dhan.co REST dies while the
        // live WS is healthy. Must page (pre-existing) but NEVER mint.
        assert_eq!(
            cycle_outcome(&http_status_err("500 Internal Server Error")),
            CycleOutcome::RestSurfaceDegraded
        );
        assert_eq!(
            cycle_outcome(&http_status_err("503 Service Unavailable")),
            CycleOutcome::RestSurfaceDegraded
        );
    }

    #[test]
    fn cycle_outcome_http_400_and_429_are_rest_surface_degraded() {
        assert_eq!(
            cycle_outcome(&http_status_err("400 Bad Request")),
            CycleOutcome::RestSurfaceDegraded
        );
        assert_eq!(
            cycle_outcome(&http_status_err("429 Too Many Requests")),
            CycleOutcome::RestSurfaceDegraded
        );
    }

    #[test]
    fn cycle_outcome_http_403_is_real_auth_fail() {
        assert_eq!(
            cycle_outcome(&http_status_err("403 Forbidden")),
            CycleOutcome::RealAuthFail
        );
    }

    #[test]
    fn cycle_outcome_parse_error_is_rest_surface_degraded() {
        // 200 with a garbage body (WAF/CDN interception) — REST-surface
        // problem, not a token rejection.
        let r: Result<(), tickvault_common::error::ApplicationError> = Err(
            tickvault_common::error::ApplicationError::AuthenticationFailed {
                reason: "profile response parse error: expected value at line 1 column 1"
                    .to_string(),
            },
        );
        assert_eq!(cycle_outcome(&r), CycleOutcome::RestSurfaceDegraded);
    }

    /// SEC-R1-3 regression: a hostile 401 body that embeds BOTH the
    /// send-leg wrapper AND a transient needle must still classify as a
    /// REAL auth failure — the status is parsed from the fixed prefix our
    /// own `format!` produces; the server body can forge neither it nor
    /// the (prefix-anchored) send-leg wrapper.
    #[test]
    fn cycle_outcome_hostile_401_body_with_transient_needles_is_real_auth_fail() {
        let r: Result<(), tickvault_common::error::ApplicationError> = Err(
            tickvault_common::error::ApplicationError::AuthenticationFailed {
                reason: "profile request HTTP 401 Unauthorized \
                         url=https://api.dhan.co/v2/profile \
                         body=profile request failed: connection reset"
                    .to_string(),
            },
        );
        assert_eq!(cycle_outcome(&r), CycleOutcome::RealAuthFail);
    }

    /// SEC-R1-3: the same hostile body must not fool the transient
    /// classifier directly either (prefix-anchored, not `contains`).
    #[test]
    fn hostile_body_embedding_send_leg_wrapper_is_not_transient() {
        let reason = "profile request HTTP 401 Unauthorized \
                      url=https://api.dhan.co/v2/profile \
                      body=profile request failed: connection reset";
        assert!(!is_transient_network_failure(reason));
    }

    /// F12 (2026-07-08): a send-leg failure whose remainder matches NO
    /// known transient needle (a new reqwest wording, a proxy-injected
    /// message) is still a NETWORK-side failure by construction — no
    /// response was ever received, so it cannot implicate the token. It
    /// must classify RestSurfaceDegraded (pages via the rising-edge
    /// CRITICAL, never walks the counter toward a destructive mint).
    /// Previously this branch fell through to RealAuthFail, so two novel
    /// send-leg wordings ~30 min apart minted against a healthy token.
    #[test]
    fn cycle_outcome_send_leg_without_transient_needle_is_rest_surface_degraded() {
        let r: Result<(), tickvault_common::error::ApplicationError> = Err(
            tickvault_common::error::ApplicationError::AuthenticationFailed {
                reason: "profile request failed: some brand-new reqwest wording \
                         this classifier has never seen"
                    .to_string(),
            },
        );
        assert_eq!(cycle_outcome(&r), CycleOutcome::RestSurfaceDegraded);
        // And it never walks the re-mint counter (the toward-mint direction).
        assert_eq!(
            next_consecutive_failures(1, CycleOutcome::RestSurfaceDegraded),
            1,
            "a needle-miss send-leg failure must never escalate toward a mint"
        );
    }

    /// F12 companion: two needle-miss send-leg cycles never reach the
    /// re-mint threshold (the exact regression the reclassification closes).
    #[test]
    fn send_leg_needle_miss_two_cycles_never_reach_remint_threshold() {
        let mut consecutive = 0u32;
        for _ in 0..2 {
            let outcome =
                classify_failure_reason("profile request failed: unknown transport oddity");
            consecutive = next_consecutive_failures(consecutive, outcome);
        }
        assert!(
            consecutive < CONSECUTIVE_INVALID_REMINT_THRESHOLD,
            "two novel send-leg failures must stay below the mint threshold \
             (got {consecutive})"
        );
    }

    /// The Display prefix (`Dhan authentication failed: `) must not defeat
    /// the prefix anchoring in either direction.
    #[test]
    fn display_prefixed_send_leg_is_still_transient() {
        let reason = "Dhan authentication failed: profile request failed: \
                      dns error: failed to lookup address information";
        assert!(is_transient_network_failure(reason));
        assert_eq!(
            classify_failure_reason(reason),
            CycleOutcome::Transient,
            "Display-prefixed send-leg transient must classify Transient"
        );
    }

    /// AUTH-GAP-06 (2026-07-08): `reason_core` became pub(crate) for the
    /// fast-boot cached-token classifier — pin its prefix-stripping
    /// contract directly.
    #[test]
    fn reason_core_strips_optional_display_prefix() {
        assert_eq!(
            reason_core("Dhan authentication failed: profile request HTTP 401"),
            "profile request HTTP 401"
        );
        // No prefix → returned unchanged.
        assert_eq!(
            reason_core("profile request HTTP 401"),
            "profile request HTTP 401"
        );
        // Prefix embedded mid-string is NOT stripped (prefix-anchored).
        assert_eq!(
            reason_core("x Dhan authentication failed: y"),
            "x Dhan authentication failed: y"
        );
    }

    #[test]
    fn http_response_status_parses_fixed_prefix_only() {
        assert_eq!(
            http_response_status("profile request HTTP 503 Service Unavailable url=x body=y"),
            Some(503)
        );
        assert_eq!(
            http_response_status("profile request HTTP 401 Unauthorized"),
            Some(401)
        );
        assert_eq!(http_response_status("profile request failed: x"), None);
        assert_eq!(http_response_status("data plan is 'Inactive'"), None);
    }

    /// COV-R4-2 (2026-07-06): pins the two previously-untested branches of
    /// `http_response_status` — unreachable via the current producer
    /// (token_manager interpolates a reqwest `StatusCode` whose Display
    /// always renders digits + " ReasonPhrase"), but the parse-fail
    /// fallback walks classify_failure_reason to the fail-safe RealAuthFail
    /// default (the toward-mint direction), so both arms are pinned.
    #[test]
    fn http_response_status_all_digits_and_parse_failure_branches() {
        // `map_or` FIRST arm: rest is ENTIRELY digits (nothing after the
        // status code) — no non-digit found, the whole rest parses.
        assert_eq!(http_response_status("profile request HTTP 503"), Some(503));
        // Parse-fail fallback (a): EMPTY digits — a non-digit immediately
        // after the wrapper.
        assert_eq!(http_response_status("profile request HTTP x"), None);
        // Parse-fail fallback (b): value exceeding u16.
        assert_eq!(
            http_response_status("profile request HTTP 99999 huge"),
            None
        );
    }

    #[test]
    fn next_consecutive_failures_rest_degraded_leaves_counter_unchanged() {
        assert_eq!(
            next_consecutive_failures(0, CycleOutcome::RestSurfaceDegraded),
            0
        );
        assert_eq!(
            next_consecutive_failures(1, CycleOutcome::RestSurfaceDegraded),
            1
        );
    }

    #[test]
    fn apply_cycle_outcome_rest_degraded_leaves_state_and_flag_untouched() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState {
            currently_failing: true,
            consecutive_invalid: 1,
            remint_attempted_this_episode: false,
            ..WatchdogState::default()
        };
        apply_cycle_outcome(&mut state, CycleOutcome::RestSurfaceDegraded, &flag);
        assert_eq!(
            state.consecutive_invalid, 1,
            "REST-surface degradation must not escalate toward a mint"
        );
        assert!(!state.remint_attempted_this_episode);
        assert!(
            flag.load(Ordering::Acquire),
            "a 5xx must never flip tv_token_valid to a false 0.0"
        );
    }

    /// Headline AG5-R1-3 / EDGE-3 property: a 30-min pure REST outage
    /// (two consecutive 503 cycles) still drives the rising edge (coded error!)
    /// but NEVER reaches the forced re-mint threshold — the healthy live
    /// WS token is never destroyed by a REST-surface-only failure.
    #[test]
    fn rest_outage_two_cycles_never_reaches_remint_threshold() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState::default();
        for _ in 0..2 {
            let r = http_status_err("503 Service Unavailable");
            let outcome = cycle_outcome(&r);
            apply_cycle_outcome(&mut state, outcome, &flag);
            let (is_failing, _) = classify_check_result(&r);
            assert!(is_failing, "REST outage must still drive the rising edge");
        }
        assert_eq!(state.consecutive_invalid, 0);
        assert_eq!(
            decide_remint(state.consecutive_invalid, false, true, Some(true)),
            RemintDecision::Wait,
            "a pure REST outage must never trigger a token-killing mint"
        );
    }

    #[test]
    fn classify_check_result_rest_degraded_still_counts_as_failing() {
        // Preserves the rising-edge detection for the 2026-06-10
        // all-400s incident class — detection stays, destruction goes.
        let (is_failing, reason) = classify_check_result(&http_status_err("400 Bad Request"));
        assert!(is_failing);
        assert!(reason.is_some());
    }

    #[test]
    fn cycle_outcome_data_plan_inactive_is_real_auth_fail() {
        let r: Result<(), tickvault_common::error::ApplicationError> = Err(
            tickvault_common::error::ApplicationError::AuthenticationFailed {
                reason: "data plan is 'Inactive', must be 'Active' for market data access"
                    .to_string(),
            },
        );
        assert_eq!(cycle_outcome(&r), CycleOutcome::RealAuthFail);
    }

    #[test]
    fn next_consecutive_failures_ok_resets_to_zero() {
        assert_eq!(next_consecutive_failures(7, CycleOutcome::Ok), 0);
    }

    #[test]
    fn next_consecutive_failures_real_fail_increments_saturating() {
        assert_eq!(next_consecutive_failures(0, CycleOutcome::RealAuthFail), 1);
        assert_eq!(next_consecutive_failures(1, CycleOutcome::RealAuthFail), 2);
        assert_eq!(
            next_consecutive_failures(u32::MAX, CycleOutcome::RealAuthFail),
            u32::MAX,
            "counter must saturate, never overflow-panic"
        );
    }

    #[test]
    fn next_consecutive_failures_transient_leaves_counter_unchanged() {
        assert_eq!(next_consecutive_failures(0, CycleOutcome::Transient), 0);
        assert_eq!(next_consecutive_failures(1, CycleOutcome::Transient), 1);
    }

    /// Property: a transient blip BETWEEN two real failures still reaches
    /// the threshold — the blip is evidence neither way.
    #[test]
    fn transient_between_two_real_failures_still_reaches_threshold() {
        let mut consecutive = 0_u32;
        consecutive = next_consecutive_failures(consecutive, CycleOutcome::RealAuthFail);
        consecutive = next_consecutive_failures(consecutive, CycleOutcome::Transient);
        consecutive = next_consecutive_failures(consecutive, CycleOutcome::RealAuthFail);
        assert_eq!(consecutive, CONSECUTIVE_INVALID_REMINT_THRESHOLD);
    }

    #[test]
    fn decide_remint_below_threshold_waits() {
        assert_eq!(
            decide_remint(0, false, true, Some(true)),
            RemintDecision::Wait
        );
        assert_eq!(
            decide_remint(1, false, true, Some(true)),
            RemintDecision::Wait
        );
        // Threshold beats everything — even a lost lock is not evaluated
        // below the threshold.
        assert_eq!(
            decide_remint(1, true, false, Some(false)),
            RemintDecision::Wait
        );
    }

    #[test]
    fn decide_remint_latch_blocks_second_attempt() {
        assert_eq!(
            decide_remint(2, true, true, Some(true)),
            RemintDecision::AlreadyAttempted,
            "retry-once latch must block a second mint this episode"
        );
        // The latch is checked BEFORE the lock — an already-attempted
        // episode never re-evaluates the refusal every cycle.
        assert_eq!(
            decide_remint(3, true, true, Some(false)),
            RemintDecision::AlreadyAttempted
        );
    }

    /// The headline safety ordering: a lost lock refuses fail-closed EVEN
    /// WHEN the cooldown has not elapsed — the safety refusal can never be
    /// masked by a cooldown hold.
    #[test]
    fn decide_remint_lock_lost_beats_cooldown() {
        assert_eq!(
            decide_remint(2, false, false, Some(false)),
            RemintDecision::RefuseLockLost,
            "lock refusal must be decided BEFORE the cooldown check"
        );
        assert_eq!(
            decide_remint(2, false, true, Some(false)),
            RemintDecision::RefuseLockLost
        );
    }

    #[test]
    fn decide_remint_holds_inside_cooldown_when_lock_held() {
        assert_eq!(
            decide_remint(2, false, false, Some(true)),
            RemintDecision::HoldCooldown
        );
        // None lock (fast-boot arm / tests) = tripwire disarmed = held.
        assert_eq!(
            decide_remint(2, false, false, None),
            RemintDecision::HoldCooldown
        );
    }

    #[test]
    fn decide_remint_triggers_at_threshold_with_lock_and_cooldown_ok() {
        assert_eq!(
            decide_remint(2, false, true, Some(true)),
            RemintDecision::Trigger
        );
        // Above threshold still triggers (the latch is what bounds it).
        assert_eq!(
            decide_remint(5, false, true, Some(true)),
            RemintDecision::Trigger
        );
        // None lock (fast-boot arm / tests) is treated as held.
        assert_eq!(decide_remint(2, false, true, None), RemintDecision::Trigger);
    }

    #[test]
    fn apply_cycle_outcome_ok_resets_counter_latch_and_flag() {
        let flag = AtomicBool::new(false);
        let mut state = WatchdogState {
            currently_failing: true,
            consecutive_invalid: 3,
            remint_attempted_this_episode: true,
            ..WatchdogState::default()
        };
        apply_cycle_outcome(&mut state, CycleOutcome::Ok, &flag);
        assert_eq!(state.consecutive_invalid, 0);
        assert!(!state.remint_attempted_this_episode);
        assert!(
            flag.load(Ordering::Acquire),
            "Ok must publish profile_valid = true"
        );
    }

    #[test]
    fn apply_cycle_outcome_real_fail_increments_and_clears_flag() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState::default();
        apply_cycle_outcome(&mut state, CycleOutcome::RealAuthFail, &flag);
        assert_eq!(state.consecutive_invalid, 1);
        assert!(
            !flag.load(Ordering::Acquire),
            "real auth failure must publish profile_valid = false"
        );
        apply_cycle_outcome(&mut state, CycleOutcome::RealAuthFail, &flag);
        assert_eq!(state.consecutive_invalid, 2);
    }

    #[test]
    fn apply_cycle_outcome_transient_leaves_state_and_flag_untouched() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState {
            currently_failing: false,
            consecutive_invalid: 1,
            remint_attempted_this_episode: false,
            ..WatchdogState::default()
        };
        apply_cycle_outcome(&mut state, CycleOutcome::Transient, &flag);
        assert_eq!(state.consecutive_invalid, 1, "transient must not escalate");
        assert!(!state.remint_attempted_this_episode);
        assert!(
            flag.load(Ordering::Acquire),
            "transient must never flip profile_valid to a false 0.0"
        );
    }

    // ---------------------------------------------------------------
    // AG5-R2-1 / SEC-R2-2 (2026-07-06) — shared-wrapper single-sourcing
    // + prefix-anchored permanence classification.
    // ---------------------------------------------------------------

    /// AG5-R2-1 behavioural ratchet: the ONE literal this module cannot
    /// share with its producer (the cross-crate `thiserror` Display
    /// attribute in `crates/common/src/error.rs`) is pinned against the
    /// LIVE Display output — a reword of
    /// `#[error("Dhan authentication failed: {reason}")]` fails here
    /// instead of silently un-anchoring `reason_core` (which would make
    /// every Display-rendered failure fall through to the fail-safe
    /// RealAuthFail default → toward a destructive mint).
    #[test]
    fn auth_failed_display_prefix_matches_application_error_display() {
        let err = tickvault_common::error::ApplicationError::AuthenticationFailed {
            reason: "x".to_string(),
        };
        assert_eq!(
            format!("{err}"),
            format!("{AUTH_FAILED_DISPLAY_PREFIX}x"),
            "ApplicationError::AuthenticationFailed Display drifted from \
             AUTH_FAILED_DISPLAY_PREFIX — reason_core's prefix strip (and \
             therefore ALL wrapper classification) is broken"
        );
    }

    /// AG5-R2-1 source-scan ratchet: the three `get_user_profile` wrapper
    /// strings must exist in token_manager.rs's PRODUCTION region exactly
    /// ONCE each — the shared `pub(crate) const` definition. Zero copies =
    /// the constant (and the producer coupling) was deleted; a second
    /// inline copy = a producer `format!` site stopped interpolating the
    /// shared constant, re-opening the silent-drift → RealAuthFail →
    /// REST-outage-mints regression this fix closes.
    ///
    /// COV-R4-3 (2026-07-06): the FOURTH shared classification constant —
    /// `RESILIENCE03_MINT_REFUSAL_REASON_PREFIX` (prefix-anchored by
    /// `is_inflight_lock_refusal`) — is pinned by the same loop. A producer
    /// edit that inlines a diverging refusal literal instead of
    /// interpolating the constant would silently break the in-flight
    /// RESILIENCE-03 classification (counted as `failed` instead of
    /// `refused_lock_lost_inflight`, `permanent=false` in the payload) —
    /// the exact drift class this ratchet exists to catch.
    #[test]
    fn wrapper_constants_are_single_sourced_in_token_manager() {
        let token_manager_src = std::fs::read_to_string("src/auth/token_manager.rs")
            .or_else(|_| std::fs::read_to_string("crates/core/src/auth/token_manager.rs"))
            .expect("token_manager.rs must be readable");
        // R7-CPLX-1 / COV-R7-2 fix (2026-07-07) + F8 hardening (2026-07-08):
        // split via the SHARED `source_scan::production_region` helper —
        // token_manager.rs carries a doc-comment mention + item-level
        // `#[cfg(test)]` attributes on the test-only constructors BEFORE the
        // real `mod tests`, so the old first-literal split silently EXCLUDED
        // the entire "Error Classification" production tail (~100 lines,
        // incl. `is_permanent_auth_error` / `mint_refused_by_instance_lock`)
        // from this exactly-once duplicate-literal scan — a false-OK
        // direction hole. The helper also keeps any production code that may
        // ever land AFTER the test module in scope.
        let production = tickvault_common::source_scan::production_region(&token_manager_src)
            .expect(
                "token_manager.rs must contain a #[cfg(test)]-gated top-level \
                 `mod tests` module — without it this production-region split \
                 is scanning the whole file (vacuous)",
            );
        // Split-boundary self-checks: the scanned production region must
        // extend PAST the inline #[cfg(test)] constructor attributes to the
        // LAST production items (the Error Classification tail). If any of
        // these fn definitions moves below `mod tests` (or is deleted), this
        // fails loud instead of silently shrinking the scan region.
        for tail_fn in [
            "fn is_permanent_auth_error",
            "fn mint_refused_by_instance_lock",
            "fn is_totp_error",
            "fn is_dhan_rate_limited",
        ] {
            assert!(
                production.contains(tail_fn),
                "token_manager.rs production region (everything before `mod \
                 tests`) must include the tail production item `{tail_fn}` — \
                 the split boundary no longer follows the last production fn \
                 scanned (R7-CPLX-1)"
            );
        }
        for (name, wrapper) in [
            ("PROFILE_SEND_LEG_WRAPPER", SEND_LEG_WRAPPER),
            ("PROFILE_HTTP_RESPONSE_WRAPPER", HTTP_RESPONSE_WRAPPER),
            ("PROFILE_PARSE_ERROR_WRAPPER", PARSE_ERROR_WRAPPER),
            (
                "RESILIENCE03_MINT_REFUSAL_REASON_PREFIX",
                RESILIENCE03_MINT_REFUSAL_REASON_PREFIX,
            ),
        ] {
            let literal = format!("\"{wrapper}\"");
            let count = production.matches(literal.as_str()).count();
            assert_eq!(
                count, 1,
                "token_manager.rs production region must contain the raw \
                 wrapper literal {literal} exactly once (the {name} const \
                 definition); found {count} — either the shared constant was \
                 deleted or a producer format! site duplicated the literal \
                 inline instead of interpolating the constant"
            );
            // The producer sites must actually interpolate the constant.
            assert!(
                production.matches(&format!("{{{name}}}")).count() >= 1,
                "token_manager.rs production region must interpolate {{{name}}} \
                 at its format! producer site(s)"
            );
        }
        // Non-vacuity (F8): the shared helper only returns Some when it
        // found a #[cfg(test)]-gated `mod tests` block; assert the excision
        // actually happened so the scan region is production-only.
        assert!(
            !production.contains("\nmod tests {"),
            "token_manager.rs production region must have its #[cfg(test)] \
             mod tests block excised (blanked) — the production-region split \
             boundary is no longer the test module"
        );
    }

    /// SEC-R2-2: the genuine in-flight RESILIENCE-03 refusal (our own
    /// literal at the reason's START) classifies permanent…
    #[test]
    fn inflight_lock_refusal_matches_genuine_refusal() {
        let genuine = tickvault_common::error::ApplicationError::AuthenticationFailed {
            reason: format!(
                "{RESILIENCE03_MINT_REFUSAL_REASON_PREFIX} — generateAccessToken mint \
                 refused (fail-closed)"
            ),
        };
        assert!(is_inflight_lock_refusal(&format!("{genuine}")));
    }

    /// …while a mint HTTP failure whose SERVER-CONTROLLED body plants
    /// "RESILIENCE-03" must NOT — the classification is prefix-anchored
    /// on our own literal, so the hostile/WAF body cannot forge the
    /// `refused_lock_lost_inflight` label and misdirect the operator to a
    /// dual-instance/peer investigation that does not exist.
    #[test]
    fn inflight_lock_refusal_ignores_server_controlled_body_forgery() {
        let hostile = tickvault_common::error::ApplicationError::AuthenticationFailed {
            reason: "generateAccessToken HTTP 503 url=https://auth.dhan.co/x \
                     body=RESILIENCE-03: instance lock not held"
                .to_string(),
        };
        assert!(!is_inflight_lock_refusal(&format!("{hostile}")));
        // Plain mint failures classify non-permanent too.
        assert!(!is_inflight_lock_refusal(
            "Dhan authentication failed: generateAccessToken request failed: dns error"
        ));
    }

    #[test]
    fn remint_threshold_constant_is_sane() {
        // At least 2 so a single Dhan-side blip never mints…
        const _: () = assert!(CONSECUTIVE_INVALID_REMINT_THRESHOLD >= 2);
        // …at most 4 so a genuinely dead token re-mints within ~1 hour.
        const _: () = assert!(CONSECUTIVE_INVALID_REMINT_THRESHOLD <= 4);
    }

    // ---------------------------------------------------------------
    // COV-R6-1 (2026-07-07) — retry-once episode latch mutation. The
    // latch/stamp assignments previously lived ONLY inside the untested
    // loop body; these tests pin the exactly-once-per-episode contract.
    // ---------------------------------------------------------------

    #[test]
    fn apply_remint_decision_trigger_latches_and_stamps_cooldown() {
        let mut state = WatchdogState {
            consecutive_invalid: CONSECUTIVE_INVALID_REMINT_THRESHOLD,
            ..WatchdogState::default()
        };
        let mut last_remint_at: Option<Instant> = None;
        apply_remint_decision(&mut state, &mut last_remint_at, RemintDecision::Trigger);
        assert!(
            state.remint_attempted_this_episode,
            "Trigger must latch the episode — exactly ONE mint per episode"
        );
        assert!(
            last_remint_at.is_some(),
            "Trigger must stamp the ~125s Dhan mint cooldown"
        );
        // The latch now blocks the NEXT cycle's decision (retry-once).
        assert_eq!(
            decide_remint(
                state.consecutive_invalid.saturating_add(1),
                state.remint_attempted_this_episode,
                true,
                Some(true)
            ),
            RemintDecision::AlreadyAttempted,
            "a latched episode must never re-mint on the next 900s cycle"
        );
    }

    #[test]
    fn apply_remint_decision_refuse_lock_lost_latches_without_cooldown_stamp() {
        let mut state = WatchdogState {
            consecutive_invalid: CONSECUTIVE_INVALID_REMINT_THRESHOLD,
            ..WatchdogState::default()
        };
        let mut last_remint_at: Option<Instant> = None;
        apply_remint_decision(
            &mut state,
            &mut last_remint_at,
            RemintDecision::RefuseLockLost,
        );
        assert!(
            state.remint_attempted_this_episode,
            "RefuseLockLost must latch — the RESILIENCE-01 refusal page \
             fires at most once per episode, never per 900s cycle"
        );
        assert!(
            last_remint_at.is_none(),
            "a refused mint never happened — no Dhan cooldown stamp"
        );
        // The latch blocks re-evaluating the refusal every cycle.
        assert_eq!(
            decide_remint(
                state.consecutive_invalid,
                state.remint_attempted_this_episode,
                true,
                Some(false)
            ),
            RemintDecision::AlreadyAttempted,
        );
    }

    #[test]
    fn apply_remint_decision_non_actionable_decisions_leave_state_untouched() {
        for decision in [
            RemintDecision::Wait,
            RemintDecision::AlreadyAttempted,
            RemintDecision::HoldCooldown,
        ] {
            let mut state = WatchdogState::default();
            let mut last_remint_at: Option<Instant> = None;
            apply_remint_decision(&mut state, &mut last_remint_at, decision);
            assert!(
                !state.remint_attempted_this_episode,
                "{decision:?} must not latch the episode"
            );
            assert!(
                last_remint_at.is_none(),
                "{decision:?} must not stamp the cooldown"
            );
        }
    }

    /// Full episode round trip: Trigger latches → subsequent cycles are
    /// AlreadyAttempted → a clean Ok cycle re-arms via
    /// `apply_cycle_outcome` → the next episode can Trigger again.
    #[test]
    fn remint_episode_latch_round_trip_rearms_on_ok() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState {
            consecutive_invalid: CONSECUTIVE_INVALID_REMINT_THRESHOLD,
            ..WatchdogState::default()
        };
        let mut last_remint_at: Option<Instant> = None;
        apply_remint_decision(&mut state, &mut last_remint_at, RemintDecision::Trigger);
        assert!(state.remint_attempted_this_episode);
        // Clean Ok cycle re-arms the latch.
        apply_cycle_outcome(&mut state, CycleOutcome::Ok, &flag);
        assert!(!state.remint_attempted_this_episode, "Ok must re-arm");
        // A fresh failing episode reaches Trigger again.
        state.consecutive_invalid = CONSECUTIVE_INVALID_REMINT_THRESHOLD;
        assert_eq!(
            decide_remint(
                state.consecutive_invalid,
                state.remint_attempted_this_episode,
                true,
                Some(true)
            ),
            RemintDecision::Trigger
        );
    }

    // ---------------------------------------------------------------
    // R6-CPLX-1 (2026-07-07) — single-pass classification.
    // ---------------------------------------------------------------

    /// `classify_cycle` is the ONE classification pass: its outcome must
    /// agree with `cycle_outcome` (which delegates to it) and it must carry
    /// the rendered reason so the loop never re-formats the error.
    #[test]
    fn classify_cycle_matches_cycle_outcome_and_carries_reason() {
        let ok: Result<(), tickvault_common::error::ApplicationError> = Ok(());
        assert_eq!(classify_cycle(&ok), (CycleOutcome::Ok, None));

        let (outcome, reason) = classify_cycle(&real_auth_err());
        assert_eq!(outcome, CycleOutcome::RealAuthFail);
        assert_eq!(outcome, cycle_outcome(&real_auth_err()));
        assert!(reason.is_some_and(|r| !r.is_empty()));

        let (outcome, reason) = classify_cycle(&transient_err());
        assert_eq!(outcome, CycleOutcome::Transient);
        assert!(reason.is_some());
    }

    /// `report_cycle` derives the rising-edge boolean from the already
    /// computed outcome — same truth table `classify_check_result` pins.
    #[test]
    fn report_cycle_boolean_matches_outcome_truth_table() {
        assert_eq!(report_cycle(CycleOutcome::Ok, None), (false, None));
        let (failing, reason) = report_cycle(CycleOutcome::Transient, Some("blip".to_string()));
        assert!(!failing, "transient must never count as failing");
        assert_eq!(reason.as_deref(), Some("blip"));
        let (failing, _) = report_cycle(CycleOutcome::RestSurfaceDegraded, Some("503".to_string()));
        assert!(failing, "REST degradation must page via rising edge");
        let (failing, _) = report_cycle(CycleOutcome::RealAuthFail, Some("401".to_string()));
        assert!(failing, "real auth failure must count as failing");
    }

    // ---------------------------------------------------------------
    // GAP-04 (2026-07-14, Dhan noise lock) — silent latch re-arm every
    // REMINT_REARM_FAILING_CYCLES failing cycles of a persisting episode.
    // ---------------------------------------------------------------

    #[test]
    fn should_rearm_remint_latch_truth_table() {
        // At/below the first-trigger threshold: never a re-arm (the normal
        // decide_remint path owns the FIRST attempt).
        assert!(!should_rearm_remint_latch(0));
        assert!(!should_rearm_remint_latch(1));
        assert!(!should_rearm_remint_latch(
            CONSECUTIVE_INVALID_REMINT_THRESHOLD
        ));
        // Odd offsets past the threshold: hold.
        assert!(!should_rearm_remint_latch(
            CONSECUTIVE_INVALID_REMINT_THRESHOLD + 1
        ));
        assert!(!should_rearm_remint_latch(
            CONSECUTIVE_INVALID_REMINT_THRESHOLD + 3
        ));
        // Every REMINT_REARM_FAILING_CYCLES-th failing cycle: re-arm.
        assert!(should_rearm_remint_latch(
            CONSECUTIVE_INVALID_REMINT_THRESHOLD + REMINT_REARM_FAILING_CYCLES
        ));
        assert!(should_rearm_remint_latch(
            CONSECUTIVE_INVALID_REMINT_THRESHOLD + 2 * REMINT_REARM_FAILING_CYCLES
        ));
    }

    /// The headline GAP-04 property: a PERSISTING dead-token episode
    /// re-mints every 2nd failing cycle (~30 min) instead of exactly once
    /// — mints at consecutive counts 2, 4, 6 with the latch re-armed by
    /// `apply_cycle_outcome` between them. Silent by design (no Telegram
    /// exists on any of these arms).
    #[test]
    fn gap04_persisting_episode_remints_every_second_failing_cycle() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState::default();
        let mut last_remint_at: Option<Instant> = None;
        let mut mints = Vec::new();
        for cycle in 1..=6u32 {
            apply_cycle_outcome(&mut state, CycleOutcome::RealAuthFail, &flag);
            let decision = decide_remint(
                state.consecutive_invalid,
                state.remint_attempted_this_episode,
                true, // cooldown structurally moot at the 900s cadence
                Some(true),
            );
            apply_remint_decision(&mut state, &mut last_remint_at, decision);
            if decision == RemintDecision::Trigger {
                mints.push(cycle);
            }
        }
        assert_eq!(
            mints,
            vec![2, 4, 6],
            "a persisting episode must re-mint every REMINT_REARM_FAILING_CYCLES \
             (= {REMINT_REARM_FAILING_CYCLES}) failing cycles — got mints at {mints:?}"
        );
        // A clean Ok cycle still resets everything (episode over).
        apply_cycle_outcome(&mut state, CycleOutcome::Ok, &flag);
        assert_eq!(state.consecutive_invalid, 0);
        assert!(!state.remint_attempted_this_episode);
    }

    /// GAP-04 must not disturb the toward-mint safety rails: transient /
    /// REST-degraded cycles neither advance the counter nor re-arm, so a
    /// pure REST outage still never mints.
    #[test]
    fn gap04_rearm_never_fires_on_non_real_failures() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState {
            currently_failing: true,
            consecutive_invalid: CONSECUTIVE_INVALID_REMINT_THRESHOLD,
            remint_attempted_this_episode: true,
            ..WatchdogState::default()
        };
        for _ in 0..4 {
            apply_cycle_outcome(&mut state, CycleOutcome::RestSurfaceDegraded, &flag);
            apply_cycle_outcome(&mut state, CycleOutcome::Transient, &flag);
        }
        assert!(
            state.remint_attempted_this_episode,
            "non-REAL cycles must never re-arm the latch (GAP-04 counts only \
             REAL failing cycles)"
        );
        assert_eq!(
            state.consecutive_invalid,
            CONSECUTIVE_INVALID_REMINT_THRESHOLD
        );
    }

    #[test]
    fn gap04_rearm_cadence_constant_is_sane() {
        // At least 2 (never a per-cycle mint storm — the ~125s Dhan cooldown
        // + the 900s cadence make 2 ≈ a 30-min retry)…
        const _: () = assert!(REMINT_REARM_FAILING_CYCLES >= 2);
        // …at most 4 so a genuinely dead token keeps retrying within the hour.
        const _: () = assert!(REMINT_REARM_FAILING_CYCLES <= 4);
    }

    // ---------------------------------------------------------------
    // H1 (2026-07-14 fix round) — once-per-episode terminal page +
    // per-episode re-mint attempt cap.
    // ---------------------------------------------------------------

    /// H1b cap arithmetic: a persisting dead-token episode mints at
    /// consecutive counts 2, 4, 6 and then STOPS — the GAP-04 re-arm is
    /// blocked once the per-episode budget is spent, and the cap-page
    /// condition becomes due exactly on the next REAL failing cycle.
    #[test]
    fn h1_cap_stops_rearm_after_max_attempts_per_episode() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState::default();
        let mut last_remint_at: Option<Instant> = None;
        let mut mints = Vec::new();
        for cycle in 1..=12u32 {
            apply_cycle_outcome(&mut state, CycleOutcome::RealAuthFail, &flag);
            let decision = decide_remint(
                state.consecutive_invalid,
                state.remint_attempted_this_episode,
                true,
                Some(true),
            );
            apply_remint_decision(&mut state, &mut last_remint_at, decision);
            if decision == RemintDecision::Trigger {
                mints.push(cycle);
            }
        }
        assert_eq!(
            mints,
            vec![2, 4, 6],
            "the episode must mint exactly REMINT_MAX_ATTEMPTS_PER_EPISODE \
             (= {REMINT_MAX_ATTEMPTS_PER_EPISODE}) times, then stop — got {mints:?}"
        );
        assert_eq!(
            state.remint_attempts_this_episode,
            REMINT_MAX_ATTEMPTS_PER_EPISODE
        );
        assert!(
            remint_cap_exhausted(&state),
            "after the cap the cap-page condition must be due on a REAL failing cycle"
        );
    }

    /// H1a latch: the family-(3) page fires at most ONCE per episode; a
    /// clean Ok cycle resets the latch so the NEXT episode can page again.
    #[test]
    fn h1_terminal_page_latch_fires_once_per_episode_and_resets_on_ok() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState::default();
        assert!(take_terminal_page(&mut state), "first take must page");
        assert!(
            !take_terminal_page(&mut state),
            "second take in the SAME episode must NOT page (the pre-fix bug: \
             the GAP-04 re-arm re-paged every ~30 min)"
        );
        assert!(!take_terminal_page(&mut state));
        // Episode clears — the latch re-arms for the next episode.
        apply_cycle_outcome(&mut state, CycleOutcome::Ok, &flag);
        assert!(
            take_terminal_page(&mut state),
            "a NEW episode after a clean Ok cycle must be able to page again"
        );
    }

    /// H1b dead-dataPlan walk: every mint "succeeds" (no terminal failure)
    /// yet the profile stays REAL-invalid — the cap page becomes due
    /// exactly once, on the first failing cycle after the budget is spent,
    /// and never again within the episode.
    #[test]
    fn h1_cap_page_fires_once_when_profile_stays_invalid_after_cap() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState::default();
        let mut last_remint_at: Option<Instant> = None;
        let mut pages = Vec::new();
        for cycle in 1..=12u32 {
            apply_cycle_outcome(&mut state, CycleOutcome::RealAuthFail, &flag);
            // The loop's cap-page block (H1b), replicated verbatim.
            if remint_cap_exhausted(&state) && take_terminal_page(&mut state) {
                pages.push(cycle);
            }
            let decision = decide_remint(
                state.consecutive_invalid,
                state.remint_attempted_this_episode,
                true,
                Some(true),
            );
            apply_remint_decision(&mut state, &mut last_remint_at, decision);
        }
        assert_eq!(
            pages,
            vec![7],
            "the cap page must fire exactly ONCE, on the first REAL failing \
             cycle after the third mint (consecutive 7) — got {pages:?}"
        );
    }

    /// H1 regression floor: a transient blip followed by recovery pages
    /// NOTHING — no mint, no cap condition, no latch consumed.
    #[test]
    fn h1_transient_then_recover_pages_nothing() {
        let flag = AtomicBool::new(true);
        let mut state = WatchdogState::default();
        apply_cycle_outcome(&mut state, CycleOutcome::Transient, &flag);
        assert!(!remint_cap_exhausted(&state));
        assert_eq!(state.remint_attempts_this_episode, 0);
        assert!(!state.terminal_paged_this_episode);
        apply_cycle_outcome(&mut state, CycleOutcome::Ok, &flag);
        assert_eq!(state, WatchdogState::default());
    }

    /// H3 classifier: the TokenManager mint-cooldown skip is recognized
    /// prefix-anchored and must never be treated as a terminal failure.
    #[test]
    fn h3_mint_cooldown_refusal_is_prefix_anchored() {
        let genuine = format!("{MINT_COOLDOWN_REFUSAL_REASON_PREFIX}: last mint was 40s ago");
        assert!(is_mint_cooldown_refusal(&genuine));
        // With the ApplicationError Display prefix (the rendered shape).
        let rendered = format!("Dhan authentication failed: {genuine}");
        assert!(is_mint_cooldown_refusal(&rendered));
        // A server-controlled body embedding the literal mid-string must
        // NOT classify (SEC-R1-3 prefix discipline).
        let forged =
            format!("generateAccessToken HTTP 500 body={MINT_COOLDOWN_REFUSAL_REASON_PREFIX}");
        assert!(!is_mint_cooldown_refusal(&forged));
    }
}
