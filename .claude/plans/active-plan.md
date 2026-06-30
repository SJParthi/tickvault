# Implementation Plan: Collapse dev/staging into a single `prod` environment (dry_run=true LOCKED)

**Status:** VERIFIED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — directive given this session 2026-06-30 ("collapse dev/staging/prod into a single prod env across the entire workspace; preserve dry_run=true / NO real orders")

> Guarantee matrices: this item carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (mandatory per `per-item-guarantee-check.sh`). The dominant guarantee for
> THIS change is the order-safety invariant: `dry_run=true` is preserved in
> every active config; no path reaches `api_client.place_order`.

## Design

The workspace currently models three environments — `dev`, `staging`, `prod`.
`staging` was the 3-month data-pull env (sandbox/dry_run, no real orders);
`dev` was local. The operator directive collapses these into a SINGLE real env,
`prod`, used everywhere — config, systemd, deploy workflows, terraform IAM,
helper-script defaults, and BOTH feeds (Dhan + Groww). `prod` becomes the only
real environment while PRESERVING the order-safety lock.

Hard safety constraint (non-negotiable): `dry_run=true` (NO real orders) MUST be
preserved. This is still the data-pull phase. `config/production.toml` currently
sets `dry_run = false` — it is changed to `dry_run = true` with a LOUD comment,
plus a far-future `sandbox_only_until` belt-and-suspenders (mirroring the deleted
staging.toml) so even a future misedit flipping `mode=live` is mechanically
blocked at boot by `StrategyConfig::check_sandbox_window`.

Semantic (not blind find-replace): only the env-SELECTION strings/defaults change.
`dev`/`staging` config branches are retired; everything repoints to `prod`.
`config/base.toml` + `config/production.toml` become the only env configs.

Changed crates: **tickvault-common** (the build-blocking `aws_infra_wiring.rs`
test must be rewritten to assert `prod` + the new `dry_run=true` invariant, plus
the `DEFAULT_SSM_ENVIRONMENT` constant) and **tickvault-core**/**tickvault-app**
(env default `dev`→`prod` in `secret_manager::resolve_environment` +
`boot_helpers::resolve_config_env`). Non-crate files: systemd unit, terraform
`main.tf` IAM grant, deploy/autopilot workflows, helper scripts, config TOMLs.

## Plan Items

- [x] `config/production.toml`: `dry_run = true` (LOUD locked comment); `[feeds] dhan_enabled=true` + `groww_enabled=true`; far-future `sandbox_only_until`; keep `ip_verification_enabled=true`, `sns_enabled`.
  - Files: config/production.toml
  - Tests: aws_infra_wiring::test_production_toml_locks_dry_run_true_no_real_orders (new)
- [x] Delete `config/staging.toml`; keep `config/local.toml` (Mac host-dev, still referenced by `CONFIG_LOCAL_PATH`).
  - Files: config/staging.toml (deleted)
- [x] Default env → `prod`: `resolve_config_env` (boot_helpers), `resolve_environment` (secret_manager), `DEFAULT_SSM_ENVIRONMENT` const (common/constants); helper-script defaults `staging`→`prod` (aws-autopilot.sh, ensure-questdb.sh, aws-autopilot.yml).
  - Files: crates/app/src/boot_helpers.rs, crates/core/src/auth/secret_manager.rs, crates/common/src/constants.rs, scripts/aws-autopilot.sh, scripts/ensure-questdb.sh, .github/workflows/aws-autopilot.yml
  - Tests: boot_helpers config_env_path tests; secret_manager resolve/validate tests (updated)
- [x] `deploy/systemd/tickvault.service`: `TV_ENVIRONMENT=staging`→`prod` + comment block rewrite.
  - Files: deploy/systemd/tickvault.service
- [x] `deploy/aws/terraform/main.tf`: drop the redundant `/tickvault/staging/*` IAM grant (already granted via `var.environment`=prod), rewrite comment.
  - Files: deploy/aws/terraform/main.tf
- [x] `.github/workflows/deploy-aws.yml`: retire the auto-seed-to-staging step (operator populates `/tickvault/prod/*` manually); repoint the config-diff + QuestDB SSM reads to prod; rewrite comments.
  - Files: .github/workflows/deploy-aws.yml
- [x] Delete the now-dead `.github/workflows/seed-staging-ssm.yml` auto-seed workflow; drop its stale reference in aws-autopilot.sh.
  - Files: .github/workflows/seed-staging-ssm.yml (deleted), scripts/aws-autopilot.sh
- [x] Rewrite `crates/common/tests/aws_infra_wiring.rs` env-string tests to assert `prod` + the new invariants; drop the two seed-staging-ssm tests.
  - Files: crates/common/tests/aws_infra_wiring.rs
  - Tests: test_terraform_instance_iam_allows_prod_ssm_prefix, test_production_toml_locks_dry_run_true_no_real_orders, test_deploy_aws_workflow_refreshes_repo_and_systemd_unit

## Edge Cases

- A box that already exported `TV_ENVIRONMENT=staging` (stale systemd unit) — the deploy refreshes the systemd unit from the repo, so after this lands the box runs `TV_ENVIRONMENT=prod` and reads `/tickvault/prod/*` (which the operator populates manually + the IAM `var.environment` grant already covers).
- `config_env_path("prod")` already maps to `production.toml` (pre-existing alias) — no new path logic needed; only the DEFAULT when the env var is unset changes.
- Path-traversal hostile env var: the strict `[a-z0-9-]` allowlist in `config_env_path` + `validate_environment` is unchanged, so the safety property is preserved.
- `local.toml`: still selected by `config_env_path` returning `None` for `local`/`dev` and merged separately via `CONFIG_LOCAL_PATH`; left intact for Mac host-dev.

## Failure Modes

- **Real orders accidentally enabled** — the whole point of the lock. Mitigated by: `production.toml dry_run=true`, `base.toml dry_run=true`, `default_dry_run()=true`, far-future `sandbox_only_until`, and the engine.rs dry_run gate that returns before any HTTP. Proven by grep + order-path trace in the PR body.
- **Box IAM-denied reading its secrets** — `var.environment`=prod already grants `/tickvault/prod/*`; removing the redundant staging grant cannot deny prod.
- **Deploy FATAL on missing `config/staging.toml`** — the deploy config-diff is repointed to `production.toml`, so deleting staging.toml does not break the deploy verification step.
- **Build break from the build-blocking `aws_infra_wiring.rs`** — rewritten in the same change so the workspace builds green.

## Test Plan

- `grep -rn 'dry_run' config/` → production.toml = true, no active `false`.
- `cargo build --workspace` → 0 errors.
- `cargo test -p tickvault-common -p tickvault-app -p tickvault-core -p tickvault-trading` (env/secret/oms/aws_infra_wiring/dry_run paths) → green; paste result lines.
- `cargo fmt --check`, banned-pattern-scanner, plan-verify, plan-gate.
- Adversarial order-safety trace: with `dry_run=true`, no config/code path under `prod` reaches `api_client.place_order`.

## Rollback

Single squash-merge PR. Revert the merge commit to restore the three-env model
(`config/staging.toml`, the seed-staging-ssm workflow, the `dev`/`staging`
defaults, and the original `production.toml dry_run=false`). No data migration,
no schema change, no stored-state dependency — pure config/deploy/test edits.

## Observability

No new runtime telemetry is required — this is a config/deploy consolidation.
Existing signals remain: `resolve_environment()` warns once if `TV_ENVIRONMENT`
is unset (now defaulting to `prod`); the OMS engine logs `dry_run=...` at daily
reset; deploy workflow steps print the deployed config diff. The order-safety
invariant is enforced mechanically by the rewritten `aws_infra_wiring.rs` test
(`test_production_toml_locks_dry_run_true_no_real_orders`) which fails the build
if `production.toml` ever sets `dry_run = false` again.

---

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
- [x] Fix A no-op close (2026-06-30) — wire REAL production QuestDB-liveness into the reconnect-in-place gate (tickvault-app + tickvault-api)
  - Files: crates/app/src/main.rs, crates/api/tests/health_questdb_reachable_wiring_guard.rs
  - Tests: test_pool_watchdog_sets_questdb_reachable_from_production, test_questdb_reachable_round_trips, pool_watchdog_halt_arm_gates_process_exit_on_market_hours

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

### Design — Fix A no-op close (2026-06-30, rework)
The shipped `is_bare_reset_class(healths, token_valid, questdb_reachable)` gate
required `questdb_reachable == true`, but the backing `questdb_reachable`
`AtomicBool` on `SystemHealthStatus` was set `true` ONLY in `#[cfg(test)]` code
— so in production `health.questdb_reachable()` was permanently `false`, the gate
always returned `false`, and the reconnect-in-place branch was DEAD (the watchdog
still exited → the 429 restart storm still happened). The rework wires a REAL
production signal: `spawn_pool_watchdog_task` now takes a `QuestDbConfig` and, on
its existing 5s tick (the same tick that already pushes
`set_websocket_connections`), probes QuestDB via the canonical
`boot_probe::wait_for_questdb_ready` wrapped in a 2s `tokio::time::timeout` and
calls `health.set_questdb_reachable(...)` with the live result. The probe runs
every 5s INDEPENDENTLY of tick flow (a Dhan bare-RST storm stops ticks while
QuestDB stays up — exactly when the gate must read it correctly). Both call sites
(`crates/app/src/main.rs` BOOT-ON fast-boot + slow-boot/lane) pass
`config.questdb.clone()`. The gate intent is preserved: reconnect-in-place engages
ONLY when QuestDB is genuinely up; the `rate_limit_streak == 0` /
`!saw_non_reconnectable` / `token_valid` conditions are untouched (429/805 still
restart by design).

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
| 1 | `TV_ENVIRONMENT` unset at boot | defaults to `prod`, reads `/tickvault/prod/*`, loads `production.toml` |
| 2 | Stale box with `TV_ENVIRONMENT=staging` unit | deploy refreshes unit → `prod` |
| 3 | Future misedit flips `mode=live` in production.toml | boot panics via `check_sandbox_window` (far-future `sandbox_only_until`) |
| 4 | Operator has not yet populated `/tickvault/prod/*` | boot halts with SecretRetrieval (honest, no silent fallback to dev) — operator is populating manually |
| 5 | Dhan bare-RST storm, token valid, QuestDB up | reconnect-in-place, no exit, WS-GAP-09, <=15min |
| 6 | Real 429 (streak>0) on any conn at Halt | genuine-fatal exit (today's behaviour) |
| 7 | Token invalid at Halt | genuine-fatal exit |
| 8 | QuestDB unreachable at Halt | genuine-fatal exit |
| 9 | Non-reconnectable Dhan code seen | genuine-fatal exit |
| 10 | In-place episode > 15 min, still zero frames | fall back to exit |
| 11 | 5s session RST | Fix B floors first reconnect to 3s |
| 12 | 5-min session drop | Fix B keeps 0ms instant first retry |
| 13 | Fix-A no-op rework: prod boot, QuestDB up | watchdog 5s probe sets questdb_reachable=true; bare-reset gate can engage |
| 14 | Fix-A no-op rework: QuestDB genuinely down at Halt | watchdog 2s probe times out → set_questdb_reachable(false) → gate false → genuine-fatal restart (correct) |
| 15 | Fix-A no-op rework: ticks stopped (RST storm) but QuestDB up | 5s probe runs independent of tick flow → signal stays fresh → gate engages |

### Edge Cases — Fix A no-op close (2026-06-30, rework)
QuestDB probe times out (hung TCP connect) → 2s `tokio::time::timeout` returns
`Err` → `is_ok_and(...)` is `false` → `set_questdb_reachable(false)` → the gate
reads QuestDB-down → genuine-fatal restart (safe, never rides out a Halt while
persistence is broken). Probe transiently fails for one 5s tick while QuestDB is
actually up → that single tick reads false; the watchdog Halt only fires after
300s all-down, by which time many 5s probes have run, so a one-off blip cannot
mis-gate. The probe builds a fresh reqwest client per call (cold path, 5s cadence,
not the hot tick path) — acceptable; mirrors the 10s SLO scheduler's existing
`wait_for_questdb_ready` usage.

### Failure Modes — Fix A no-op close (2026-06-30, rework)
(6) Probe falsely reports QuestDB up while it is down → would let the gate ride
out a Halt with broken persistence. Mitigated: the probe is a real `SELECT 1`
against the same `/exec` endpoint the SLO scheduler + boot probe use; a 2s timeout
bounds a hung connect; on ANY error/timeout the result is `false` (fail-safe toward
restart). (7) `spawn_pool_watchdog_task` signature change breaks a call site →
compile error caught at build; both sites updated in-PR. (8) Source-scan guard
`test_pool_watchdog_sets_questdb_reachable_from_production` fails the build if the
production `set_questdb_reachable` call site, the `QuestDbConfig` param, or the
`wait_for_questdb_ready` probe is removed — so the dead-signal regression cannot
recur.

### Test Plan — Fix A no-op close (2026-06-30, rework)
New guard `crates/api/tests/health_questdb_reachable_wiring_guard.rs`:
`test_questdb_reachable_round_trips` (state setter/getter honesty),
`test_pool_watchdog_sets_questdb_reachable_from_production` (source-scan: the
production call site + `QuestDbConfig` param + `wait_for_questdb_ready` probe +
the Halt arm reading `health.questdb_reachable()` into `is_bare_reset_class` all
present). Existing `is_bare_reset_class` truth-table + ceiling tests + the
`post_market_pool_halt_guard.rs` Halt-arm guards remain green. Gates: `cargo build`
+ `cargo test -p tickvault-app -p tickvault-api -p tickvault-storage`,
banned-pattern, plan-verify, plan-gate.

### Rollback — Fix A no-op close (2026-06-30, rework)
Additive + self-contained: revert the `questdb_config` parameter + the 5s probe
block + the two `config.questdb.clone()` call-site args + the new guard test →
returns to the (no-op) shipped state. No schema, no migration, no persisted state.
`git revert` of the rework commit is clean.

### Observability — Fix A no-op close (2026-06-30, rework)
No new counter/log/event — the rework only makes the EXISTING `WS-GAP-09`
reconnect-in-place gate functional in production by feeding it the live
`questdb_reachable` signal. `health.questdb_reachable()` now reflects true QuestDB
state on `/health` + in the SLO/self-test paths that read it, and the bare-reset
gate's QuestDB predicate is no longer a dead constant `false`.
