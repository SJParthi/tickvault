//! Live token-health gauges — 15s poller (AUTH-GAP-05, 2026-07-06).
//!
//! # Why this exists
//!
//! Before this module, `tv_token_remaining_seconds` was emitted only twice
//! per ~23h renewal cycle (the frozen mint-time snapshot inside
//! `TokenManager`'s renewal loop) — so a token that Dhan killed or that
//! passed local expiry mid-day kept reading ~86,395 seconds for hours: a
//! false-OK (audit Rule 11). This poller makes the gauge LIVE (recomputed
//! every [`TOKEN_HEALTH_GAUGE_POLL_SECS`] via the O(1) arc-swap
//! [`TokenManager::seconds_until_expiry`], 0 fail-closed) and adds an
//! honest AND-composed `tv_token_valid` 0/1 gauge:
//!
//! ```text
//! tv_token_valid = 1.0  iff  has_token AND locally_valid AND profile_valid
//! ```
//!
//! `profile_valid` is the mid-session profile watchdog's shared
//! `AtomicBool` (single-writer: the watchdog; single-reader: this poller)
//! — it flips false on a REAL `/v2/profile` auth failure and true on a
//! clean check. The AND composition means the gauge can never show a
//! stale 1.0 for a locally-expired token, and — WHERE THE WATCHDOG RUNS —
//! never for a killed-but-not-yet-expired one either.
//!
//! **Honest envelope (SEC-R3-2, alarm designers READ THIS):** the
//! killed-token half of that claim holds only on the SLOW boot lane, which
//! runs the mid-session profile watchdog. The FAST crash-recovery boot arm
//! (main.rs EDGE-2) spawns this poller with a fresh INERT `profile_valid`
//! flag (`AtomicBool::new(true)`, no writer ever — that arm runs no
//! watchdog), so on the fast arm the composite reflects
//! `has_token AND local expiry` ONLY: a Dhan-KILLED but locally-unexpired
//! token can read `tv_token_valid = 1.0` for up to ~23h there. Do not
//! build an alarm that assumes killed-token detection on the fast arm.
//!
//! # Gauge writers (COV-3 honesty note, 2026-07-06)
//!
//! `tv_token_remaining_seconds` has THREE writers, not one: this 15s
//! poller PLUS the two pre-existing mint/renewal-time snapshot writes in
//! `TokenManager::renewal_loop` (`token_manager.rs` — kept: both are live
//! computations at their write instants, and — while the lane is UP — this
//! poller overwrites within one 15s cadence, so no false-OK results).
//! POST-TEARDOWN the "poller overwrites" justification does not hold (the
//! poller is joined-dead), so the Dhan-lane teardown in `main.rs`
//! abort+JOINS the renewal loop too BEFORE publishing its honest 0/0.0
//! lane-off reset — a late renewal-loop store can never land after the
//! reset (R3-1/AG5-R3-2). `tv_token_valid` is written ONLY by this poller
//! (and reset to 0.0 by the Dhan-lane teardown/Drop in `main.rs` — the
//! honest deliberate-lane-off state).
//! Do NOT delete this poller believing the renewal-loop writes keep the
//! gauge live — they are the frozen snapshots this module exists to fix.
//!
//! # NOT market-hours gated
//!
//! A killed/expired token must read 0 around the clock — off-hours the
//! profile flag simply retains its last in-session value while the
//! local-expiry gate keeps the composite honest.
//!
//! # Hot-path note
//!
//! Zero hot-path cost: a 15s cold-path timer performing two atomic loads
//! and two gauge stores. Nothing here touches the tick pipeline.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::auth::token_manager::TokenManager;

/// Poll cadence (seconds) for the live token-health gauges. 15s keeps the
/// killed-token detection latency to one poll while staying a trivially
/// cheap cold-path timer (two atomic loads per tick).
pub const TOKEN_HEALTH_GAUGE_POLL_SECS: u64 = 15;

/// Pure. `tv_token_valid` = 1.0 iff a token is loaded AND not past local
/// expiry AND the last mid-session `/v2/profile` check did not fail.
/// Else 0.0. AND-composition closes the past-local-expiry false-OK the
/// profile-only signal would leave (audit Rule 11 — no false-OK signals).
#[must_use]
pub fn token_valid_gauge(has_token: bool, locally_valid: bool, profile_valid: bool) -> f64 {
    if has_token && locally_valid && profile_valid {
        1.0
    } else {
        0.0
    }
}

/// Pure. One poll sample → the two gauge values
/// `(tv_token_remaining_seconds, tv_token_valid)`.
///
/// COV-R8-1 (2026-07-07): this fn owns the `locally_valid` derivation
/// (`has_token && remaining > 0`) that previously lived ONLY inside the
/// TEST-EXEMPT spawn body — where a `remaining > 0` → `remaining >= 0`
/// mutation would silently resurrect the expired-token `tv_token_valid=1.0`
/// false-OK (the exact audit-Rule-11 hole this module exists to close).
/// The boundary is pinned by
/// `test_token_health_sample_expired_token_boundary_is_invalid` below, and
/// the source-scan ratchet
/// `ratchet_poller_body_uses_pure_sample_and_emits_both_gauges` pins that
/// the production loop body actually calls this fn and stores BOTH gauges.
#[must_use]
pub fn token_health_sample(has_token: bool, remaining: u64, profile_valid: bool) -> (f64, f64) {
    // A token AT its expiry instant (remaining == 0) is locally EXPIRED —
    // strictly-greater is load-bearing (fail-closed).
    let locally_valid = has_token && remaining > 0;
    #[allow(clippy::cast_precision_loss)] // APPROVED: seconds fit f64 exactly (< 2^53)
    (
        remaining as f64,
        token_valid_gauge(has_token, locally_valid, profile_valid),
    )
}

/// Spawns the token-health gauge poller (see the module-level "Gauge
/// writers" note for the full writer inventory).
///
/// Every [`TOKEN_HEALTH_GAUGE_POLL_SECS`] it sets:
/// - `tv_token_remaining_seconds` — LIVE `expiry - now` via
///   [`TokenManager::seconds_until_expiry`] (0 fail-closed: no token OR
///   already expired).
/// - `tv_token_valid` — the pure [`token_valid_gauge`] composite.
///
/// The first interval tick completes immediately, so both gauges are live
/// within the same scheduler turn as the spawn (no separate boot seed
/// needed). Both are plain gauges — they auto-register on first emit.
// The values set come from the pure `token_health_sample` above
// (boundary-tested — COV-R8-1) fed by `seconds_until_expiry()` (already
// unit-tested, 0 fail-closed — pinned by
// `test_seconds_until_expiry_fail_closed_zero_without_token` below). The
// body's shape is pinned by the source-scan ratchet
// `ratchet_poller_body_uses_pure_sample_and_emits_both_gauges` below.
// TEST-EXEMPT: thin tokio::spawn interval wrapper (see the pins above).
pub fn spawn_token_health_gauge_poller(
    token_manager: Arc<TokenManager>,
    profile_valid: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(TOKEN_HEALTH_GAUGE_POLL_SECS));
        loop {
            interval.tick().await;
            // O(1) arc-swap reads; 0 when no token / already expired.
            let remaining = token_manager.seconds_until_expiry();
            let has_token = token_manager.next_renewal_at().is_some();
            let pv = profile_valid.load(Ordering::Acquire);
            // COV-R8-1: the derivation + composition live in the pure,
            // boundary-tested `token_health_sample` — never inline here.
            let (remaining_gauge, valid_gauge) = token_health_sample(has_token, remaining, pv);
            metrics::gauge!("tv_token_remaining_seconds").set(remaining_gauge);
            metrics::gauge!("tv_token_valid").set(valid_gauge);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full 8-row truth table — 1.0 iff ALL THREE inputs are true.
    #[test]
    fn test_token_valid_gauge_truth_table() {
        let rows: [(bool, bool, bool, f64); 8] = [
            (false, false, false, 0.0),
            (false, false, true, 0.0),
            (false, true, false, 0.0),
            (false, true, true, 0.0),
            (true, false, false, 0.0),
            (true, false, true, 0.0),
            (true, true, false, 0.0),
            (true, true, true, 1.0),
        ];
        for (has_token, locally_valid, profile_valid, expected) in rows {
            let got = token_valid_gauge(has_token, locally_valid, profile_valid);
            assert!(
                (got - expected).abs() < f64::EPSILON,
                "token_valid_gauge({has_token}, {locally_valid}, {profile_valid}) \
                 must be {expected}, got {got}"
            );
        }
    }

    /// The live remaining-seconds source is fail-closed: with NO token
    /// loaded, `seconds_until_expiry()` reads 0 and `next_renewal_at()`
    /// reads None — so the poller publishes remaining=0 and
    /// tv_token_valid=0.0 (never a false 1.0 before auth completes).
    #[test]
    fn test_seconds_until_expiry_fail_closed_zero_without_token() {
        let manager = TokenManager::new_for_test(None);
        assert_eq!(
            manager.seconds_until_expiry(),
            0,
            "no token loaded must read 0 remaining seconds (fail-closed)"
        );
        assert!(
            manager.next_renewal_at().is_none(),
            "no token loaded must report no expiry timestamp"
        );
        // COV-R8-1: feed the manager reads through the PRODUCTION
        // derivation (`token_health_sample`) — not a test-local re-derivation
        // — so this test exercises the same code path the poller runs.
        let has_token = manager.next_renewal_at().is_some();
        let (remaining_gauge, valid_gauge) =
            token_health_sample(has_token, manager.seconds_until_expiry(), true);
        assert!(
            (remaining_gauge - 0.0).abs() < f64::EPSILON,
            "absent token must publish tv_token_remaining_seconds = 0.0"
        );
        assert!(
            (valid_gauge - 0.0).abs() < f64::EPSILON,
            "absent token must compose to tv_token_valid = 0.0 even with profile_valid = true"
        );
    }

    /// COV-R8-1 boundary pin: a LOADED but locally-EXPIRED token
    /// (`has_token = true`, `remaining = 0`) must compose to
    /// `tv_token_valid = 0.0` even with a green profile flag. This is the
    /// mutation kill for `remaining > 0` → `remaining >= 0` — the exact
    /// one-token change that would resurrect the expired-token 1.0
    /// false-OK the module doc's headline claim rules out (audit Rule 11).
    #[test]
    fn test_token_health_sample_expired_token_boundary_is_invalid() {
        let (remaining_gauge, valid_gauge) = token_health_sample(true, 0, true);
        assert!(
            (remaining_gauge - 0.0).abs() < f64::EPSILON,
            "remaining = 0 must publish tv_token_remaining_seconds = 0.0"
        );
        assert!(
            (valid_gauge - 0.0).abs() < f64::EPSILON,
            "a loaded-but-locally-expired token (remaining = 0) must read \
             tv_token_valid = 0.0 — `remaining > 0` is strictly-greater, \
             fail-closed (COV-R8-1)"
        );
        // One second of validity flips the composite — pins the exact edge.
        let (remaining_gauge, valid_gauge) = token_health_sample(true, 1, true);
        assert!(
            (remaining_gauge - 1.0).abs() < f64::EPSILON,
            "remaining = 1 must publish tv_token_remaining_seconds = 1.0"
        );
        assert!(
            (valid_gauge - 1.0).abs() < f64::EPSILON,
            "a loaded token with 1s remaining and a green profile must read \
             tv_token_valid = 1.0"
        );
    }

    /// COV-R8-1: the remaining-seconds gauge value is the raw remaining
    /// count regardless of validity, and each AND leg independently zeroes
    /// the composite.
    #[test]
    fn test_token_health_sample_each_leg_zeroes_composite() {
        // Profile failed → invalid, but remaining still reported honestly.
        let (remaining_gauge, valid_gauge) = token_health_sample(true, 86_400, false);
        assert!((remaining_gauge - 86_400.0).abs() < f64::EPSILON);
        assert!(
            (valid_gauge - 0.0).abs() < f64::EPSILON,
            "profile_valid = false must zero tv_token_valid"
        );
        // No token → invalid even if a (stale) remaining value were fed.
        let (_, valid_gauge) = token_health_sample(false, 86_400, true);
        assert!(
            (valid_gauge - 0.0).abs() < f64::EPSILON,
            "has_token = false must zero tv_token_valid"
        );
        // All green → valid.
        let (remaining_gauge, valid_gauge) = token_health_sample(true, 86_400, true);
        assert!((remaining_gauge - 86_400.0).abs() < f64::EPSILON);
        assert!((valid_gauge - 1.0).abs() < f64::EPSILON);
    }

    /// COV-R8-1 source-scan ratchet: the PRODUCTION poller body (scanned
    /// strictly BEFORE the `#[cfg(test)]` marker so this test's own literals
    /// can never satisfy the scan — the 2026-07-06 vacuous-whole-file-scan
    /// lesson) must (a) call the pure `token_health_sample(` derivation
    /// rather than re-inlining `remaining > 0`, and (b) store BOTH gauge
    /// names — deleting either store silently regresses the gauge to the
    /// frozen mint-time renewal-loop snapshots (the round-1 incident class).
    #[test]
    fn ratchet_poller_body_uses_pure_sample_and_emits_both_gauges() {
        let src = include_str!("token_health_gauge.rs");
        let prod = &src[..src
            .find("#[cfg(test)]")
            .expect("token_health_gauge.rs must have a test module marker")];
        // The needle is concat!-split so the hooks' pub-fn source scans never
        // mistake this string literal for a second fn declaration.
        let spawn_off = prod
            .find(concat!("pub fn ", "spawn_token_health_gauge_poller("))
            .expect("the poller spawn fn must exist in the production region");
        let body = &prod[spawn_off..];
        assert!(
            body.contains("token_health_sample("),
            "the poller loop body must derive both gauge values via the pure \
             boundary-tested token_health_sample() — an inline re-derivation \
             re-opens the untested `remaining > 0` mutation hole (COV-R8-1)"
        );
        assert!(
            body.contains("\"tv_token_remaining_seconds\""),
            "the poller loop body must store the tv_token_remaining_seconds \
             gauge — deleting it regresses to the frozen mint-time snapshots"
        );
        assert!(
            body.contains("\"tv_token_valid\""),
            "the poller loop body must store the tv_token_valid gauge — the \
             AND-composed honest 0/1 signal is this module's whole purpose"
        );
    }

    #[test]
    fn test_poll_cadence_constant_is_sane() {
        // Tight enough that a killed token reads 0 within seconds…
        const _: () = assert!(TOKEN_HEALTH_GAUGE_POLL_SECS <= 60);
        // …loose enough to stay a trivial cold-path timer.
        const _: () = assert!(TOKEN_HEALTH_GAUGE_POLL_SECS >= 5);
    }
}
