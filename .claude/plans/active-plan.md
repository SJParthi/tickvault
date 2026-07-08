# Implementation Plan: Silent-Feed Alerting Hardening (2026-07-06 incident)

**Status:** VERIFIED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator directive 2026-07-06)
**Changed crates:** core (`crates/core`), app (`crates/app`), common (test ratchets in `crates/common`) — plus terraform (`deploy/aws/terraform/`) and the operator-portal Lambda (`deploy/aws/lambda/operator-control/handler.py`).

> **Incident context (2026-07-06):** the Dhan feed degraded ALL day and the
> operator received ZERO pages, despite FOUR independent signals firing
> internally: (1) exchange→receive lag p99 46s / max 199s all session;
> (2) 29–67 of 776 subscribed instruments silent EVERY minute — the
> tick-gap alarm threshold of 100 was never crossed; (3) 125 SLO score
> crossings in the 0.94–0.97 band — SLO-02 has been Telegram-suppressed
> (log-only) since 2026-05-11 and the <0.80 Critical tier is mathematically
> unreachable for feed degradation (tick_freshness alone bottoms at
> 1−67/776 ≈ 0.914); (4) BOUNDARY-01 catch-up sealing ran at 9k–11.5k
> seals/10min (coalesced warn!, no page). A fifth defect compounded the
> blindness: the operator-portal lag SQL used `now()` (UTC) against
> IST-shifted `ts`, producing a ~5h40m window that conflated WAL-replay
> rows with live rows, AND its exchange-time window structurally
> lag-censored the very lag it was meant to show (Rule-11 false-OK).

## Plan Items

- [x] Add live single-writer token-health gauge poller (`tv_token_remaining_seconds` LIVE +
      `tv_token_valid` AND-composed) + pure `token_valid_gauge` fn
      — impl: `crates/core/src/auth/token_health_gauge.rs::token_valid_gauge` (:81) +
      `spawn_token_health_gauge_poller`; exported via `crates/core/src/auth/mod.rs`
  - Files: crates/core/src/auth/token_health_gauge.rs, crates/core/src/auth/mod.rs
  - Tests: test_token_valid_gauge_truth_table, test_seconds_until_expiry_fail_closed_zero_without_token, test_poll_cadence_constant_is_sane
- [x] Add O(1) `TokenManager::dual_instance_lock_held(&self) -> Option<bool>` accessor (reuse
      existing `instance_lock_held` field; do NOT fork mint/renew)
      — impl: `crates/core/src/auth/token_manager.rs::dual_instance_lock_held`
  - Files: crates/core/src/auth/token_manager.rs
  - Tests: test_dual_instance_lock_held_mirrors_constructor_flag
- [x] Add pure decision fns + counter/latch fields to the mid-session watchdog and wire the
      forced re-mint via existing `force_renewal()` (lock-before-mint, retry-once, cooldown)
      — impl: `crates/core/src/auth/mid_session_watchdog.rs::{decide_remint, cycle_outcome,
      next_consecutive_failures, apply_remint_decision}` + the `force_renewal(` call site
  - Files: crates/core/src/auth/mid_session_watchdog.rs
  - Tests: decide_remint_below_threshold_waits, decide_remint_latch_blocks_second_attempt, decide_remint_lock_lost_beats_cooldown, decide_remint_holds_inside_cooldown_when_lock_held, decide_remint_triggers_at_threshold_with_lock_and_cooldown_ok, next_consecutive_failures_ok_resets_to_zero
- [x] Add ONE HIGH `NotificationEvent::TokenForcedRemintTriggered` (4 match arms: formatter,
      event_kind, severity High, feed_badge Dhan)
      — impl: `crates/core/src/notification/events.rs::TokenForcedRemintTriggered` (:566, 4 arms)
  - Files: crates/core/src/notification/events.rs
  - Tests: test_token_forced_remint_triggered_event
- [x] Add `ErrorCode::AuthGap05ForcedRemintTriggered` + rule-file runbook section
      — impl: `crates/common/src/error_code.rs::AuthGap05ForcedRemintTriggered` (:297,
      code_str "AUTH-GAP-05") + `.claude/rules/project/wave-4-error-codes.md` §AUTH-GAP-05
  - Files: crates/common/src/error_code.rs, .claude/rules/project/wave-4-error-codes.md
  - Tests: existing error_code meta-tests + crossref + tag_guard (tickvault-common suite)
- [x] Add named constants (poll cadence, consecutive threshold); reuse DHAN_TOKEN_GENERATION_COOLDOWN_SECS
      — impl: `CONSECUTIVE_INVALID_REMINT_THRESHOLD` (mid_session_watchdog.rs:87) +
      `TOKEN_HEALTH_GAUGE_POLL_SECS` (token_health_gauge.rs:74); cooldown constant reused from
      crates/common/src/constants.rs (no change needed there)
  - Files: crates/core/src/auth/mid_session_watchdog.rs, crates/core/src/auth/token_health_gauge.rs
  - Tests: remint_threshold_constant_is_sane, test_poll_cadence_constant_is_sane
- [x] Wire poller + extended watchdog signature into boot; seed tv_token_valid at Step 6
      — impl: `crates/app/src/main.rs` `spawn_token_health_gauge_poller(` call sites (slow lane +
      FAST crash-recovery arm) + extended `spawn_mid_session_profile_watchdog` wiring
  - Files: crates/app/src/main.rs
  - Tests: (covered by ratchets below)
- [x] Fixer round 1: lane-own the gauge poller + mid-session watchdog (DhanLaneRunHandles
      fields, teardown step-0 abort + honest 0/0.0 gauge reset, H8 Drop abort); spawn the
      gauge poller on the FAST crash-recovery arm; 4-way status-aware `cycle_outcome`
      (`RestSurfaceDegraded` never escalates to a mint); prefix-anchored transient classifier;
      de-vacuate the decide_remint ratchet (production-region scan)
      — impl: `crates/app/src/main.rs::DhanLaneRunHandles::{token_health_gauge_handle,
      mid_session_watchdog_handle}` + `teardown_dhan_lane_tasks` step-0 abort;
      `mid_session_watchdog.rs::CycleOutcome::RestSurfaceDegraded`
  - Files: crates/app/src/main.rs, crates/core/src/auth/mid_session_watchdog.rs,
    crates/core/src/auth/token_health_gauge.rs, crates/core/src/auth/secret_manager.rs
  - Tests: cycle_outcome_http_5xx_is_rest_surface_degraded, cycle_outcome_http_400_and_429_are_rest_surface_degraded, cycle_outcome_hostile_401_body_with_transient_needles_is_real_auth_fail, hostile_body_embedding_send_leg_wrapper_is_not_transient, rest_outage_two_cycles_never_reaches_remint_threshold, apply_cycle_outcome_rest_degraded_leaves_state_and_flag_untouched, dhan_lane_run_handles_drop_aborts_handles
- [x] Source-scan ratchets: re-mint call site exists + poller wired into main
      — impl: `crates/core/src/auth/secret_manager.rs` tests module (production-region scan)
  - Files: crates/core/src/auth/secret_manager.rs
  - Tests: test_mid_session_remint_trigger_call_site_exists, test_token_health_gauge_poller_wired_into_main
- [x] Re-applied continuation-review fixes (2026-07-08, adjudicated findings — disk-rollback
      recovery). F4: market-open one-shots (09:14/09:15:30/09:16) once-per-process latched
      (`MARKET_OPEN_ONE_SHOTS_SPAWNED`) + fire-time dhan-enabled gates
      (`market_open_fire_gate_dhan_enabled`) + FirstSeenSet midnight-reset latch
      (`FIRST_SEEN_RESET_SPAWNED`). F5: runtime IP monitor lane-owned (mem::forget removed;
      guard-wrapped, defused into `DhanLaneRunHandles.ip_monitor_handle/_shutdown`, stopped by
      teardown/Drop). F12: needle-miss SEND-LEG profile failures classify RestSurfaceDegraded
      (never walk the mint counter). F13: slow-boot heartbeat watchdog lane-owned
      (`heartbeat_watchdog_handle`). F14: teardown bounded-joins the pool watchdog then publishes
      honest lane-off `/health` ws count 0 + feed-health Dhan disconnected (Drop mirrors);
      teardown budget const updated (4 joins, 42s ≤ 45s). F15: `/health token_valid` honors the
      profile-truth flag via pure `token_health_writer_valid`. F8: ONE shared comment-aware
      production-region helper (`tickvault_common::source_scan`) used by all region ratchets
      (covers inline #[cfg(test)] attrs, doc-comment mentions, AND main.rs's trailing production
      code after `mod tests`). F9/F17: comment-stripped teardown abort→join→reset ordering
      ratchet. F10: InstanceLockHeartbeatGuard defuse semantics behaviourally tested. F19:
      cancel-cleanup comments enumerate the REAL residual detached tasks.
  - Files: crates/common/src/source_scan.rs, crates/common/src/lib.rs,
    crates/core/src/auth/mid_session_watchdog.rs, crates/core/src/auth/secret_manager.rs,
    crates/app/src/main.rs, crates/api/tests/token_headroom_wired_guard.rs,
    .claude/rules/project/wave-4-error-codes.md
  - Tests: test_market_open_one_shots_latched_and_gated,
    test_ip_monitor_and_heartbeat_watchdog_are_lane_owned,
    cycle_outcome_send_leg_without_transient_needle_is_rest_surface_degraded,
    send_leg_needle_miss_two_cycles_never_reach_remint_threshold,
    instance_lock_heartbeat_guard_defuse_and_drop_semantics,
    test_token_health_writer_valid_honors_profile_truth,
    test_market_open_fire_gate_reads_dhan_flag,
    ratchet_teardown_abort_join_reset_ordering_comment_stripped,
    production_region_keeps_trailing_code_after_mod_tests,
    strips_line_and_doc_comments_but_keeps_strings
