# Implementation Plan: Market-Hours Interlock on Data-Destructive Portal Actions + Honest Autopilot Wording + Conservation-Audit Late-Boot Catch-Up

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — standing "go ahead with the plan" 2026-07-03 + the 2026-07-02 forensics R-fixes (audit fix #2 remediation)

> **Guarantee matrices:** this plan cross-references the canonical 15-row +
> 7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`
> (see also Item 22 / Item 24). Every item below inherits them.

## Context (forensics-proven, 2026-07-02)

At 15:05 IST mid-market the operator ran the portal wipe-ALL + docker-reset
with `force:true`. The QuestDB volume was deleted — ~4.5M rows wiped, 77s of
feed darkness, and the upstream ticks in that window are unrecoverable
(capture-at-receipt envelope: what was never re-deliverable is gone). The
deploy monitor misread the wipe as a crash, and `scripts/aws-autopilot.sh`
prints "reset-failed cleared any crash-loop limit" unconditionally — it
ASSUMES a crash-loop it never observed.

## Plan Items

- [x] Item 1 — Hard market-hours interlock on DATA-DESTRUCTIVE portal actions
  (Lambda). New `_DATA_DESTRUCTIVE = {wipe-questdb, wipe-groww, docker-reset,
  docker-nuke-bare}` subset of `_DESTRUCTIVE`; during 09:15–15:30 IST Mon–Fri
  these return HTTP 409 **even with `force:true`**. Lifecycle actions
  (stop / reboot / restart-app / stop-app) keep the existing force override.
  Danger-zone UI shows a "🔒 locked until 3:30 PM IST" label when in-window.
  - Files: deploy/aws/lambda/operator-control/handler.py,
    deploy/aws/lambda/operator-control/test_handler.py
  - Tests: DataDestructiveMarketHoursLock::test_all_data_destructive_actions_locked_in_window_even_with_force,
    test_lifecycle_actions_keep_force_override_in_window,
    test_data_destructive_is_subset_of_destructive,
    test_out_of_window_forced_wipe_still_dispatches,
    test_boundary_minutes_pin_exact_semantics,
    test_danger_zone_shows_lock_label_when_market_open

- [x] Item 2 — Honest autopilot heal wording. `scripts/aws-autopilot.sh`
  restart-heal message states what autopilot OBSERVED (unit inactive), not an
  assumed crash-loop; cause explicitly "unknown to autopilot: could be a
  crash, an external stop, or an operator action".
  - Files: scripts/aws-autopilot.sh
  - Tests: none (no self-test harness exists for this script; message-only
    change, verified by shellcheck-style read + `bash -n`)

- [x] Item 3 — Conservation-audit late-boot catch-up. Verified from code:
  `crates/app/src/tick_conservation_boot.rs:110-112`
  (`decide_conservation_start`) returns `SkipPastTrigger` for any trading-day
  boot at/after 15:40:00 IST, and `crates/app/src/main.rs:11042-11048`
  translates that into an unconditional `return` — so a boot between 15:40
  and midnight IST NEVER runs that day's audit. This is a real gap (a
  post-incident evening boot records no forensic WAL-vs-DB row for the day).
  Minimal fix: replace `SkipPastTrigger` with `RunCatchUp` (run once,
  immediately) for trading-day boots past the trigger; the row is honestly
  `partial` (boot after 09:00 → counters can't vouch for the session) but the
  WAL frame count + QuestDB row count for the day ARE recorded — exactly the
  forensic signal that was missing after the 2026-07-02 wipe. Idempotency:
  `tick_conservation_audit` DEDUP UPSERT KEYS `(ts, trading_date_ist, feed)`;
  each boot runs at most once; a re-boot the same evening writes a second
  honest row with its own `ts` (append-only forensic table — acceptable and
  documented).
  - Files: crates/app/src/tick_conservation_boot.rs (crate: tickvault-app),
    crates/app/src/main.rs
  - Tests: test_decide_conservation_start_before_after_force (updated),
    test_decide_conservation_start_late_boot_catches_up (new)

## Design

**Item 1 (Lambda interlock):** the gate ordering in `lambda_handler` becomes:
(1) NEW hard gate — `action in _DATA_DESTRUCTIVE and _is_market_hours(now)` →
409 with a plain-English body naming the lock window and the reason, NO force
escape; (2) existing soft gate — `action in _DESTRUCTIVE and market_hours and
not force` → 409 with the force hint (unchanged, still covers lifecycle
actions). `_DATA_DESTRUCTIVE` is a strict subset of `_DESTRUCTIVE`, asserted
by test. Boundary semantics inherit `_is_market_hours`:
`09:15:00 ≤ t < 15:30:00` IST Mon–Fri → locked (09:14:59 open, 09:15:00
locked, 15:29:59 locked, 15:30:00 open) — pinned by tests. UI: the `view`
response already carries `market_hours`; `loadOverview()` un-hides a
`#dangerlock` label inside the danger-zone card when `market_hours` is true.

**Item 2 (autopilot wording):** message-only change at
`scripts/aws-autopilot.sh:218` — the heal line reports the observed state
(`unit was inactive/failed`) and explicitly says the cause is unknown to
autopilot; the mechanical recovery (reset-failed + start) is still described
as the action taken, not as proof of a crash-loop.

**Item 3 (catch-up):** pure-function change in `decide_conservation_start` —
the `now >= 15:40` trading-day arm returns `ConservationStart::RunCatchUp`
instead of `SkipPastTrigger`; the enum variant is renamed (no dead variant
left behind). `main.rs` handles `RunCatchUp` by logging `info!` and falling
through to the same run path `RunNow` uses. Non-trading days still skip.
`cross_verify_1m_boot`'s separate `CrossVerifyStart::SkipPastTrigger` is OUT
OF SCOPE (cross-verify needs Dhan's intraday REST data which exists after
15:31 regardless; its skip is a separate design decision, untouched here).

## Edge Cases

- Boot at exactly 15:40:00 IST → `RunCatchUp` (was SkipPastTrigger) — pinned.
- Boot at 23:59:59 IST trading day → catch-up targets TODAY (target_ist_day
  computed after the decide arm, from now) — correct day attribution.
- Boot just after midnight IST → next trading-day path (SleepThenRun or
  SkipNonTradingDay) — unchanged.
- Boot 15:30–15:39 IST (post-close recovery fast-boot) → SleepThenRun to
  15:40 — already correct, unchanged.
- Catch-up on a feed disabled at runtime → existing per-feed runtime gate in
  `spawn_daily_tick_conservation_task` skips that lane's row — unchanged.
- Two boots the same evening → two rows with distinct `ts` (append-only
  forensic table; DEDUP prevents exact duplicates only) — documented, honest.
- Lambda: `force:true` + correct confirm word + in-window → still 409 (the
  whole point). Out-of-window + force + confirm → dispatches as before.
- Lambda: weekend/holiday wipe → `_is_market_hours` is Mon–Fri only (simple
  weekday window, matching the existing guard's approach — NSE holidays are
  NOT special-cased, consistent with the existing soft gate).
- Existing forced-wipe unit tests would newly fail if CI runs 09:15–15:30
  IST — fixed by pinning `_is_market_hours` to False in the WipeGate /
  WipeGrowwGate setUp (deterministic off-hours), with the new lock tested in
  its own class pinning True.

## Failure Modes

- `_is_market_hours` clock wrong on Lambda → worst case the lock applies at
  the wrong time; the operator can still run the action after 15:30 IST
  real time (the window is 6h15m/day; no permanent lockout). Fail direction
  is CLOSED for data (blocks a wipe), never closed for lifecycle actions.
- Catch-up audit runs while QuestDB is still recovering → existing fail-soft
  paths mark sources incomplete → `partial` row, never a false `balanced`
  and never a boot block (cold path, spawned task).
- Autopilot message change: pure string; `bash -n` syntax check; no logic
  path altered.

## Test Plan

- `python3 -m unittest discover deploy/aws/lambda/operator-control` — all
  existing tests + new `DataDestructiveMarketHoursLock` class green,
  deterministic at any wall-clock time.
- `cargo test -p tickvault-app` (scoped per testing-scope.md — only
  crates/app touched in Rust) + `cargo fmt --check` + scoped clippy.
- `bash -n scripts/aws-autopilot.sh`.

## Rollback

- Item 1: revert the handler.py + test_handler.py hunks — the soft
  force-overridable guard is fully preserved underneath, so reverting
  restores exactly the pre-PR behaviour.
- Item 2: revert the one-line message change.
- Item 3: revert the enum arm (`RunCatchUp` → `SkipPastTrigger`) + the
  main.rs match arm; no schema, no persisted-state, no config change —
  pure decision-function rollback.

## Observability

- Item 1: every refused action returns a typed 409 body
  (`market_hours_locked: true`) which the portal toasts verbatim; the
  danger-zone lock label makes the state visible BEFORE a click.
- Item 3: catch-up run logs `info!` ("late boot — running catch-up
  conservation audit now"), increments the existing
  `tv_tick_conservation_audit_runs_total{outcome}` counter, and writes the
  per-feed `tick_conservation_audit` row (the audit trail itself IS the
  deliverable). Existing TICK-CONSERVE-01 error path unchanged.
- Item 2: the SNS/Telegram autopilot summary now carries the honest wording.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | wipe-questdb, Tue 15:05 IST, force+WIPE | 409 locked (no SSM call) |
| 2 | wipe-groww / docker-reset / docker-nuke-bare, in-window, force+word | 409 locked |
| 3 | stop / restart-app, in-window, force | allowed (lifecycle override kept) |
| 4 | wipe-questdb, Tue 16:00 IST, force+WIPE | dispatches (unchanged) |
| 5 | 09:14:59 / 09:15:00 / 15:29:59 / 15:30:00 IST | open / locked / locked / open |
| 6 | app boot 16:30 IST trading day | conservation audit runs once, `partial` row written |
| 7 | app boot 15:39 IST trading day | sleeps 60s, runs at 15:40 (unchanged) |
| 8 | app boot Saturday 16:30 IST | skipped (non-trading day, unchanged) |
| 9 | autopilot finds unit inactive | heal msg states observation + unknown cause |
