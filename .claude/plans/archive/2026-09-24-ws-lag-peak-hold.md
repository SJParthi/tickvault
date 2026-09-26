# Implementation Plan: minute-peak feed delay + ring-wait gauges that the alarm can actually see

**Status:** APPROVED
**Date:** 2026-09-24
**Approved by:** Parthiban (operator) — "i dont want any agps ir any issues dude can you coevr all tehse dude okay?Always achieve O(1) everywhere."

Authority: `.claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md` §2.3u addendum (2026-09-24);
cost: `.claude/rules/project/aws-budget.md` "COST NOTE 2026-09-24".

## Design

Crate `app`, file `crates/app/src/dhan_feed_stack.rs`.

- [x] `PeakHoldCell`: three atomics (window start, in-progress max, last completed max).
  A pure `peak_hold_step(state, sample, now_ms, window_ms) -> (state, published)`
  decides; the cell only loads and stores. Window = `PEAK_HOLD_WINDOW_MS` = 60,000,
  equal to the CloudWatch agent's collection interval.
- [x] `publish_fold_depth` publishes `RING_DWELL_PEAK.publish(take_ring_dwell_max_ms(), now)`
  instead of the raw reset-on-read value.
- [x] New `record_ws_lag_max` beside `record_ws_lag`, fed only for a NEW folded trade on
  a live socket (`connection_index != u8::MAX`, `IngestOutcome::Folded` without
  `repeat_quote`). `tv_dhan_ws_lag_max_ms` published through its own peak hold.
- [x] `ws_lag_whole_ms` extracted so the histogram and the maximum compute the same number.
- [x] EMF selector + count pin 98 → 99 + dashboard line.

Complexity: every per-tick and per-publish step is O(1), zero allocation.

## Edge Cases

- First window after boot: publishes its in-progress max (never a stale 0 from "no previous window").
- A publish gap longer than a window (drain stalled): the completed window closes on the next
  publish; windows with no publish at all publish nothing — the stall itself is the ring-dwell spike.
- Clock: monotonic `Instant`, so a wall-clock step cannot corrupt the window.
- Lag before any trade: 0, published as 0 (not "missing") after the first publish.
- Replayed WAL frames, repeated quotes, pre-2020 sentinel timestamps: excluded from the lag max.

## Failure Modes

- Peak hold misses a window boundary: the max is carried into the next window, never dropped.
- `u32` overflow on a pathological lag: saturates at `u32::MAX`, never wraps.

## Test Plan

- `test_peak_hold_first_window_publishes_in_progress_max`
- `test_peak_hold_publishes_completed_window_after_rollover`
- `test_peak_hold_gap_spanning_windows_keeps_the_last_completed_max`
- `test_peak_hold_window_equals_the_agent_collection_interval`
- `test_ws_lag_whole_ms_matches_the_histogram_arithmetic`
- `test_ws_lag_max_takes_the_largest_and_resets`
- `ring_dwell_gauge_guard` updated; `cloudwatch_app_alarms_wiring` count 99; `dashboard_live_lane_visibility_guard` green.

## Rollback

Revert the PR. No schema change. The ring-dwell alarm keeps its threshold either way.

## Observability

- `tv_dhan_feed_ring_dwell_max_ms` — same name, now the completed-minute peak.
- `tv_dhan_ws_lag_max_ms` — new, EMF-selected, on the dashboard, unalarmed.

## Per-Item Guarantee Matrix

See per-wave-guarantee-matrix.md. All 15 rows of the guarantee matrix and all 7
rows of the resilience matrix apply to every item in this plan. Where a row does
not apply:

- Audit table: N/A — a gauge, not an event.
- Zero tick loss / WS reconnect: this change adds two atomic ops per new folded tick
  and touches no socket and no persistence path.
- Alerting: no new alarm (§2.3n lever rule not satisfied); the existing ring-dwell
  alarm becomes able to see the spikes it was built for.

Every other row is proven by the tests listed under Test Plan, inside the tested
envelope, with ratcheted regression coverage.
