# Implementation Plan: API Bearer-Token SSM Re-Read / Rotation (W2#7)

**Status:** VERIFIED
**Date:** 2026-07-10
**Approved by:** coordinator session (wave-2 #7 of 8 directive, 2026-07-10 — audit row 13)

> **The gap (audit row 13):** the API bearer token
> (`/tickvault/<env>/api/bearer-token` in SSM) is read ONCE at boot
> (`crates/app/src/main.rs` fast-boot + slow-boot arms) and held for the
> process lifetime inside `ApiAuthConfig` — rotating the token in SSM
> requires an app restart, so in practice it never rotates.

## Design

1. **Hot-swap holder (crates/api).** `ApiAuthConfig.bearer_token` moves from
   `SecretString` to `Arc<arc_swap::ArcSwap<SecretString>>` (house auth
   pattern — `token_manager.rs` precedent): lock-free O(1) `load()` on every
   request in `require_bearer_auth`; atomic swap on refresh. All per-request
   clones of `ApiAuthConfig` share the SAME holder, so a swap is instantly
   visible to in-flight middleware state. `enabled` stays a boot-time bool
   (rotation can never enable/disable auth — swap only accepts non-empty,
   shape-valid values, so an enabled config stays enabled; a disabled config
   never spawns the reload task).
2. **Rotation API (crates/api).** `ApiAuthConfig::rotate_bearer_token(new:
   SecretString) -> TokenReloadOutcome` (`Rotated` / `Unchanged` /
   `RejectedInvalid`) built on the pure `is_valid_reload_token(&str)`
   (non-empty, ≤ 512 bytes, no whitespace/control chars) — fail-open: an
   empty/placeholder SSM value NEVER replaces the working token and NEVER
   becomes accept-all.
3. **401-triggered out-of-band re-read hint (crates/api).** The
   invalid-bearer arm (well-formed `Bearer` + mismatch ONLY — never the
   missing/malformed-header arms) calls `config.request_oob_reload(now)`,
   a CAS-gated `AtomicU64` + `tokio::sync::Notify` with a hard
   `OOB_RELOAD_FLOOR_SECS = 60` floor (pure decision fn
   `should_request_oob_reload`) — an attacker spamming bad bearers can
   trigger at most ONE SSM read per 60s; per-request cost is one atomic
   load (+ one CAS at the floor edge). O(1), no allocation.
4. **Supervised periodic re-read task (crates/app).** New module
   `crates/app/src/api_token_rotation.rs`:
   `spawn_supervised_api_token_reload(config)` (WS-GAP-05 /
   DISK-WATCHER-01 house supervisor pattern, exit classified via
   `tickvault_storage::disk_health_watcher::classify_join_exit`, 5s respawn
   backoff, `tv_api_token_reload_respawn_total{reason}`). Inner loop:
   `select!` on a `API_TOKEN_RELOAD_INTERVAL_SECS = 300` tick OR the OOB
   notify; each cycle does ONE READ-ONLY
   `fetch_api_bearer_token()` (SSM GetParameter — never a write) and swaps
   only on a valid value. SSM outage = keep current token working
   (fail-open; degraded-not-broken — deliberately NO new ErrorCode: a
   failing re-read leaves auth WORKING on the old token; visibility is the
   counter + edge-latched warn below).
5. **Wiring (crates/app/src/main.rs).** Both boot arms (fast ~2711, slow
   ~5787) spawn the supervised reload task right after constructing
   `ApiAuthConfig::from_token(...)`, gated on `enabled` (a disabled dev
   config gets no task).
6. **Honest rotation window (documented in module + PR):** operator updates
   SSM → live within ≤ 5 min (periodic) or ~60s under active mismatched
   use (OOB path). Single-value swap: the OLD token dies at the swap
   instant (no dual-accept window) — accepted per coordinator directive.

## Edge Cases

- Empty SSM value / placeholder → `RejectedInvalid`; current token keeps
  working (never blank, never accept-all).
- Whitespace/control-char/oversized (>512B) value → `RejectedInvalid`.
- Same value re-read (the common case) → `Unchanged`, `debug!` only — no
  log spam at the 5-min cadence.
- SSM outage (transient or sustained) → `failed` counter + `debug!`;
  edge-latched `warn!` once after `API_TOKEN_RELOAD_FAILURE_WARN_THRESHOLD
  = 3` consecutive failures (re-armed on the next success) — never a
  per-cycle warn storm.
- 401 spam with well-formed bearers → OOB trigger floored at 60s (CAS —
  two racing requests cannot double-trigger inside the floor).
- 401s with missing/malformed headers → NO OOB trigger (cheapest probes
  must not touch the hint path at all).
- Rotation concurrent with in-flight request → the request compares
  against whichever token the `load()` observed — atomically either old
  or new, never torn.
- Disabled auth (empty token at boot, dry-run) → no reload task spawned;
  `enabled=false` passthrough unchanged.
- Reload task panic/exit → supervisor respawns (5s backoff) with reason
  counter; token keeps serving from the holder meanwhile.
- Env-race landmine: no new env-mutating tests; existing
  `tv_api_token_prod_guard.rs` serialization untouched.

## Failure Modes

| Failure | Behaviour | Signal |
|---|---|---|
| SSM read error | keep current token (fail-open) | `tv_api_token_reloads_total{outcome="failed"}` + debug!; warn! after 3 consecutive |
| SSM returns empty/garbage | reject swap, keep current | `outcome="rejected"` + warn! (a bad SSM value is operator-actionable) |
| Reload task dies | supervisor respawn ≤ 5s | `tv_api_token_reload_respawn_total{reason}` + error-free warn! (degraded-not-broken) |
| Operator rotates SSM | new token live ≤ 5 min (or ~60s via OOB) | `outcome="ok"` + info! (one line per actual rotation) |
| Attacker 401 spam | ≥ 60s between OOB SSM reads | bounded by `should_request_oob_reload` |

## Test Plan

- `crates/api/src/middleware.rs` units: `is_valid_reload_token` (empty /
  whitespace / control / oversize / valid), `rotate_bearer_token`
  outcomes (Rotated / Unchanged / RejectedInvalid), rotation visibility
  across clones, `should_request_oob_reload` floor boundaries (0s, 59s,
  60s, 61s), OOB hint only fires from the mismatched-bearer arm.
- `crates/api/src/lib.rs` (router-level, tower oneshot): old token
  rejected after swap, new token accepted; empty-swap rejected keeps old
  token accepted (fail-open); disabled config unaffected.
- `crates/app/src/api_token_rotation.rs` units: consecutive-failure warn
  edge latch (fires once at threshold, re-arms on success), constants
  pinned (300s cadence, 3-failure threshold), reload-cycle outcome
  classification.
- NEW wiring guard `crates/app/tests/api_token_rotation_wiring_guard.rs`:
  main.rs spawns `spawn_supervised_api_token_reload(` at BOTH boot arms
  (≥ 2 call sites), the middleware mismatched-token arm calls
  `request_oob_reload`, the reload loop calls `fetch_api_bearer_token` +
  `rotate_bearer_token`, and the counter literal
  `tv_api_token_reloads_total` exists at the loop site.
- ALL existing auth tests stay green (`cargo test -p tickvault-api`,
  `-p tickvault-app`).

## Rollback

Single revert of the PR restores the boot-frozen token exactly (the
holder degenerates to the boot value if the task never swaps; reverting
removes holder + task + hint wholesale). No schema, no config, no infra
change — rollback is a pure `git revert`.

## Observability

- Counter `tv_api_token_reloads_total{outcome="ok"|"unchanged"|"failed"|"rejected"}`
  (static labels only), pre-registered at 0 for all four outcomes inside
  the reload task at spawn (post-recorder-install by construction — the
  task is spawned from main.rs after `observability::init_metrics`).
- Counter `tv_api_token_reload_respawn_total{reason}` (supervisor).
- `debug!` for routine cycles (unchanged/failed), `info!` on an actual
  rotation, edge-latched `warn!` on ≥3 consecutive read failures and on a
  rejected (invalid) SSM value. No new ErrorCode (degraded-not-broken —
  auth keeps working on the old token; justified in Design item 4).
- No new CloudWatch alarm (the failure mode does not break auth; the
  401-burst pager from W2#2 already covers the attack surface).

## Per-Item Guarantee Matrix (cross-reference)

This single-item plan carries the **15-row "100% everything" matrix** and
the **7-row Resilience Demand Matrix** by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical
tables), with the item-specific deltas:

| Matrix row (15-row) | This item's proof |
|---|---|
| 100% code coverage | new units + router tests in api/app; coverage delta ≥ 0 |
| 100% audit coverage | N/A — no SEBI-relevant event; log+counter surface only |
| 100% testing coverage | unit + integration (router oneshot) + source-scan ratchet |
| 100% code checks | all pre-commit/pre-push gates + banned-pattern clean |
| 100% code performance | not tick hot path; per-request delta = one lock-free load (O(1), zero alloc) |
| 100% monitoring | 2 counters + tracing lines (see Observability) |
| 100% logging | debug!/info!/warn! ladder; no println!; no secret ever logged |
| 100% alerting | N/A — degraded-not-broken by design (justified above) |
| 100% security | timing-safe compare intact; SecretString boundary intact; READ-ONLY SSM; security-review pass |
| 100% security hardening | OOB floor bounds SSM-amplification DoS to 1 read/60s |
| 100% bugs fixing | synchronous adversarial review (security + hostile) pre-PR |
| 100% scenarios covering | Edge Cases section — 10 enumerated |
| 100% functionalities covering | every new pub fn has test + call site |
| 100% code review | adversarial review on the diff before PR |
| 100% extreme check | wiring-guard ratchet fails build on regression |

| Matrix row (7-row) | This item's proof |
|---|---|
| Zero ticks lost | untouched — API cold path only |
| WS never disconnects | untouched |
| Never slow/locked/hanged | per-request auth read stays lock-free O(1) |
| QuestDB never fails | untouched |
| O(1) latency | ArcSwap load = O(1); swap = O(1); OOB hint = O(1) atomics |
| Uniqueness + dedup | N/A — no storage |
| Real-time proof | counters + wiring ratchet pin the chain |

## Plan Items

- [x] Item 1 — ArcSwap holder + rotate/validate + OOB hint in ApiAuthConfig
  - Files: crates/api/src/middleware.rs, crates/api/Cargo.toml
  - Tests: test_rotate_bearer_token_swaps_valid_value, test_rotate_bearer_token_rejects_empty, test_should_request_oob_reload_floor
- [x] Item 2 — supervised periodic re-read task
  - Files: crates/app/src/api_token_rotation.rs, crates/app/src/lib.rs
  - Tests: test_failure_warn_edge_latch_fires_once_at_threshold, test_reload_constants_pinned
- [x] Item 3 — main.rs wiring at both boot arms
  - Files: crates/app/src/main.rs
  - Tests: ratchet_main_spawns_reload_at_both_boot_arms
- [x] Item 4 — wiring guard ratchet
  - Files: crates/app/tests/api_token_rotation_wiring_guard.rs
  - Tests: ratchet_middleware_mismatch_arm_requests_oob_reload, ratchet_reload_loop_reads_ssm_and_rotates
- [x] Item 5 — GAP-SEC-01 rule note
  - Files: .claude/rules/project/gap-enforcement.md
  - Tests: n/a (docs)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Operator rotates SSM value | new token accepted ≤ 5 min; old token 401s after swap |
| 2 | SSM outage during cycle | old token keeps working; failed counter; warn after 3 consecutive |
| 3 | SSM returns empty string | swap rejected; old token keeps working; rejected counter + warn |
| 4 | Attacker floods bad bearers | ≤ 1 OOB SSM read per 60s; 401s unchanged; burst pager (W2#2) covers |
| 5 | Reload task panics | supervisor respawns ≤ 5s; auth unaffected |
| 6 | Dry-run disabled auth | no task spawned; passthrough unchanged |
