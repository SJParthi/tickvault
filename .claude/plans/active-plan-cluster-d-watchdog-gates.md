# Implementation Plan: Cluster D — Orphan-Watchdog Re-Home + Expired Live-Gate Re-Arm

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — coordinator-relayed cluster D directive, 2026-07-14

Per-item guarantee matrices: cross-reference .claude/rules/project/per-wave-guarantee-matrix.md (15-row + 7-row) — applies to every item below.

Crates touched: tickvault-common, tickvault-core, tickvault-trading, tickvault-app.

## Design

Two independent safety fixes, one PR:

**Part 1 — orphan-position watchdog re-home.** The 15:25 IST broker-position
cross-check (`spawn_supervised_orphan_position_watchdog`) is spawned today ONLY
from `fn spawn_post_market_tasks` in `crates/app/src/main.rs`, whose two callers
(the fast crash-recovery arm and `start_dhan_lane`) are both Dhan-gated — so on
the post-2026-07-13 `dhan_enabled=false` production boots the check never runs.
Fix: the spawn fn gains an `auth: WatchdogAuth` parameter —
`Static { token_handle, client_id }` (byte-identical to today's behaviour, used
by the existing `spawn_post_market_tasks` site so the fast crash-recovery arm
keeps its coverage) and `GlobalAtFireTime` (resolves
`tickvault_core::auth::token_manager::global_token_manager()` at each 15:25
fire — the OnceLock is registered by BOTH the lane and the Dhan REST stack's
Phase 2, so a dhan-off boot has it well before 15:25). A NEW process-global
prefix spawn in main.rs (immediately after `spawn_daily_tick_conservation_task`,
the exact tick-conservation hoist precedent) uses `GlobalAtFireTime` and runs on
every slow boot regardless of `dhan_enabled`. A module-level
`ORPHAN_WATCHDOG_CLAIMED` AtomicBool once-guard (compare_exchange) makes a
double 15:25 page structurally impossible even if a future refactor makes both
call sites reachable. `TokenManager` gains a `client_id_string()` accessor
(client-id is NOT secret-classed — it travels as a plain `client-id` header in
every REST call). The `global_token_manager()` read lives in
`orphan_position_watchdog_boot.rs`, NOT main.rs (main.rs carries a ratchet
pinning exactly one such read in its prod region). No-manager at fire time =
LOUD degraded arm (error! + failure counter + Critical Telegram "check the Dhan
app manually before 3:30 PM") — never a clean signal, mirroring the existing
no-token degraded arm's un-coded Custom-Critical style.

**Part 2 — gate re-arm (2099 sentinels, single source of truth).** All three
date safety gates expired silently: Gate A (OMS `SANDBOX_DEADLINE_EPOCH_SECS`,
2026-07-01 epoch, fn-local in engine.rs), Gate B (`LIVE_TRADING_EARLIEST_*`
constants, 2026-07-01 IST), Gate C (`sandbox_only_until` serde default +
base.toml, 2026-06-30). Re-arm all three to the 2099-12-31 sentinel matching
production.toml's armed precedent: Gate A's constant moves to
`crates/common/src/constants.rs` as `SANDBOX_DEADLINE_EPOCH_SECS =
4_102_358_400` (2099-12-31T00:00:00Z, chrono-cross-checked) and engine.rs
imports it; Gate B constants become 2099/12/31; Gate C default + base.toml
become "2099-12-31". A NEW pure fn `expired_live_gates(today_ist)` in config.rs,
called from `ApplicationConfig::validate()`, emits one `warn!` per date gate
whose date is already in the past — the loud-expiry tripwire that prevents this
silent-no-op class from recurring on any future date-bump. Going live now
requires editing the constants with a fresh dated operator quote.

## Edge Cases

- 15:25 fire with NO global TokenManager registered (fully-dhan-less boot, or
  the REST stack parked on AlreadyHeld/SSM retry, or pre-Phase-2 boot stall):
  degraded Critical Telegram + counter — NEVER clean, NEVER a panic.
- 15:25 fire with a manager but `token_handle.load() == None` (mint failed):
  the existing degraded arm fires unchanged.
- Non-trading day: the loop's existing `calendar.is_trading_day` gate skips —
  no page (correct).
- Fast crash-recovery arm boot: keeps the `Static` spawn via
  `spawn_post_market_tasks`; the arm early-returns before the process-global
  block, so only ONE spawn executes; the AtomicBool is belt-and-braces.
- Both call sites reached by a future refactor: the once-guard's
  compare_exchange makes the second call an info-logged no-op.
- Gate boundary semantics preserved exactly: Gate A strict `<` on epoch secs
  (UTC), Gate B strict `<` on IST NaiveDate, Gate C INCLUSIVE `<=` on IST
  NaiveDate — the sentinel change touches only the date values.
- Gate A UTC vs Gate B IST calendar-day nuance: both sentinels are 2099-12-31
  on their own clock; the alignment ratchet compares the UTC calendar date of
  the epoch against the Gate B NaiveDate (same day) and documents the nuance.
- `expired_live_gates` at exactly the sentinel date: a gate dated today is NOT
  expired (strictly-past check) — no warn.
- Config-ON boot held under the runtime-disabled overlay (dormant shape: no
  Dhan lane starts, no Dhan REST stack starts): NO token manager — live or
  global — ever registers, so the watchdog fires ONE degraded Critical
  Telegram per trading day at 15:25. DELIBERATE Rule-11 loudness (a
  broker-position check that cannot run must never look clean), NOT a bug.
  Triage: re-enable the Dhan feed, or expect the daily page while the shape
  is deliberately held. (Review round 1, 2026-07-14.)
- Runtime lane stop→re-start (once the Phase-A 409 refusal relaxes): the
  `GlobalAtFireTime` fire-time resolution PREFERS the live lane-owned manager
  (`FeedRuntimeState::live_token_manager()`, the D2c gauge pattern) and falls
  back to the global OnceLock — the set-once OnceLock can never pin the
  watchdog to a dead boot-time JWT. (Review round 1, 2026-07-14.)

## Failure Modes

- Degraded 15:25 arms are Telegram-only (no CloudWatch errcode filter) — honest
  delivery boundary; an ORPHAN-POSITION-02 code + filter is a flagged follow-up.
- If the REST stack never registers the global manager (parked all day), every
  trading day pages the degraded Critical at 15:25 — loud and correct (the
  operator must check positions manually).
- `get_positions` transport failure: existing one-retry-then-degraded-Critical
  arm unchanged.
- A future date-bump that re-expires: `expired_live_gates` warns at every boot
  ("silent no-op — re-arm with a dated operator quote").
- Watchdog task death: the existing supervised 30s-respawn loop is unchanged.

## Test Plan

- `crates/core/src/auth/token_manager.rs`: unit test `client_id_string()`
  returns the constructor-provided client id (no network).
- `crates/app/src/orphan_position_watchdog_boot.rs`: once-guard claim/reset
  unit tests; `WatchdogAuth` enum construction compile-path test.
- `crates/app/tests/orphan_position_watchdog_wiring_guard.rs` rewritten:
  (t1) main.rs contains the spawn string; (t2) FIRST occurrence comes BEFORE
  `fn spawn_post_market_tasks(` (process-global prefix pin — inverts the old
  assertion); (t3) exactly 2 spawn-call occurrences in main.rs; (t4) the boot
  module contains `global_token_manager()` + the no-manager degraded wording
  (stub-guard); (t5) main.rs calls `spawn_post_market_tasks(` ≥ 3 times.
- `crates/trading/tests/sandbox_enforcement_guard.rs` rewritten: engine.rs
  references the shared constant NAME inside `#[cfg(not(test))]` with the `<`
  comparison + `OmsError::SandboxEnforcement`; constants.rs carries the literal
  `4_102_358_400`; chrono cross-check `Utc.with_ymd_and_hms(2099,12,31,0,0,0)`
  equals the imported constant; alignment check constant's UTC date ==
  `LIVE_TRADING_EARLIEST_*` NaiveDate.
- `crates/common/src/config.rs`: `expired_live_gates` unit tests — empty at
  2026-07-14, non-empty (all three names) at 2100-01-01, per-gate granularity;
  serde-default inline test literal updated to "2099-12-31".
- Era-aware tests (config.rs sandbox-guard tests,
  `crates/common/tests/sandbox_live_gate_validation.rs`) self-adapt — verified
  green after the constant change.
- Full: `cargo test -p tickvault-common -p tickvault-core -p tickvault-trading
  -p tickvault-app` + clippy `-D warnings` + fmt.

## Rollback

Both parts are pure-additive/constant changes with no schema, no hot-path, no
deploy-surface impact. Rollback = `git revert` of the two implementation
commits: Part 1's revert restores the (Dhan-gated, today-dead) spawn topology;
Part 2's revert restores the expired dates (a strictly-worse but known state).
No data migration, no config coordination needed (base.toml change is
backward-compatible; production.toml untouched).

## Observability

- Watchdog: existing counters unchanged (`tv_orphan_position_watchdog_runs_total`,
  `tv_orphan_position_watchdog_fetch_failures_total` — the new no-manager arm
  increments the fetch-failures counter), existing
  `error!(code = ORPHAN-POSITION-01)` on detection, existing Telegram events
  (`OrphanPositionsClean` / `OrphanPositionDetected` / degraded Custom
  Critical). New: one info! line when the once-guard refuses a duplicate spawn.
- Gates: `tv_sandbox_gate_blocks_total` counter unchanged; reworded
  `error!` on a sandbox block; NEW one `warn!` per expired gate at every
  validate() (the loud-expiry tripwire).
- Runbook: `.claude/rules/project/phase-0-item-20-error-codes.md` gains a dated
  2026-07-14 update (spawn topology, deleted audit table, live ratchet path).

## Plan Items

- [x] Archive the two merged plans (spot-1m-diagnostics #1524, questdb-partition-s3-archive #1504)
  - Files: .claude/plans/archive/2026-07-14-spot-1m-diagnostics.md, .claude/plans/archive/2026-07-13-questdb-partition-s3-archive.md
  - Tests: plan-gate cap (≤5 active) satisfied
- [x] TokenManager `client_id_string()` accessor
  - Files: crates/core/src/auth/token_manager.rs
  - Tests: test_client_id_string_returns_constructor_client_id
- [x] `WatchdogAuth` enum + fire-time global resolution + once-guard in the boot module
  - Files: crates/app/src/orphan_position_watchdog_boot.rs
  - Tests: test_orphan_watchdog_claim_once_guard, test_watchdog_auth_enum_arms_construct
- [x] main.rs process-global prefix spawn (GlobalAtFireTime) + Static at the family site + hoist doc-comment
  - Files: crates/app/src/main.rs
  - Tests: orphan_position_watchdog_wiring_guard (rewritten, 5 tests)
- [x] Wiring guard rewrite pinning the new topology
  - Files: crates/app/tests/orphan_position_watchdog_wiring_guard.rs
  - Tests: t1..t5 as in Test Plan
- [x] Gate A: shared `SANDBOX_DEADLINE_EPOCH_SECS` constant + engine.rs import + reworded error
  - Files: crates/common/src/constants.rs, crates/trading/src/oms/engine.rs
  - Tests: sandbox_enforcement_guard (rewritten)
- [x] Gate B: LIVE_TRADING_EARLIEST_* → 2099/12/31 + dated doc note
  - Files: crates/common/src/constants.rs
  - Tests: era-aware tests self-adapt; alignment check in sandbox_enforcement_guard
- [x] Gate C: serde default + base.toml → "2099-12-31"
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: inline default-literal test updated
- [x] Loud-expiry tripwire `expired_live_gates` called from validate()
  - Files: crates/common/src/config.rs
  - Tests: test_expired_live_gates_empty_today, test_expired_live_gates_all_at_2100, test_expired_live_gates_per_gate
- [x] Runbook dated update
  - Files: .claude/rules/project/phase-0-item-20-error-codes.md
  - Tests: error_code_rule_file_crossref stays green (section additive)

## Follow-ups (design-only this PR)

**(a) Static-IP ordersAllowed gate re-home.** The Step-6a boot gate
(main.rs `start_dhan_lane`, live-mode-only) never runs post-retirement, yet
authentication.md rule 7 mandates a pre-market `ordersAllowed == true` check.
Sketch: re-home into the Dhan REST stack post-Phase-2 as a fail-closed
process-global `orders_allowed: AtomicBool` DEFAULT FALSE that `place_order`
must read — a check failure pages and stays false, never silently allows.
Operator decisions needed before implementation: halt-vs-degrade on
`ordersAllowed == false` (the stack is fail-soft/never-halts by design), and
whether the check runs in sandbox/dry-run (today's `is_live()` skip means zero
live verification of the IP surface). Flagged as a follow-up PR.

**(b) Kill-switch / exit-all emergency-stop orchestration.** All five
traders-control client fns exist (`exit_all_positions`,
`activate_kill_switch`, etc.) with ZERO production callers. Sketch: an
`emergency_stop` app module sequencing exit_all → re-query positions (verify
flat) → activate_kill_switch → get_kill_switch_status (verify), token from the
global TokenManager, idempotent/re-entrant; needs a new Critical
NotificationEvent + a new ErrorCode + the MISSING
`.claude/rules/dhan/traders-control.md` rule file (index drift — the CLAUDE.md
rule index lists it but it does not exist on disk). Recommended trigger: a
manual bearer-gated endpoint first (never auto). Depends on (a) for the
static-IP prereq on order APIs. Flagged as a follow-up PR.
