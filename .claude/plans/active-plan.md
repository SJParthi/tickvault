# Implementation Plan: Dhan reconnect hardening (Fix A reconnect-in-place + Fix B short-session floor)

**Status:** VERIFIED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — "fix everything" 2026-06-30

> Crates changed: **tickvault-core** (`connection.rs`, `ConnectionHealth`),
> **tickvault-common** (`constants.rs`, `ErrorCode::WsGap09*`),
> **tickvault-app** (`main.rs` pool-watchdog Halt arm + pure classifier/ceiling
> helpers). Builds on merged `origin/main` (PR #1265 cooldown-survives-restart).

## Plan Items

- [x] Fix B — short-session first-reconnect floor (pure, tickvault-core)
  - Files: crates/common/src/constants.rs, crates/core/src/websocket/connection.rs
  - Tests: test_compute_short_session_reconnect_floor_ms_short_session_floored, test_compute_short_session_reconnect_floor_ms_long_session_zero, test_compute_short_session_reconnect_floor_ms_boundary_at_threshold, test_compute_short_session_reconnect_floor_ms_attempt_nonzero_zero, test_compute_short_session_reconnect_floor_ms_zero_uptime_floored
- [x] Fix A — ErrorCode WS-GAP-09 + runbook + triage rule (tickvault-common)
  - Files: crates/common/src/error_code.rs, .claude/rules/project/wave-2-error-codes.md, .claude/triage/error-rules.yaml
  - Tests: test_all_list_length_matches_catalogue_size, every_error_code_variant_appears_in_a_rule_file, every_error_code_variant_has_a_triage_rule
- [x] Fix A — pure reconnect-in-place classifier + ceiling helpers (tickvault-app)
  - Files: crates/app/src/main.rs
  - Tests: test_is_bare_reset_class_all_benign_true, test_is_bare_reset_class_rate_limited_false, test_is_bare_reset_class_non_reconnectable_false, test_is_bare_reset_class_token_invalid_false, test_is_bare_reset_class_questdb_down_false, test_reconnect_in_place_ceiling_under_continues, test_reconnect_in_place_ceiling_over_exits
- [x] Fix A — surface rate_limit_streak + saw_non_reconnectable on ConnectionHealth (tickvault-core)
  - Files: crates/core/src/websocket/types.rs, crates/core/src/websocket/connection.rs
  - Tests: existing pool_watchdog + types constructors updated; cargo test -p tickvault-core green
- [x] Fix A — wire classifier into the BOOT-ON + lane Halt arms of spawn_pool_watchdog_task (tickvault-app)
  - Files: crates/app/src/main.rs
  - Tests: pool_watchdog_halt_arm_gates_process_exit_on_market_hours, pool_watchdog_gates_on_runtime_dhan_enable_flag, runtime_lane_watchdog_does_not_process_exit
- [x] R1 — pin docker compose arg order (compose subcommand first) regression guard (tickvault-app)
  - Files: crates/app/src/infra.rs
  - Tests: test_docker_compose_up_args_compose_subcommand_first

## Design
Two independent fixes stop the Dhan bare-RST → 5-min-Halt → `process::exit(2)`
→ 775-SID re-subscribe → per-IP 429 restart storm (confirmed root cause: Dhan
silently RSTs the main-feed socket ~5-6s after each connect; the A4 watchdog's
`>300s all-down → process::exit(2)` in `main.rs` restarts ~27×/hr).

**Fix B (tickvault-core, pure):** a new pure
`compute_short_session_reconnect_floor_ms(attempt, session_uptime_secs,
short_session_threshold_secs, floor_ms)` raises ONLY the attempt-0 delay to
`WS_SHORT_SESSION_RECONNECT_FLOOR_MS` (3000) when the PRIOR session lived
`< WS_SHORT_SESSION_THRESHOLD_MS` (10_000ms = 10s), via
`base_delay_ms.max(...)` in `wait_with_backoff`. Session uptime comes from a new
`connected_at: AtomicI64` (epoch secs) reset to 0 at each connect attempt and
stamped on successful connect+subscribe (`connection.rs` line 1350). A
long-lived session (`>= 10s`) keeps the 0ms instant first retry. Attempts 1+
unchanged.

**Fix A (tickvault-app + tickvault-core):** at the BOOT-ON pool-watchdog Halt
arm (`main.rs` `process::exit(2)` site) consult a PURE
`is_bare_reset_class(healths, token_valid, questdb_reachable)` that returns true
iff: every connection's `rate_limit_streak == 0` AND no connection saw a
`NonReconnectableDisconnect` AND `token_valid` AND `questdb_reachable`. New
`rate_limit_streak: u32` + `saw_non_reconnectable: bool` fields on
`ConnectionHealth` carry the per-connection signals from the snapshot the
watchdog already takes. A `TokenHandle` is plumbed into
`spawn_pool_watchdog_task` for the token signal; `health.questdb_reachable()`
provides QuestDB. If bare-reset class AND the episode has spent
`< POOL_RECONNECT_IN_PLACE_CEILING_SECS` (900 = 15 min) in reconnect-in-place:
do NOT exit — reset the episode timer, `pool.reset_watchdog()`, emit WS-GAP-09
+ `tv_ws_watchdog_reconnect_in_place_total`, keep reconnecting in place
(per-connection `wait_with_backoff` + `SubscribeRxGuard` untouched). The
ceiling boundary is a pure `reconnect_in_place_ceiling_exceeded(elapsed_secs)`.
Otherwise (ceiling exceeded OR genuinely-fatal class) → exit / lane-teardown
exactly as today.

## Edge Cases
Mixed 429+reset in one cycle → streak check fails → genuine-fatal (correct;
#1265 persisted cooldown absorbs the restart). Token expiry mid-storm →
`token_valid` false → genuine-fatal. QuestDB death mid-storm →
`questdb_reachable()` false → genuine-fatal. Pre-market / Dhan-OFF → unchanged
(`should_act` already gates the whole arm). Runtime-lane (`lane_halt.is_some()`)
→ same short-circuit BEFORE lane teardown so a benign reset never tears the lane
down (Groww + shared infra untouched). Long-lived session drop → Fix B returns
0ms (instant retry preserved). `connected_at == 0` (no successful session) →
uptime 0 → floored (errs toward backing off — safe). `reset_watchdog()` restarts
the 300s AllDown window; the 15-min ceiling timer is SEPARATE, reset only when
the pool is no longer all-down (a non-Halt verdict observed).

## Failure Modes
(1) Classifier too lenient → a truly-wedged feed reconnects in place up to 15
min before restart — bounded by the ceiling, strictly degrades to today's 5-min
behaviour, never worse; **15-min ceiling FLAGGED FOR OPERATOR SIGN-OFF**.
(2) New `ConnectionHealth` fields break test constructors (`types.rs`,
`pool_watchdog.rs` test helper) → updated in-PR; `cargo test -p tickvault-core`
gate. (3) Fix B floor wrongly applied to a long session → guarded by the
`>= threshold` branch + unit test. (4) `connected_at` unset → uptime 0 → floored
(safe). (5) Watchdog reads stale `rate_limit_streak` (Acquire) → a real 429 sets
the streak before the next 5s poll; worst case one extra in-place cycle, caught
next poll.

## Test Plan
tickvault-common: WsGap09 flows `error_code_rule_file_crossref` +
`triage_rules_full_coverage_guard` + tag-guard + `test_all_list_length_*`
(109 -> 110). tickvault-core: 5 pure `compute_short_session_reconnect_floor_ms`
unit tests; `ConnectionHealth` new fields default + roundtrip;
`pool_watchdog.rs` test helper updated; existing `test_watchdog_halts_at_300s`
unchanged. tickvault-app: `is_bare_reset_class` truth table (all-benign true;
each negated input false); `reconnect_in_place_ceiling_exceeded` boundary
(under → continue, at/over → exit). Gates:
`cargo test -p tickvault-common -p tickvault-core`, banned-pattern,
pub-fn-test, pub-fn-wiring, plan-verify, plan-gate.

## Rollback
Both fixes additive and self-contained. Fix B: revert the `base_delay_ms.max(..)`
line + helper + 2 constants + `connected_at` field → instant first retry
restored. Fix A: revert the Halt-arm branch → unconditional `process::exit(2)`;
the new `ErrorCode`/counter/`ConnectionHealth` fields + plumbed `TokenHandle`
are inert if the branch is gone (drop in the same revert). No schema, no
migration, no persisted state (the 15-min timer is in-process only). `git revert`
of the single PR is clean.

## Observability
WS-GAP-09 `error!`/`warn!` with `code = ErrorCode::WsGap09WatchdogReconnectInPlace.code_str()`
on each in-place decision; counter
`tv_ws_watchdog_reconnect_in_place_total{reason="bare_dhan_reset"|"ceiling_exceeded"}`.
`tv_pool_self_halts_total` now increments only on genuine-fatal Halt (sharper
meaning — noted in runbook). Fix B: `info!` in `wait_with_backoff` gains
`session_uptime_secs` + `short_session_floor_applied`. New WS-GAP-09 section in
`wave-2-error-codes.md` + triage YAML rule (action: silence, Low). No new audit
table, no Telegram page for the benign in-place case (Severity::Low); the
genuine-fatal Halt keeps its existing `WebSocketPoolHalt` Telegram.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan bare-RST storm, token valid, QuestDB up | reconnect-in-place, no exit, WS-GAP-09, <=15min |
| 2 | Real 429 (streak>0) on any conn at Halt | genuine-fatal exit (today's behaviour) |
| 3 | Token invalid at Halt | genuine-fatal exit |
| 4 | QuestDB unreachable at Halt | genuine-fatal exit |
| 5 | Non-reconnectable Dhan code seen | genuine-fatal exit |
| 6 | In-place episode > 15 min, still zero frames | fall back to exit |
| 7 | 5s session RST | Fix B floors first reconnect to 3s |
| 8 | 5-min session drop | Fix B keeps 0ms instant first retry |
