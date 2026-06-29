# Implementation Plan: Pool watchdog respects runtime Dhan-OFF

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** operator (focused bug-fix task)
**Crate:** `app` — `crates/app/src/main.rs`

## Design

When the operator toggles Dhan OFF at runtime from the feed-control page (PR-E
runtime toggle), the Dhan main-feed WebSocket connections go dormant by design —
the read/reconnect loop consults the `FeedRuntimeState::dhan` `Arc<AtomicBool>`
and parks. But `spawn_pool_watchdog_task` in `crates/app/src/main.rs` only gated
its `Degraded`/`Halt` side-effects on `in_market_hours` (+ the H7 `lane_halt`
blast-radius mode), NOT the runtime Dhan-enable flag. So a deliberately-dormant
0-connection pool was read as "fully degraded" and, after ~300s, the `Halt`
verdict fired `process::exit(2)` — self-killing the whole process (which also
killed the independent Groww feed). Live evidence:
`A4 FATAL: WebSocket pool ... FULLY DEGRADED for >300s — initiating process halt`.

Fix (minimal, action-only — the core `poll_watchdog` verdict computation in
`crates/core/src/websocket/connection_pool.rs` is untouched):

1. New pure helper `pool_watchdog_should_act_on_degradation(in_market_hours,
   dhan_enabled) -> bool` (returns `true` to ACT, i.e. page + exit/teardown;
   `false` to treat the down pool as expected idle). It is simply
   `in_market_hours && dhan_enabled`.
2. Thread `dhan_enabled: Option<Arc<AtomicBool>>` into `spawn_pool_watchdog_task`
   — a clone of `FeedRuntimeState::dhan` via the existing `feed_runtime.dhan_flag()`
   (the SAME atomic the connection read loop already consults). `None` keeps the
   legacy always-enabled behaviour.
3. In the watchdog poll, read the flag once (`Relaxed`) into `dhan_on` and compute
   `should_act = pool_watchdog_should_act_on_degradation(in_market_hours, dhan_on)`.
4. Gate the `Degraded`, `Recovered`, and `Halt` ACTIONS on `should_act` instead of
   raw `in_market_hours`. When `should_act == false`, take the existing
   "expected idle" info-log branch — skip Telegram page, skip `process::exit`,
   skip lane teardown.
5. Extend the off-hours `reset_watchdog` gate from `!in_market_hours` to
   `!should_act` so a stale `AllDown { since }` does not accumulate while Dhan is
   OFF and trip Halt the instant Dhan is re-enabled mid-market.
6. Pass `Some(feed_runtime.dhan_flag())` at both `spawn_pool_watchdog_task` call
   sites (fast/crash-recovery boot ~L1652 and slow boot ~L5603).

## Edge Cases

- `dhan_enabled = None` (legacy / not wired): treated as enabled → behaviour
  identical to before the change.
- In-market AND Dhan enabled, real all-down pool: `should_act == true` → genuine
  300s Halt safety property preserved (page + exit/teardown unchanged).
- Off-hours, Dhan enabled: `should_act == false` → pre-existing post-market
  suppression preserved.
- Dhan OFF during market hours: `should_act == false` → no page, no exit, no
  teardown; watchdog reset keeps the `since` window fresh.
- Dhan re-enabled mid-market after an OFF window: reset on each OFF poll means
  the first enabled poll starts a fresh 300s window (no spurious instant Halt).
- Metric counters (`tv_pool_degraded_alerts_total`, `tv_pool_self_halts_total`)
  still increment BEFORE the gate so dashboards retain the signal.

## Failure Modes

- If the flag is wired to the WRONG atomic, the watchdog could suppress a real
  in-market outage. Mitigated by using the existing `feed_runtime.dhan_flag()`
  accessor (the exact atomic the connection loop reads) and a source-scan ratchet.
- If a future refactor drops the gate, the A4-FATAL self-halt regression returns.
  Mitigated by the source-scan guard `pool_watchdog_gates_on_runtime_dhan_enable_flag`
  plus the unit-test truth table.
- `Relaxed` ordering is correct: the flag is a UI-status atomic with no ordering
  dependency on other shared state (matches `FeedRuntimeState` usage).

## Test Plan

- `crates/app/src/main.rs` unit test
  `test_pool_watchdog_should_act_on_degradation_truth_table` — pins all 4 truth
  cases of the pure helper (in-market×enabled, in-market×OFF, off-hours×enabled,
  off-hours×OFF).
- `crates/app/tests/post_market_pool_halt_guard.rs`:
  - new `pool_watchdog_gates_on_runtime_dhan_enable_flag` — source-scan pinning
    the `dhan_enabled` param, the helper, and the `should_act` gate (incl. the
    Halt gate preceding `process::exit(2)`).
  - updated `watchdog_metrics_still_increment_outside_market_hours` and
    `pool_watchdog_is_reset_outside_market_hours` to the new `should_act` gate.
  - widened the source-scan window (8,500 → 10,360) for the bytes the fix added.
- Verify: `cargo check -p tickvault-app` + `cargo test -p tickvault-app`.

## Rollback

Single-crate, additive change isolated to `crates/app/src/main.rs` (one helper,
one new param, gating swap) plus its guard test. Rollback = revert the PR commit;
no schema, no migration, no config change, no DB impact. `dhan_enabled = None`
path means the change is inert unless the call sites pass the flag.

## Observability

- The expected-idle info log now carries structured `in_market_hours` +
  `dhan_enabled` fields so the operator can see WHY a down pool is being treated
  as idle (off-hours vs Dhan toggled OFF).
- The existing `tv_pool_degraded_alerts_total` / `tv_pool_self_halts_total` /
  `tv_pool_recoveries_total` counters still increment before the gate, so
  dashboards keep the raw signal even when paging is suppressed.
- No new metric is introduced (minimal-diff fix); the A4-FATAL self-halt is
  removed for the dormant Dhan-OFF case, which is the desired observable change.

## Plan Items

- [x] Add pure helper `pool_watchdog_should_act_on_degradation`
  - Files: crates/app/src/main.rs
  - Tests: test_pool_watchdog_should_act_on_degradation_truth_table
- [x] Thread `dhan_enabled` flag into `spawn_pool_watchdog_task` + gate
      Degraded/Recovered/Halt actions + reset on `!should_act`
  - Files: crates/app/src/main.rs
  - Tests: pool_watchdog_gates_on_runtime_dhan_enable_flag
- [x] Pass `Some(feed_runtime.dhan_flag())` at both call sites
  - Files: crates/app/src/main.rs
  - Tests: pool_watchdog_gates_on_runtime_dhan_enable_flag
- [x] Update existing source-scan guards for the new gate + widened window
  - Files: crates/app/tests/post_market_pool_halt_guard.rs
  - Tests: watchdog_metrics_still_increment_outside_market_hours,
    pool_watchdog_is_reset_outside_market_hours

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan toggled OFF, in market hours, pool 0 conns for >300s | No page, no process::exit, no teardown; info log "expected idle"; Groww unaffected |
| 2 | Dhan ON, in market hours, real all-down for >300s | Genuine Halt fires (page + exit/teardown) — unchanged |
| 3 | Off-hours, Dhan ON, pool down | Suppressed (pre-existing post-market behaviour) — unchanged |
| 4 | Dhan re-enabled mid-market after OFF window | Fresh 300s window; no spurious instant Halt |
