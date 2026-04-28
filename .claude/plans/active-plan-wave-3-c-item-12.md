# Implementation Plan: Wave-3-C Item 12 — Market-Open Self-Test

**Status:** APPROVED
**Date:** 2026-04-28
**Approved by:** Parthiban (carte blanche: "you just fix everything dude okay")
**Branch:** `claude/automation-charter-setup-QNGYi` (harness override of SCOPE deliverable name)
**Parent plan:** `.claude/plans/active-plan-wave-3.md` (Item 12 row)

## Reconciliation of prompt conflicts

| Conflict | Resolved as | Why |
|---|---|---|
| Branch | `claude/automation-charter-setup-QNGYi` | Harness git instruction overrides SCOPE deliverable |
| Trigger time | 09:16:00 IST | Avoids collision with existing 09:15:30 `MarketOpenStreamingConfirmation` heartbeat |
| Outcomes | 3 (Passed/Degraded/Critical) per SCOPE §12.2 | Matches tri-state mental model + maps cleanly to Severity |
| Sub-checks | SCOPE §12.1 list (7 checks) | Self-contained; doesn't depend on Wave 3 Items 10/11 |
| Kill-switch coupling | Deferred | SCOPE doesn't mandate; operator can layer later |
| Tests | 6 minimum per SCOPE §12.5 | Pure-logic table-driven |

## Plan Items

- [x] **1. Pure-logic module** — `crates/core/src/instrument/market_open_self_test.rs` (NEW)
  - `MarketOpenSelfTestInputs` struct (7 fields per SCOPE §12.1)
  - `MarketOpenSelfTestOutcome` enum: `Passed { checks_passed }` / `Degraded { passed, failed, names }` / `Critical { failed, names }`
  - `evaluate_self_test(&Inputs) -> Outcome` pure function
  - Tests: 6 named per SCOPE §12.5

- [x] **2. NotificationEvent variants** — `crates/core/src/notification/events.rs`
  - `SelfTestPassed { checks_passed }` Severity::Info
  - `SelfTestDegraded { checks_passed, checks_failed, failed }` Severity::High
  - `SelfTestCritical { checks_failed, failed }` Severity::Critical
  - Routing in `severity()` and message in `to_message()`

- [x] **3. ErrorCode variants** — `crates/common/src/error_code.rs`
  - `Selftest01Passed` ("SELFTEST-01", Severity::Info, never auto-triage)
  - `Selftest02Failed` ("SELFTEST-02", Severity::Critical, never auto-triage)
  - Runbook path → `.claude/rules/project/wave-3-c-error-codes.md`
  - Append both to `all()` list

- [x] **4. Runbook** — `.claude/rules/project/wave-3-c-error-codes.md`
  - Existing file already covers BOOT-03; extend with SELFTEST-01 + SELFTEST-02 sections

- [x] **5. Module registration** — `crates/core/src/instrument/mod.rs`
  - Add `pub mod market_open_self_test;`

## Out of scope (follow-up commits, same PR)

- Boot wiring in `crates/app/src/main.rs` (3,700-line scheduler integration). The flag `market_open_self_test` already exists in `config.rs:111`. Once-per-day 09:16 IST scheduler + persistence call is a follow-up.
- 5-layer telemetry (Prom counters, Grafana panel, alert rule). Module emits structured events; metrics added with the boot wiring.
- 3-agent adversarial review per stream-resilience.md B3. Flagged for next session.

## Scenarios

| # | Scenario | Inputs | Expected |
|---|----------|--------|----------|
| 1 | All green | main=5, d20=4, d200=2, oms=true, pipeline=true, tick_age=10s, qdb=true, token_h=5h | `Passed{7}` |
| 2 | No main feed | main=0 | `Critical` contains `"main_feed_active"` |
| 3 | QuestDB down | qdb=false | `Critical` contains `"questdb_connected"` |
| 4 | Token <4h | token_h=3h | `Critical` contains `"token_expiry_headroom"` |
| 5 | Pipeline inactive | pipeline=false | `Degraded` contains `"pipeline_active"` |
| 6 | Stale tick | tick_age=120s | `Degraded` contains `"recent_tick"` |
