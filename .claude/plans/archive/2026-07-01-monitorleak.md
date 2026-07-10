# Implementation Plan: Fix per-lane leak of process-global monitors

**Status:** APPROVED
**Date:** 2026-07-01
**Approved by:** operator (worker task directive, this session)

> **Guarantee matrix cross-ref:** this plan carries the 15-row + 7-row guarantee
> matrix by reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (§ "The 15-row" + "The 7-row"). This is a bug-fix (relocate 4 already-tested
> spawns; no new pub fn, no new hot-path, no new table, no new alert) — the
> applicable rows are: code checks (banned-pattern/pub-fn/plan-verify), testing
> coverage (the existing wiring + isolation guards), monitoring (the 4 monitors
> keep their counters), extreme check (source-scan ratchets fail the build on
> regression). All other rows are `N/A — pure relocation, behaviour preserved`.

## Design

Four PROCESS-GLOBAL (host-level) supervised observability monitors are spawned
INSIDE the per-lane `start_dhan_lane` (`crates/app/src/main.rs`), bound to
`_`-prefixed locals that are NOT captured in `DhanLaneRunHandles` and are NOT
aborted by its `Drop` / `teardown_dhan_lane_tasks`:

- `spawn_supervised_spill_disk_health_watcher` (~5501) — DISK-WATCHER-01
- `spawn_supervised_oom_monitor` (~5515) — PROC-01
- `spawn_supervised_resource_monitor` (~5529) — RESOURCE-01/02/03
- the DayOhlc tracker + tick-consumer + `spawn_supervised_midnight_reset_task`
  (~6474–6497) — INDEX-OHLC-02

`start_dhan_lane` runs at boot AND is re-invoked by the runtime cold-start retry
loop (`run_dhan_lane_cold_start`) on every Dhan enable / stop→restart / cold-start
retry. Each re-invocation LEAKS a fresh never-aborted monitor → duplicate
PROC-01 / RESOURCE-01/02/03 / INDEX-OHLC-02 pages + N× metric increments for one
real event, unbounded task growth under toggling, duplicate blocking `df`
shell-outs. On a boot-OFF-Dhan deployment the host monitors never start at all
until Dhan is toggled on.

**Fix:** these are host/process-global observability monitors — they belong in
`main()`'s process-global prefix (after `build_shared_infra(...).await?`, before
the Dhan-lane context/gate), spawned EXACTLY ONCE regardless of Dhan
enable/disable/restart, with handles owned at the process level. The DayOhlc
tracker + consumer + reset move as a unit (they share the `Arc<DayOhlcTracker>`
and read the process-global `tick_broadcast_sender`), decoupling them from the
lane-local `should_connect_ws && subscription_plan.is_some()` block.

## Edge Cases

- Dhan-OFF boot: the 4 monitors + DayOhlc consumer now start once at process
  scope; DayOhlc consumer idles (no Dhan IDX_I ticks), midnight reset resets an
  empty tracker — benign, and matches "runs regardless of enable/disable".
- Runtime cold-start retry / stop→restart: `start_dhan_lane` re-invocation no
  longer re-spawns any of the 4 monitors → no leak, no N× alerts.
- The lane-local 09:14 readiness ping + Phase2/SLO tasks STAY in the Dhan block
  (they are feed-readiness, not process-global) — only the DayOhlc
  tracker/consumer/reset move out.

## Failure Modes

- A monitor panic still respawns via its own supervisor (unchanged).
- `build_shared_infra` isolation guard forbids auth/instrument/WS strings in its
  body — the 4 monitor spawns contain none of those, and they are placed in
  `main()` AFTER `build_shared_infra` returns (not inside it), so the guard holds.
- Wiring ratchets (`test_oom_monitor_is_wired_into_main`,
  `test_main_uses_supervised_resource_monitor`, disk-watcher guard,
  `test_day_ohlc_orchestrator_is_wired_into_main`) are `main_rs.contains(...)`
  source-scans with NO location pin → stay green at the new location.

## Test Plan

- `cargo test -p tickvault-app` (lib + tests + bins) — incl.
  `per_feed_boot_isolation_guard`, boot guards, d2a start-lane guard.
- `cargo test -p tickvault-core --lib` — `secret_manager` wiring ratchets.
- `cargo test -p tickvault-common --test triage_rules_full_coverage_guard --test aws_infra_wiring`.
- `cargo test -p tickvault-storage --test per_item_guarantee_matrix_guard`
  + resource/disk supervisor guards run under `-p tickvault-storage`.
- hooks: per-item-guarantee-check, plan-verify, banned-pattern, pub-fn-test,
  pub-fn-wiring, fmt.

## Rollback

Single-commit relocation on branch `claude/fix-monitor-per-lane-leak`. Revert the
commit to restore the prior (leaky) placement; no schema/config/dep change, so
rollback is a clean `git revert`.

## Observability

The 4 monitors keep their exact counters + ErrorCodes + supervisors
(DISK-WATCHER-01 / PROC-01 / RESOURCE-01/02/03 / INDEX-OHLC-02). This fix
REMOVES duplicate emissions (N monitors → N× alerts) by ensuring exactly one
instance of each runs process-wide.

## Plan Items

- [x] Move `spawn_supervised_spill_disk_health_watcher` + `spawn_supervised_oom_monitor`
      + `spawn_supervised_resource_monitor` (ONLY the 3 monitor spawns; leave the
      unrelated `tick_spill_drain::init` prelude in place) out of `start_dhan_lane`
      into `main()` process-global prefix.
  - Files: crates/app/src/main.rs
  - Tests: test_oom_monitor_is_wired_into_main, test_main_uses_supervised_resource_monitor
- [x] Move the DayOhlc tracker + `spawn_day_ohlc_tick_consumer` + `spawn_supervised_midnight_reset_task`
      out of the lane-local subscription-plan block into `main()` process-global prefix.
  - Files: crates/app/src/main.rs
  - Tests: test_day_ohlc_orchestrator_is_wired_into_main
- [x] Verify wiring ratchets + isolation guards stay green.
  - Files: crates/core/src/auth/secret_manager.rs, crates/app/tests/per_feed_boot_isolation_guard.rs
  - Tests: dhan_off_skips_auth_and_instruments_via_the_lane_gate

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot Dhan-ON | 4 monitors spawn once at process scope |
| 2 | Runtime cold-start retry (start_dhan_lane re-invoked) | NO new monitor spawned (no leak) |
| 3 | Boot Dhan-OFF | 4 monitors spawn once at process scope (previously never started) |
| 4 | Stop→restart Dhan lane toggling | monitor count stays 1 each (no N× alerts) |
