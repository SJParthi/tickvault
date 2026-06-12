# Plan: retire LastTickAfterBoundary alert (redundant with the live counter)

**Status:** APPROVED
**Date:** 2026-06-12
**Approved by:** Parthiban (2026-06-12 — "make everything fully automated". Investigation reversed the
earlier "wire" recommendation: wiring requires hot-path notifier threading for an already-counted Info
event — the honest call is to retire.)
**Branch:** claude/nice-cerf-hemuvn
**Changed crates:** `core` (remove the `LastTickAfterBoundary` NotificationEvent variant + 3 match arms
+ integration-test render), `common` (correct the constants.rs doc comment that referenced it)

## Why
`NotificationEvent::LastTickAfterBoundary` (Info) is defined but has **0 production emit sites**
(grep-verified). The post-close-tick anomaly it describes is **already tracked** by the
`tv_late_tick_after_boundary_total` counter in `tick_processor.rs:1133` (ratchet-tested at line
3772), which increments on every tick stamped ≥ 15:30:00 IST. Wiring the Telegram form would require
threading a `NotificationService` into the **per-tick hot path** (`run_tick_processor` has no
notifier) for an Info-severity event that is already counted — a hot-path risk for marginal value.
The correct, hot-path-safe alert is a **CloudWatch alarm on the existing counter** (no code change to
the hot path). So the variant is retired, consistent with `TokenRenewalDeadlineMissed` (#1116).

## Design
Delete the variant + its 3 match arms (message / event-name / severity) in `events.rs`; remove the
integration-test render in `event_formatting_coverage.rs`; correct the `constants.rs`
`LATE_TICK_ANOMALY_THRESHOLD_MS` doc comment to point at the counter (not the retired variant).

## Edge Cases
- 0 emit sites → no runtime behaviour change. The counter + its ratchet test are UNTOUCHED, so
  post-close ticks are still tracked exactly as before.
- No `#[test]` function removed (only a render statement) → test-count baseline unchanged (6982).

## Failure Modes
- None at runtime (dead-code removal). Compiler enforces exhaustive-match completeness across the 3
  removed arms.

## Test Plan
- `core`: `notification::events` (143) + `event_formatting_coverage` (10) tests green; core builds.
- grep confirms no live `LastTickAfterBoundary` refs remain (only RETIRED comments + archived plans).
- banned-pattern + pub-fn + clippy(my files) clean.

## Rollback
`git revert`. No data path, no hot path, no schema touched.

## Observability
- The post-close-tick anomaly remains observable via `tv_late_tick_after_boundary_total` (unchanged,
  ratchet-tested). Recommended follow-up (deploy-side, no code): a CloudWatch alarm on that counter to
  page the operator — the hot-path-safe equivalent of the retired Telegram variant.

## Per-Item Guarantee Matrix
Carries the 15+7 matrix by reference — `.claude/rules/project/per-wave-guarantee-matrix.md`. Specifics:
100% honesty (no false/duplicate alert; no hot-path risk); the existing counter + ratchet test keep
the anomaly tracked; envelope: this REMOVES a never-fired duplicate, it does not reduce coverage.

## Plan Items
- [x] Remove the `LastTickAfterBoundary` variant + 3 match arms + integration-test render
  - Files: crates/core/src/notification/events.rs, crates/core/tests/event_formatting_coverage.rs
  - Tests: notification::events + event_formatting_coverage (green)
- [x] Correct the constants.rs doc comment to reference the counter, not the retired variant
  - Files: crates/common/src/constants.rs

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | tick stamped ≥ 15:30 IST | `tv_late_tick_after_boundary_total` increments (unchanged) — no Telegram |
| 2 | grep for the variant | only RETIRED comments + archived plans |
