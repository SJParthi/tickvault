# Implementation Plan: Consolidated Audit-Sweep Hardening (medium-tier gaps)

**Status:** APPROVED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — consolidated audit-sweep directive, this session

> **Guarantee matrices:** carried by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory per `per-item-guarantee-check.sh`). All 15 rows of the 100% Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply to every item in this plan.

Source: `.claude/plans/permutation-coverage-audit-2026-07-01.md`. This PR sweeps the
medium-tier real gaps that survived refutation, as ONE consolidated PR. Each item
is investigated-then-implemented with pure-function unit tests (truth tables). Items
that genuinely need the live box / AWS mutation / operator judgment are SKIPPED with
a note, not blocking the whole PR.

## Design

- **AUTH-P11 (§137/§183):** Promote the RESERVED `AUTH-GAP-04` stub to a real
  `AuthGap04TotpRotatedExternally` (Critical) ErrorCode variant. Emit it (with a
  `code =` field) at the TOTP-retry-exhaustion terminal branch in
  `token_manager.rs` so a rotated-secret failure is distinctly typed + audited,
  pointing the operator at the exact SSM param. Remove `AUTH-GAP-04` from the
  crossref reverse-check allowlist; keep the existing `AUTH-GAP-` prefix.
- **AUTH-P13 (§136):** `ip_monitor.rs` `CheckFailed` (both IP-echo endpoints
  fail, e.g. detached EIP / no route) currently only `warn!`s and never escalates.
  Add a pure `classify_ip_check_streak` helper + a bounded-consecutive-CheckFailed
  counter in the monitor loop: after N consecutive `CheckFailed`s it emits a
  CRITICAL `GapNetIpMonitor`-coded `error!` (stranded-box reachability) +
  `tv_ip_monitor_check_failed_streak` gauge, distinguishing a stranded box from a
  benign one-off blip. Pure fn is unit-tested (truth table).
- **FTC-14 (§139):** Add a `WsPoolSpawn` variant to `StartLaneError`; when
  `start_dhan_lane`'s `should_connect_ws` arm gets `None` from
  `create_websocket_pool` despite a valid plan (a real WS-pool-build failure),
  return `Err(StartLaneError::WsPoolSpawn)` instead of silently proceeding blind.
  Map it in `classify_start_lane_error` to `DhanLane02WsPoolSpawnFailed` with
  `stage='ws_pool'` so the operator hits the right runbook. Unit-tested.
- **NT-15 (§143):** Add `TickGapDetector::seed_subscribed(security_id, segment,
  at)` that inserts a baseline instant ONLY if the key is absent (never clobbers a
  real tick). Wire it in `main.rs` right after the subscription plan is built, by
  iterating `plan.registry.iter()`. A never-ticked subscribed SID now becomes a
  `scan_gaps` key after `threshold_secs` of market hours — turning WS-GAP-06 into a
  first-tick black-hole detector at zero extra infra. Pure insert-if-absent logic
  unit-tested.
- **BP-08 (§147):** New `crates/app/src/resource_monitor.rs` mirroring the
  `disk_health_watcher.rs` supervised-poll template: samples `/proc/self/fd`
  count vs `LimitNOFILE` (RESOURCE-01), VmRSS vs cgroup memory.max (RESOURCE-02),
  and spill-dir free-percent (RESOURCE-03). Each ≥80% → `error!(code=RESOURCE-0N)`
  + `tv_open_fds` / `tv_process_rss_bytes` / `tv_spill_free_pct` metric. Adds 3 real
  ErrorCode variants (`RESOURCE-` prefix) + a `PROC-` allowlist-prefix entry (kept
  for BP-07 PROC-01 reservation — but PROC-01 impl is out of scope here). Pure
  threshold classifiers unit-tested (truth tables); supervised spawn wired in
  main.rs with a `secret_manager.rs` wiring ratchet.
- **BP-14 (§148):** Add `arn:aws:automate:${region}:ec2:recover` action to the
  `system_status_check` alarm and `:reboot` to the `instance_status_check` alarm
  in `deploy/aws/terraform/alarms.tf`, alongside the SNS action. Add a ratchet in
  `aws_deploy_safety_guard.rs` asserting both auto-recover/reboot actions are
  present. Pure IaC + source-scan test.

## Edge Cases

- AUTH-P11: only the TOTP-*exhaustion* terminal branch emits the new code — a
  single transient TOTP rejection (retry still in flight) keeps the existing
  transient path (no false Critical). Permanent DH-901 auth errors are unchanged.
- AUTH-P13: escalation is edge-triggered (fires once when the streak crosses the
  threshold, not every poll) and market-hours-agnostic in the pure classifier but
  the loop only escalates while the monitor is enabled. A single failure never
  escalates. A recovered check resets the streak.
- FTC-14: only the `should_connect_ws == true && plan.is_some()` case treats
  `None` as a failure; the intentional "no plan / offline" skips are unchanged.
- NT-15: seed is insert-if-absent — a SID that has already ticked is never
  overwritten by the seed (would corrupt real freshness). Idempotent re-seed is a
  no-op. Empty registry seeds nothing.
- BP-08: non-Linux (`/proc` absent, no cgroup) → ProbeFailed branch, gauge stays
  at previous value, no false Critical. cgroup memory.max = "max" (unlimited) →
  RESOURCE-02 skips (no denominator). fd LimitNOFILE unreadable → skip.
- BP-14: alarm actions are additive (SNS + recover) — SNS notification is
  preserved; recover only fires on a real breaching System check.

## Failure Modes

- Each monitor is a supervised infinite-loop task (respawn on panic, mirroring
  DISK-WATCHER-01) — a probe panic never silently kills monitoring.
- All probe failures degrade to a `ProbeFailed`/skip branch that leaves the gauge
  honest (no false-OK), never a panic, never a crash.
- New ErrorCodes are all Critical/High severity so they route to Telegram; none
  are auto-triage-safe where operator action is required.
- FTC-14 returning an error where it previously proceeded-blind is the correct
  fail-closed behaviour: a WS pool we intended to build but couldn't is a real
  failure, not an offline mode.

## Test Plan

- `token_manager.rs` unit test: TOTP-exhaustion branch emits `AuthGap04...` code
  (source-scan / behavior assert).
- `error_code.rs`: catalogue-size bump (110 → new count), prefix-pattern test
  covers `RESOURCE-`, `AUTH-GAP-04` roundtrip, severity assignment.
- `ip_monitor.rs` unit tests: `classify_ip_check_streak` truth table
  (below/at/above threshold; reset on success).
- `main.rs` unit test: `classify_start_lane_error(WsPoolSpawn)` → DhanLane02 /
  `stage='ws_pool'`.
- `tick_gap_detector.rs` unit tests: `seed_subscribed` insert-if-absent +
  never-clobber-real-tick + scan flags a seeded-but-never-ticked SID.
- `resource_monitor.rs` unit tests: fd/RSS/free-pct threshold classifiers truth
  tables + parse helpers + `classify_join_exit` reuse.
- `aws_deploy_safety_guard.rs`: recover/reboot action present in alarms.tf.
- `secret_manager.rs`: resource_monitor spawn wiring source-scan ratchet.
- `crossref` + `tag-guard` + `aws_infra_wiring` cross-crate meta-guards green.
- `per-item-guarantee-check.sh` on this plan file green.

## Rollback

Every item is additive and independently revertable:
- New ErrorCode variants + emit sites: revert the variant + emit hunk.
- New `resource_monitor.rs` module + its spawn: delete the module + the spawn line
  + the wiring ratchet.
- FTC-14: revert the `WsPoolSpawn` arm to the prior `(None, None)` proceed.
- NT-15 seed: remove the seed loop + the `seed_subscribed` method.
- BP-14: remove the two auto-recover/reboot action lines + the ratchet.
No schema changes, no data migration, no config-format change. `dry_run` stays `true`.

## Observability

- AUTH-P11: `AuthGap04TotpRotatedExternally` → Telegram Critical + `error!` with
  `code =` field (5-sink pipeline).
- AUTH-P13: `tv_ip_monitor_check_failed_streak` gauge + CRITICAL `error!` on the
  streak edge.
- FTC-14: `DhanLane02` + `stage='ws_pool'` label on the existing lane
  classify/emit path + `tv_dhan_lane_start_failed_total{stage='ws_pool'}`.
- NT-15: reuses the existing `tv_tick_gap_instruments_silent` gauge + WS-GAP-06
  Telegram path — no new metric needed (seeding turns the existing signal into a
  first-tick detector).
- BP-08: `tv_open_fds`, `tv_process_rss_bytes`, `tv_spill_free_pct` gauges +
  `RESOURCE-01/02/03` `error!` codes + `tv_resource_monitor_respawn_total`.
- BP-14: CloudWatch auto-recover/reboot alarm actions (self-heal automation).

## Plan Items

- [ ] **AUTH-P11** — `AuthGap04TotpRotatedExternally` ErrorCode + emit at TOTP-exhaustion + crossref allowlist removal + rule mention
  - Files: crates/common/src/error_code.rs, crates/core/src/auth/token_manager.rs, crates/common/tests/error_code_rule_file_crossref.rs, .claude/rules/project/wave-4-error-codes.md
  - Tests: test_auth_gap_04_variant + catalogue bump + token_manager emit test

- [ ] **AUTH-P13** — bounded-consecutive-CheckFailed escalation in ip_monitor
  - Files: crates/core/src/network/ip_monitor.rs
  - Tests: test_classify_ip_check_streak_* truth table

- [ ] **FTC-14** — DHAN-LANE-02 ws_pool stage tagging
  - Files: crates/app/src/main.rs
  - Tests: classify_start_lane_error_ws_pool_maps_to_dhan_lane_02

- [ ] **NT-15** — per-SID cold black-hole seeding
  - Files: crates/core/src/pipeline/tick_gap_detector.rs, crates/app/src/main.rs
  - Tests: test_seed_subscribed_* + scan flags seeded-never-ticked SID

- [ ] **BP-08** — RESOURCE-01/02/03 monitors (fd/RSS/spill-free)
  - Files: crates/app/src/resource_monitor.rs, crates/app/src/lib.rs, crates/app/src/main.rs, crates/common/src/error_code.rs, .claude/rules/project/wave-4-error-codes.md, crates/app/src/secret_manager.rs (wiring ratchet)
  - Tests: threshold classifiers truth tables + spawn wiring ratchet

- [ ] **BP-14** — EC2 auto-recover / reboot alarm actions
  - Files: deploy/aws/terraform/alarms.tf, crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests: deploy_status_check_alarms_have_auto_recover_action

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | TOTP secret rotated externally, retries exhaust | distinct AUTH-GAP-04 Critical Telegram naming the SSM param |
| 2 | Box loses EIP, both IP-echo endpoints fail N times | CRITICAL stranded-box escalation, not endless warn! |
| 3 | Runtime WS-pool build fails on a connect day | DHAN-LANE-02 with stage=ws_pool (right runbook) |
| 4 | A subscribed SID never ticks all session | scan_gaps flags it → WS-GAP-06 fires |
| 5 | fd/RSS/spill-free crosses 80% | RESOURCE-01/02/03 Critical early-warning |
| 6 | EC2 System check fails (hardware) | CloudWatch auto-recover migrates the box |
