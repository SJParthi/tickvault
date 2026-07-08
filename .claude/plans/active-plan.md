# Implementation Plan: Dhan token self-heal — forced mid-session re-mint + honest token-health gauges + one HIGH event

**Status:** VERIFIED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator directive 2026-07-06)

> Crates changed: **tickvault-core** (`crates/core/src/auth/*`, `crates/core/src/notification/events.rs`),
> **tickvault-common** (`crates/common/src/error_code.rs`, `crates/common/src/constants.rs`),
> **tickvault-app** (`crates/app/src/main.rs`).
> A `crates/common/` change escalates the local test scope to `cargo test --workspace`
> per `.claude/rules/project/testing-scope.md`.
>
> Guarantee matrices: this plan carries + cross-references the 15-row + 7-row matrix in
> `.claude/rules/project/per-wave-guarantee-matrix.md` and the GUARANTEE CHECK in
> `.claude/rules/project/zero-loss-guarantee-charter.md` §3. Full synthesized contract:
> scratchpad `token-selfheal-design.md`.
>
> Out of scope (operator lock): `crates/core/src/websocket/order_update_connection.rs` (separate PR)
> and ALL terraform / CloudWatch wiring — NOT touched.

## Design

On sustained mid-session token-invalid detection — **2 consecutive REAL `/v2/profile` auth
failures** observed by the existing 900s market-hours-gated mid-session profile watchdog
(`crates/core/src/auth/mid_session_watchdog.rs`) — trigger exactly **ONE forced token re-mint
per failing episode** through the EXISTING `TokenManager::force_renewal()` machinery
(`crates/core/src/auth/token_manager.rs:1200` → `renew_with_fallback` = GET `/v2/RenewToken`
then fallback POST `generateAccessToken`). No fork of TokenManager; no new mint engine.

Lock-before-mint is operator law: the re-mint is refused **fail-closed** when the dual-instance
lock is not held. A new O(1) read-only accessor `TokenManager::dual_instance_lock_held(&self)
-> Option<bool>` (delegating to the existing private `instance_lock_held` field @ `:114`) feeds
a **pure** decision fn `decide_remint(...)` so the refusal is decided BEFORE any network call
(zero external side effects). Defense-in-depth: `acquire_token`'s own
`mint_refused_by_instance_lock` tripwire (`token_manager.rs:701/:1304`) still refuses inside
`force_renewal`, classified PERMANENT so the loop fails fast. The ~125s Dhan mint cooldown
(`DHAN_TOKEN_GENERATION_COOLDOWN_SECS = 125`) is honored by a pure `cooldown_elapsed` input
plus the once-per-episode latch; DH-901 law (rotate → retry ONCE → HALT) stays intact — the
BOOT-path HALT is untouched, and the mid-session path substitutes the pre-existing CRITICAL
`MidSessionProfileInvalidated` page for HALT because a mid-session process HALT would drop the
live WS feed (the watchdog's standing contract, docstring `:35-43`).

Two honest gauges are published by a 15s poller (new module
`crates/core/src/auth/token_health_gauge.rs`), eliminating the past-local-expiry false-OK
(the two pre-existing renewal-loop snapshot writes at `:929`/`:959` REMAIN — three writers
total for `tv_token_remaining_seconds`, honestly documented in the module header; the poller
overwrites within one 15s cadence so no false-OK results):
- `tv_token_remaining_seconds` = LIVE `expiry_ts − now` via the existing O(1) arc-swap
  `TokenManager::seconds_until_expiry()` (`:1104`), REPLACING the frozen mint-time constant
  (today emitted only twice per ~23h renewal loop at `:929`/`:959`, which stay as harmless
  augment).
- `tv_token_valid` = AND-composed `token_valid_gauge(has_token, locally_valid, profile_valid)`:
  1.0 iff a token is loaded AND not past local expiry AND the watchdog's last `/v2/profile`
  check did not fail (shared `Arc<AtomicBool>`). 0.0 on DH-901/DH-906 (profile) OR past local
  expiry (poller) OR no token. This satisfies the operator scope ("1.0 on `/v2/profile`
  success, 0.0 on DH-901/DH-906") and additionally never shows a stale 1.0 on a
  killed-but-not-yet-expired or locally-expired token (audit Rule 11 — no false-OK).

One typed HIGH `NotificationEvent::TokenForcedRemintTriggered { consecutive_checks, check_interval_secs }`
fires edge-triggered ONCE per episode (guaranteed by the `remint_attempted_this_episode` latch),
inherently market-hours-aware (the whole watchdog loop is gated). Telegram text follows the 10
plain-English commandments and NEVER contains the JWT. A new `ErrorCode::AuthGap05ForcedRemintTriggered`
(`code_str "AUTH-GAP-05"`, `Severity::High`) is added with a runbook mention in
`.claude/rules/project/wave-4-error-codes.md` (required by `error_code_rule_file_crossref.rs` +
`error_code_tag_guard.rs`).

**Lane ownership (fixer round 1, 2026-07-06 — AG5-R1-1/SEC-R1-1/SEC-R1-2/CPLX-1/EDGE-1):** the
gauge poller AND the (now remint-capable) mid-session watchdog are LANE-OWNED — their
`JoinHandle`s ride in `DhanLaneRunHandles` (`token_health_gauge_handle` /
`mid_session_watchdog_handle`), are aborted by both `teardown_dhan_lane_tasks` step 0 and the
H8 `Drop` floor, and teardown publishes the honest lane-off gauge state
(`tv_token_remaining_seconds = 0.0`, `tv_token_valid = 0.0`) mirroring the /health token-block
reset. This closes: (a) the false-OK `tv_token_valid = 1.0` for up to ~24h after a deliberate
runtime Dhan disable; (b) duplicate last-writer-wins pollers after disable→enable; (c) the
leaked watchdog's dead-token `/v2/profile` traffic + false RESILIENCE-01 pages per lane cycle;
(d) the C4 lane-TokenManager-ownership violation. **Fast-arm coverage (EDGE-2):** the FAST
crash-recovery boot arm ALSO spawns the gauge poller (inert `profile_valid = true` — no
watchdog on that arm; local-expiry gate keeps the composite honest), threaded through
`run_shutdown_fast` into `DhanLaneRunHandles`.

**Status-aware classification (fixer round 1 — AG5-R1-3/EDGE-3/SEC-R1-3):** `cycle_outcome`
is a 4-way classifier: `Ok` / `RealAuthFail` (HTTP 401/403 parsed from the FIXED
`profile request HTTP {status}` wrapper prefix, dataPlan inactive, token expiry) /
`RestSurfaceDegraded` (HTTP 400/429/5xx/WAF + 200-with-unparseable-body — keeps the
pre-existing CRITICAL page but NEVER walks the re-mint counter: a pure REST outage like the
2026-06-10 all-400s day must never destroy the token the healthy live WS is riding) /
`Transient` (send-leg network blip). All wrapper matching is PREFIX-anchored on the
Display-prefix-stripped reason core — never a `contains` scan over text that concatenates the
server response body (SEC-R1-3: a hostile 401 body could otherwise forge the send-leg wrapper
+ a transient needle and silently disable both the page and the self-heal).

## Edge Cases

- **Below threshold (1 real failure):** `decide_remint` → `Wait`. No mint, no event. A single
  Dhan-side blip never mints (ping-pong avoidance).
- **Transient network blip between two real failures:** `cycle_outcome` = `Transient` → counter
  unchanged, `profile_valid` unchanged, `tv_token_valid` never falsely 0.0, no re-mint. A
  DNS/reset/timeout flap is not evidence either way.
- **Already re-minted this episode:** `decide_remint` → `AlreadyAttempted` (DH-901 retry-once).
  No second mint until a clean `Ok` resets the episode.
- **Dual-instance lock lost mid-session (peer took the SSM lock):** `lock_held == Some(false)` →
  `RefuseLockLost` — NEVER calls `force_renewal` (zero external side effects); emits
  RESILIENCE-01-tagged error + `refused_lock_lost` counter; latch set to avoid per-cycle
  re-eval. The peer's active token is never destroyed. `RefuseLockLost` is checked BEFORE
  cooldown so the safety refusal can never be masked by a cooldown hold (pinned by a test).
- **Fast-boot crash-recovery arm (`instance_lock_held = None`):** `dual_instance_lock_held()` →
  `None` → treated as held (tripwire disarmed) — exactly as strong as the existing
  `acquire_token` gate on that arm (documented residual per dual-instance-lock §3, unchanged).
- **Dhan ~125s mint cooldown:** normally moot (900s cadence) but `cooldown_elapsed` hard-stops
  any sub-125s re-mint; `HoldCooldown` counter increments.
- **Off-hours / non-trading day:** whole loop `continue`s → no re-mint, no event; `tv_token_valid`
  stays honest via the poller's local-expiry gate (goes 0.0 if the token locally expires even
  with no profile check).
- **Boot before first watchdog cycle:** `profile_valid` seeds `true`; the poller's local-expiry
  gate still forces 0.0 for an absent/expired token — no false 1.0.

## Failure Modes

- **Re-mint does not restore validity (dead dataPlan / suspended account / wrong SSM TOTP):**
  `force_renewal` returns Err or the next cycle is still invalid → `remint_attempted_this_episode`
  blocks a second mint (retry-once); the existing CRITICAL `MidSessionProfileInvalidated` keeps
  paging; the 23h `renewal_loop` 5-cycle circuit breaker still governs the background path. No
  mid-session HALT.
- **Stale-true lock flag race:** watchdog reads `Some(true)` but the heartbeat is flipping it
  false → `acquire_token`'s internal `mint_refused_by_instance_lock` tripwire refuses inside
  `force_renewal` (permanent) → classified permanent → no retry → `refused_lock_lost` counter.
- **RenewToken succeeds but Dhan still rejects next cycle (propagation lag):** counter keeps
  climbing but the episode latch prevents a mint storm; a clean `Ok` resets.
- **Poller panic:** the `tokio::spawn` wrapper is TEST-EXEMPT; a panic loses only the live
  gauge, not the token or the feed. The frozen renewal-loop emits at `:929/:959` remain as a
  coarse fallback.
- **JWT leak:** impossible by construction — event fields are `u32`/`u64` only; all `error!`/
  `info!` carry counts, never the token; `Secret<String>` preserved.

## Test Plan

Pure decision fns (unit, no I/O — the tested heart):
- `decide_remint`: below threshold → `Wait`; threshold+latch → `AlreadyAttempted`;
  threshold+`lock=Some(false)` → `RefuseLockLost` EVEN WHEN `cooldown_elapsed=false` (lock beats
  cooldown); threshold+lock ok+`!cooldown_elapsed` → `HoldCooldown`; threshold+lock ok+cooldown
  ok → `Trigger`; `lock=None` treated as held → `Trigger`.
- `cycle_outcome`: `Ok(())` → `Ok`; transient wrapper (dns/reset/timeout, reuse
  `is_transient_network_failure` strings) → `Transient`; real HTTP 401/403/dataPlan-Inactive →
  `RealAuthFail`; HTTP 400/429/5xx + parse error → `RestSurfaceDegraded` (pages, never mints);
  hostile-401-body forging send-leg wrapper + transient needle → still `RealAuthFail`
  (SEC-R1-3 prefix-anchoring regression test).
- `next_consecutive_failures`: `Ok`→0, `RealAuthFail`→prev+1 (saturating),
  `Transient`/`RestSurfaceDegraded`→unchanged; properties: a transient between two real
  failures still reaches 2; two 503 cycles never reach the re-mint threshold.
- `token_valid_gauge`: full 8-row truth table — 1.0 iff all three true, else 0.0.
- `dual_instance_lock_held`: constructor with lock flag `Some(true)`→`Some(true)`,
  `Some(false)`→`Some(false)`, `None`→`None`.

Event/error-code:
- `TokenForcedRemintTriggered`: `severity()==High`; `event_kind()=="TokenForcedRemintTriggered"`;
  `feed_badge()` leads with Dhan; formatter contains "AUTH-GAP-05" + the consecutive count +
  a leading status word; contains NO `eyJ`/JWT/file-path substring (10-commandments assertion).
- `AuthGap05ForcedRemintTriggered`: unique `code_str`, `from_str` roundtrip, `Severity::High`,
  non-empty `runbook_path` resolving on disk (covered by existing `error_code.rs` meta-tests +
  cross-ref guard once the rule-file section lands).

Ratchets (source-scan, `crates/core/src/auth/secret_manager.rs` tests module):
- `test_mid_session_remint_trigger_call_site_exists`: the PRODUCTION region of
  `mid_session_watchdog.rs` (split at `#[cfg(test)]` — COV-1: a whole-file scan is vacuous
  against the fn definition + test literals) contains `force_renewal(` AND `match decide_remint(`.
- `test_token_health_gauge_poller_wired_into_main`: `crates/app/src/main.rs` contains
  `spawn_token_health_gauge_poller(`.

Integration (pure-driven, no network): `Ok` resets counter+latch+`profile_valid=true`;
real-fail increments+`profile_valid=false`; transient leaves all unchanged; existing
transient/edge watchdog tests stay green after `classify_check_result` delegates to `cycle_outcome`.

## Rollback

Additive + config-free: the re-mint arm is reached only on `decide_remint == Trigger`. No
QuestDB table, no DEDUP key, no boot-gate, no terraform, no migration is touched, so a straight
`git revert` of the single PR fully restores the prior state. Short of a revert, the behavior is
inert outside market hours and below the 2-consecutive threshold; removing the poller spawn line
in `crates/app/src/main.rs` reverts to the frozen-gauge behavior with zero data-model impact.
The new gauges auto-register on first emit (no `observability.rs` change) and are locally
scrapeable only — nothing downstream depends on them yet.

## Observability

- Gauge `tv_token_remaining_seconds` (LIVE, 15s) — makes the existing token-expiry
  CloudWatch/alerts rules actionable against a killed token (terraform unchanged).
- Gauge `tv_token_valid` (0/1, AND-composed) — honest killed-token signal; NO false-OK.
- Counter `tv_token_forced_remint_total{outcome=triggered|ok|failed|refused_lock_lost|cooldown}`.
- `error!(code = ErrorCode::AuthGap05ForcedRemintTriggered.code_str(), consecutive = ...)` on
  Trigger; `info!` on success; `error!(code = Resilience01DualInstanceDetected.code_str())` on
  RefuseLockLost — all counts only, never the JWT.
- ONE HIGH `NotificationEvent::TokenForcedRemintTriggered` per episode → Telegram (Dhan badge,
  plain English). Existing CRITICAL `MidSessionProfileInvalidated` edge unchanged.
- Runbook: `.claude/rules/project/wave-4-error-codes.md` §AUTH-GAP-05.

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
