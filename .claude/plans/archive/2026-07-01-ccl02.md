# Implementation Plan: CCL-02 — DayOhlc IST-midnight reset supervisor

**Status:** APPROVED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — audit fix-queue item, permutation-coverage-audit-2026-07-01.md §141

> **Guarantee matrices:** carried by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory per `per-item-guarantee-check.sh`). All 15 rows of the 100% Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply to every item in this plan.

## Design

The IST-midnight daily reset of the index DayOhlc tracker
(`crates/trading/src/in_mem/day_ohlc_tracker.rs::reset_daily_all`, orchestrator
`crates/app/src/day_ohlc_orchestrator.rs::spawn_midnight_reset_task`) runs as a
bare `tokio::spawn` loop with NO `catch_unwind`, NO respawn, and NO failure
counter. If the task panics mid-iteration or exits unexpectedly, yesterday's day
high/low/close silently carry over to the next trading day until the first live
tick re-arms — and the operator gets ZERO signal.

The `ErrorCode::IndexOhlc02DailyResetFailed` variant (`INDEX-OHLC-02`,
`Severity::High`) and its runbook `index-day-ohlc-tracker-error-codes.md` already
exist, but the variant has ZERO emit site and the runbook cites a
`tv_day_ohlc_reset_failures_total` counter that grep = zero hits (the runbook was
lying).

**Fix (mirror WS-GAP-05 / DISK-WATCHER-01 respawn):** wrap the reset task in a
supervised respawn wrapper `spawn_supervised_midnight_reset_task`. On every death
of the inner reset task it: logs `error!(code = INDEX-OHLC-02)`, increments
`tv_day_ohlc_reset_failures_total{reason}`, backs off
`DAY_OHLC_RESET_RESPAWN_BACKOFF_SECS`, then respawns so the midnight reset keeps
firing. The classification is a pure `classify_reset_task_exit` (same 4-label
scheme as `disk_health_watcher::classify_join_exit`) so the branch logic is unit
testable without constructing a real `JoinError`. `main.rs` swaps
`spawn_midnight_reset_task` → `spawn_supervised_midnight_reset_task`.

No `tickvault_common` change: the ErrorCode variant already exists, so no
`error_code.rs` edit, no new crossref/tag-guard entry needed.

## Plan Items

- [x] Item 1 — Add supervised respawn wrapper + pure exit classifier to the orchestrator.
  - Files: `crates/app/src/day_ohlc_orchestrator.rs`
  - Adds: `DAY_OHLC_RESET_RESPAWN_BACKOFF_SECS` const, `classify_reset_task_exit`
    (pure), `spawn_supervised_midnight_reset_task` (respawn loop emitting
    `INDEX-OHLC-02` + `tv_day_ohlc_reset_failures_total{reason}`).
  - Tests: `test_classify_reset_task_exit_clean`, `test_classify_reset_task_exit_panic`,
    `test_classify_reset_task_exit_cancelled`,
    `test_reset_respawn_backoff_is_small_but_nonzero`,
    `test_spawn_supervised_midnight_reset_task_keeps_running`

- [x] Item 2 — Swap main.rs boot wiring to the supervised spawn.
  - Files: `crates/app/src/main.rs`
  - Tests: `test_day_ohlc_orchestrator_is_wired_into_main` (updated in secret_manager.rs)

- [x] Item 3 — Update the wiring source-scan ratchet to require the supervised spawn.
  - Files: `crates/core/src/auth/secret_manager.rs`
  - Tests: `test_day_ohlc_orchestrator_is_wired_into_main`

- [x] Item 4 — Correct the runbook so it names the supervisor + the now-real counter.
  - Files: `.claude/rules/project/index-day-ohlc-tracker-error-codes.md`

## Edge Cases

- Inner reset task panics mid-iteration → supervisor join yields `is_panic()` →
  `reason="panic"` → INDEX-OHLC-02 + counter + respawn. (parking_lot mutex does
  NOT poison, so this vector is near-unreachable in practice — but the supervisor
  makes it non-silent if it ever fires, which is the audit's point.)
- Inner task cancelled at shutdown → `reason="cancelled"` → benign; still respawns
  (the supervisor is an infinite loop bound to `_` in main; it is not shut down
  independently of the process).
- Inner task returns cleanly (would only happen if the reset loop were ever made
  finite) → `reason="clean_exit"` → respawn keeps midnight resets firing.
- Backoff is non-zero so a task that panics instantly on every start cannot
  busy-spin the CPU; small so the midnight reset resumes quickly.

## Failure Modes

- If the supervisor itself panicked it would take the reset offline — mitigated
  exactly as DISK-WATCHER-01: the supervisor body has NO panic path of its own
  (no `unwrap`/`expect`, pure-function classification), so no
  supervisor-of-supervisor is required.
- If QuestDB/metrics sink is down, the counter increment is a no-op; the `error!`
  still routes through the 5-sink error pipeline to Telegram.

## Test Plan

- Pure classifier truth-table (clean / panic / cancelled) — 3 `#[tokio::test]`.
- Backoff-constant bounds test.
- Supervisor liveness: the wrapper's `JoinHandle` must NOT resolve in normal
  operation (mirror `test_spawn_supervised_spill_disk_health_watcher_keeps_running`).
- `cargo test -p tickvault-trading --lib` (tracker unchanged, sanity), `-p tickvault-app`,
  `-p tickvault-core` (wiring guard).

## Rollback

- Single-commit revert restores the bare `spawn_midnight_reset_task` wiring. No
  schema, no config, no ErrorCode, no data migration involved — pure task-lifecycle
  change. `spawn_midnight_reset_task` is retained (not deleted) so a revert of only
  the main.rs line is sufficient.

## Observability

- New counter `tv_day_ohlc_reset_failures_total{reason}` — now REAL (runbook
  stops lying).
- `error!(code = "INDEX-OHLC-02", reason, backoff_secs, ...)` on every reset-task
  death → routes to Telegram via the standard error pipeline (Severity::High).
- Layer coverage: counter (1), tracing error! with code field (4), Telegram event
  via error routing (5). Audit-table (6) N/A — this is a task-lifecycle liveness
  signal, not a per-event SEBI record (same class as DISK-WATCHER-01, which also
  has no audit table).
