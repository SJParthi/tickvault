# Implementation Plan: Groww Scale-Fleet Session-B Fixes (dual-instance lock + SDK pin + DEDUP-claim correction)

**Status:** APPROVED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator, 2026-07-04 — "fixing and working?" go-ahead on the Session-B fix plan, fixes #1+#2)

> Crates changed: `tickvault-common` (crates/common), `tickvault-core` (crates/core),
> `tickvault-app` (crates/app). Guarantee matrices: this plan carries the
> canonical 15-row + 7-row matrices by cross-reference to
> `per-wave-guarantee-matrix.md` (Per-Item Guarantee Matrix, see below).

## Design

**Fix #2 (small, first).** (a) The Groww Python sidecar's wire protocol is the
`growwapi==1.5.0` wheel — the native proto/subject extraction and the Session-B
verdict both pin to that exact version, but NO Rust ratchet asserts it, so a
silent SDK bump (wire-protocol change) passes every test. Extend the existing
requirements-manifest ratchet pattern in
`crates/common/tests/groww_no_mint_guard.rs` (which already reads
`scripts/groww-sidecar/requirements.txt`) with a sibling test
`test_requirements_pins_growwapi_sdk_version` asserting the exact line
`growwapi==1.5.0` plus exact-pin (`==`) discipline for boto3. (b) The refuted
shard-overlap DEDUP claim — "the `ticks` DEDUP key … still prevents duplicate
rows" — is REWRITTEN to the truth in BOTH places it appears:
`crates/common/src/error_code.rs` (the `GrowwScale03ShardOverlap` doc comment)
and `.claude/rules/project/groww-scale-error-codes.md` §3. Truth: `capture_seq`
is globally unique and IN the key `(ts, security_id, segment, capture_seq,
feed)`, so two connections streaming the SAME instrument produce DISTINCT
`capture_seq` values → DEDUP does NOT collapse them → overlap = silent
duplicate rows. The cut-time fail-closed cutter (`cut_shards` disjointness +
coverage assertion) is the actual protection; the runtime duplicate-SID
detector is PR-2 pending.

**Fix #1 (the lock).** The Groww multi-connection scale fleet
(`make scale-test` / `scale-smoke` / any boot with
`feeds.groww.scale.enabled=true`) has NO dual-instance protection — the
RESILIENCE-01 SSM lock is acquired only inside `start_dhan_lane`, and
scale-test boots run `dhan_enabled=false`. Two hosts scaling the SAME Groww
account masquerade as provider throttle. Fix: a **Groww-fleet dual-instance
SSM lock** gating the scale-fleet spawn, REUSING the
`crates/core/src/instance_lock.rs` machinery via a lock-NAME knob:

- `instance_lock.rs` gains `compute_named_lock_path(env, lock_name)` (the
  existing `compute_instance_lock_path` delegates to it with the unchanged
  `"instance-lock"` name — Dhan path output byte-identical), the constant
  `GROWW_SCALE_FLEET_LOCK_NAME = "instance-lock-groww-scale"` (deliberately
  OUTSIDE the banned `/tickvault/<env>/groww/*` namespace — token-minter
  lock untouched), named variants `try_acquire_named_lock` /
  `renew_named_lock` / `release_named_lock` (the existing Dhan pub fns
  become thin wrappers over shared path-based private helpers), and a
  generic `spawn_named_lock_heartbeat` (Dhan's
  `spawn_instance_lock_heartbeat` is left byte-identical; the generic
  heartbeat takes the loss `ErrorCode` as a parameter).
- New app module `crates/app/src/groww_scale_lock.rs`:
  `acquire_groww_scale_fleet_lock()` builds the SSM client
  (`create_ssm_client_public`), resolves env (`resolve_environment`),
  attempts the named lock with a bounded 3-attempt / 2s-4s exponential
  retry (mirror of the Dhan Step 6a-prime policy), and returns
  `Acquired(guard with heartbeat)` or `SkipFleet` (AlreadyHeld OR SSM
  unavailable after retries — fail-closed, never silently proceed). Every
  refusal emits `error!(code = "GROWW-SCALE-05")` — the 5-sink chain
  (Telegram via ERROR routing); the deferred-notifier slot is not yet
  filled at this boot stage, so the typed NotificationEvent path is not
  reachable here (noted honestly).
- Gate site: `crates/app/src/main.rs` — the `groww_scale_enabled` boolean
  (already the preflight fail-closed gate) gains a third stage: preflight
  pass → lock acquire; on SkipFleet the boot falls back to the
  SINGLE-CONNECTION Groww path (identical fallback semantics to a failed
  preflight — capture continues; only the multi-conn fleet is refused).
  `groww_scale_ladder.rs` is NOT touched (permanent-local-branch
  divergence constraint).
- New ErrorCode variant `GrowwScale05DualFleetDetected`
  (`"GROWW-SCALE-05"`, `Severity::Critical`, runbook
  `.claude/rules/project/groww-scale-error-codes.md`,
  `is_auto_triage_safe = false` via the Critical rule), wired into
  code_str / severity / runbook_path / all() (+ catalogue count 128→129).
- Rule-file updates: new §4b in `groww-scale-error-codes.md` documenting
  GROWW-SCALE-05 (trigger / triage / honest envelope) + an additive dated
  note in `dual-instance-lock-2026-07-04.md` §3 honest envelope that the
  Groww fleet now has its own lock.

## Edge Cases

- **Crashed peer holder:** the 90s TTL stale-takeover in
  `try_acquire_named_lock` (same `LockValue::is_stale` path as Dhan)
  auto-clears — no operator flag needed.
- **Empty/whitespace env:** `compute_named_lock_path` inherits the
  `sanitize_ilp_symbol` + `"unknown"` fallback from the existing path fn.
- **Lock name / env injection:** both components are sanitized; the groww
  fleet lock path can never contain `/groww/` (unit-tested).
- **scale.enabled=false (default boot):** ZERO new SSM calls — the lock is
  attempted only after cfg.enabled && preflight pass (byte-identical
  default boot).
- **Dhan + Groww-scale both ON:** two INDEPENDENT locks at two distinct SSM
  paths — neither can stomp the other (distinct parameter names).
- **Shutdown:** the heartbeat guard is held for the life of `main`; on
  process death without notify, the SSM parameter goes stale within the
  90s TTL and the next boot takes over (documented honest envelope; the
  Dhan lock has an explicit shutdown chain, the fleet lock relies on TTL).
- **Lock lost mid-run (foreign takeover):** the generic heartbeat logs
  `error!(code = GROWW-SCALE-05)` + flips its held flag and exits — the
  fleet is NOT torn down mid-run (documented residual; the peer's ladder
  will collide visibly rather than silently, and the operator is paged).

## Failure Modes

- **AlreadyHeld (fresh peer):** fleet spawn SKIPPED, single-conn fallback,
  `error!(code = GROWW-SCALE-05, peer = holder)` pages Critical. App keeps
  running.
- **SSM transport error:** bounded 3-attempt retry (2s / 4s backoff —
  mirror of the Dhan Step 6a-prime budget) then FAIL-CLOSED: fleet
  SKIPPED + same Critical page. Never silently proceeds to a multi-conn
  fleet it cannot prove is unique.
- **Env resolution failure:** treated as SSM-unavailable → fail-closed
  SkipFleet + page.
- **Corrupt lock JSON in SSM:** `AlreadyHeld{holder:"(corrupt-json: …)"}`
  (inherited fail-closed behavior) → SkipFleet; operator cleans via
  `aws ssm delete-parameter` or the 90s TTL never applies (corrupt =
  unparseable heartbeat) — documented in the runbook section.
- **Heartbeat renewal transient error:** WARN + retry next 30s tick (90s
  TTL gives 3 attempts) — inherited semantics.

## Test Plan

- `crates/common/tests/groww_no_mint_guard.rs::test_requirements_pins_growwapi_sdk_version`
  — asserts `growwapi==1.5.0` exact line + boto3 stays exact-pinned.
- `crates/common/src/error_code.rs::tests::test_groww_scale_05_dual_fleet_contract`
  — code_str roundtrip, Severity::Critical, NOT auto-triage-safe, runbook
  path exists on disk, listed in `all()`; catalogue-count test bumped
  128→129.
- `crates/core/src/instance_lock.rs` unit tests:
  `test_compute_named_lock_path_groww_scale`,
  `test_groww_scale_lock_name_is_outside_groww_namespace`,
  `test_compute_named_lock_path_sanitises_env`,
  `test_named_lock_path_dhan_name_matches_legacy_path` (byte-identical
  Dhan path), smoke tests for `try_acquire_named_lock` /
  `renew_named_lock` / `release_named_lock` /
  `spawn_named_lock_heartbeat` (house pattern — end-to-end is
  TEST-EXEMPT, real SSM required).
- `crates/app/src/groww_scale_lock.rs` unit tests: backoff formula
  (2s/4s), max-attempts pin (3), `fleet_gate_allows` decision
  (Acquired→true, AlreadyHeld→false), smoke pin of the async fn.
- Wiring ratchet:
  `crates/core/src/auth/secret_manager.rs::tests::test_groww_scale_fleet_lock_is_wired_into_main`
  — main.rs must call `acquire_groww_scale_fleet_lock(` in the scale gate.
- Scoped runs: `cargo test -p tickvault-common --lib` + the touched
  integration tests (`groww_no_mint_guard`, `error_code_rule_file_crossref`,
  `error_code_tag_guard`), `cargo test -p tickvault-core --lib`,
  `cargo test -p tickvault-app --lib`.

## Rollback

- Fix #2: revert the single test + the two doc-text edits (no runtime
  behavior).
- Fix #1: revert the main.rs gate stage (the `groww_scale_enabled` bool
  returns to preflight-only), delete `crates/app/src/groww_scale_lock.rs` +
  its lib.rs registration, revert the instance_lock.rs named-lock additions
  (the Dhan wrappers delegate — reverting restores the original bodies),
  remove the `GrowwScale05DualFleetDetected` variant + rule-file section
  (catalogue count back to 128). No QuestDB schema, no config key, no
  Dhan-path behavior is touched, so rollback is a pure `git revert`.

## Observability

- `error!(code = "GROWW-SCALE-05", severity = "critical", peer/error, …)` on
  every fleet refusal → 5-sink chain (stdout / app.log / errors.log /
  errors.jsonl / Telegram+SNS ERROR routing).
- `info!` on successful fleet-lock acquisition with path + ttl.
- Heartbeat lifecycle logs inherited from the instance_lock module
  (acquired / stale-takeover / renewal-failure WARN / loss ERROR).
- Runbook: `groww-scale-error-codes.md` §4b (triage: identify the peer
  host_id, stop it or let TTL clear; a repeating GROWW-SCALE-05 during a
  scheduled Mac scale test means the AWS box is also scale-enabled — fix
  config, not the lock).
- Counter deliberately NOT added: the refusal is a once-per-boot Critical
  page, not a rate signal (edge-triggered by construction — one boot, one
  decision).

## Per-Item Guarantee Matrix

Carried by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
(the canonical 15-row 100% Guarantee Matrix + 7-row Resilience Demand
Matrix apply to every item below; this plan's specific proofs are the Test
Plan section above). Honest 100% claim: 100% inside the tested envelope,
with ratcheted regression coverage — the lock's pure logic (path format,
staleness, backoff, gate decision) is unit-tested; the SSM round-trip is
TEST-EXEMPT (real AWS endpoint) exactly like the existing Dhan lock; zero
hot-path impact (boot-time cold path only); no new tick-drop path; DEDUP
keys untouched.

## Plan Items

- [x] Item 1 — Pin growwapi SDK version in the requirements ratchet
  - Files: crates/common/tests/groww_no_mint_guard.rs
  - Tests: test_requirements_pins_growwapi_sdk_version
- [x] Item 2 — Correct the refuted shard-overlap DEDUP claim
  - Files: crates/common/src/error_code.rs, .claude/rules/project/groww-scale-error-codes.md
  - Tests: (doc-truth fix; pinned indirectly by test_groww_scale_05_dual_fleet_contract touching the same doc area)
- [x] Item 3 — Named-lock knob in instance_lock.rs (Dhan byte-identical)
  - Files: crates/core/src/instance_lock.rs
  - Tests: test_compute_named_lock_path_groww_scale, test_groww_scale_lock_name_is_outside_groww_namespace, test_named_lock_path_dhan_name_matches_legacy_path, test_try_acquire_named_lock_smoke, test_renew_named_lock_smoke, test_release_named_lock_smoke, test_spawn_named_lock_heartbeat_smoke
- [x] Item 4 — GrowwScale05DualFleetDetected ErrorCode + rule-file section
  - Files: crates/common/src/error_code.rs, .claude/rules/project/groww-scale-error-codes.md, .claude/rules/project/dual-instance-lock-2026-07-04.md
  - Tests: test_groww_scale_05_dual_fleet_contract, test_all_list_length_matches_catalogue_size (bumped)
- [x] Item 5 — Groww fleet lock gate module + main.rs wiring
  - Files: crates/app/src/groww_scale_lock.rs, crates/app/src/lib.rs, crates/app/src/main.rs, crates/core/src/auth/secret_manager.rs
  - Tests: test_groww_scale_lock_backoff_secs_mirrors_dhan_policy, test_groww_scale_lock_max_attempts_pinned, test_fleet_gate_allows_only_acquired, test_acquire_groww_scale_fleet_lock_smoke, test_groww_scale_fleet_lock_is_wired_into_main

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Default boot (scale disabled) | Zero new SSM calls; byte-identical boot |
| 2 | Mac scale-test, no peer | Lock acquired, fleet spawns, heartbeat renews every 30s |
| 3 | Mac scale-test while AWS box also scale-enabled | AlreadyHeld → fleet SKIPPED, single-conn fallback, GROWW-SCALE-05 Critical page names the peer host_id |
| 4 | SSM unreachable (no creds / network) | 3 attempts (2s/4s), then fail-closed SkipFleet + GROWW-SCALE-05 page |
| 5 | Peer crashed 2 minutes ago | Stale (>90s TTL) → takeover, fleet spawns |
| 6 | growwapi bumped to 1.6.0 in requirements.txt | `cargo test -p tickvault-common` fails the build |
