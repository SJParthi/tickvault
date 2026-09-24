# Implementation Plan: same-day retry for the 1-minute cross-verification

**Status:** APPROVED
**Date:** 2026-09-24
**Approved by:** Parthiban (operator) — "fix the same-day retry gap and merge deploy it dude"

Authority: `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md` §12.15.5.

## Design

Crate `app`, file `crates/app/src/dhan_live_crossverify_boot.rs`.

- `run_once` returns `Result<(), AttemptFailure>`; `Ok` only when the day marker was written.
- `run_day` loops: attempt → on failure ask the pure `retry_delay_secs(attempts, now, attempt_max_secs)` → sleep 900 s or page once via `report_final_failure`.
- `classify_attempt(vacuous, measured, persisted_ok, complete)` replaces the inline marker rule; proven equal to `should_write_marker && run_is_complete`.
- Every decision is a pure O(1) function; the loop is bounded to 4 attempts.

## Edge Cases

- Boot catch-up after ~17:14 IST: no retry (next attempt cannot finish by 17:30).
- Catastrophic divergence on an attempt whose rows did not persist: page once, retry for the marker.
- Degraded outcome with minutes compared: `Incomplete`, retried.
- Day change during the retry sleep: checked, the retry stops.

## Failure Modes

- Every attempt fails: one `error!` page after the last attempt, S3 hold as before.
- Token never arrives: 4 × 5-minute waits, then the page.
- QuestDB flush fails: `xverify_persist_failed` error per attempt (not alarmed), final page `not_persisted`.

## Test Plan

- `test_classify_attempt_agrees_with_the_marker_rule_everywhere`
- `test_classify_attempt_names_the_first_problem`
- `test_retry_delay_secs_bounds`
- `test_worst_case_day_fits_every_attempt_before_the_evening_stop`
- `test_run_day_retries_through_the_pure_bound`
- `test_every_xverify_alarm_source_has_a_live_error_emit` (unchanged, still green)

## Rollback

Revert the PR. No schema, config or alarm change.

## Observability

- `tv_dhan_xverify_retries_total{reason}` (local `/metrics`).
- `warn!` per attempt with `source = xverify_retry` / `xverify_attempt_*`.
- Existing `xverify_failed` / `xverify_vacuous` / `xverify_diverged` alarms, now paged once per day.

## Per-Item Guarantee Matrix

See per-wave-guarantee-matrix.md. All 15 rows of the guarantee matrix and all 7
rows of the resilience matrix apply to every item in this plan. Where a row does
not apply:

- Performance: N/A. This is a cold path that runs once a day, not the hot path.
  The retry decision is O(1). The comparison stays O(minutes × instruments) and
  is flagged as such.
- Zero tick loss / WS reconnect: N/A. This change touches no socket and no tick
  path.
- Alerting: no new alarm and no new metric name. Final failures page through the
  existing `xverify_failed` and `xverify_vacuous` filters.

Every other row is proven by the tests listed under Test Plan. That coverage holds
inside the tested envelope, with ratcheted regression coverage.
