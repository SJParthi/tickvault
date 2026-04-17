# Implementation Plan: WebSocket Boot-Path Parity + 9:12 Phase 2 Scheduler

**Status:** VERIFIED (4 of 4 in-scope items landed; Gap C deferred with design in docs)
**Date:** 2026-04-17
**Approved by:** Parthiban ("Execute everything bro")

## Session outcome

**Committed this branch:**
- Fix A+B (53c939a) — `emit_websocket_connected_alerts` helper + `WebSocketPoolOnline` summary; guard test enforces parity.
- Gap #9 (dd2ba03) — pool watchdog Degraded/Recovered/Halt Telegram events.
- Gap #4 (f47a22d) — depth index-LTP timeout Telegram + ERROR log.
- Fix D (2ae9d3f) — websocket-complete-reference §10.1+§10.2 + enforcement rule doc updated with three new resolved entries and three remaining HIGH-severity open gaps (O1/O2/O3).

**Deferred to follow-up PRs (design sketches in `docs/architecture/websocket-complete-reference.md` §10.2):**
- O1 — 9:12 AM Phase 2 stock F&O subscription scheduler (est. 2–3 days; needs new `SubscribeCommand` channel on main-feed connections).
- O2 — `OrderUpdateConnected` timing vs auth ACK (est. 0.5 day).
- O3 — stale-spot-price detection on depth rebalancer (est. 0.5 day).

**Verified not actually gaps (audit corrected):**
- Gap #5 (depth post-market guard) — already guarded at `tick_processor.rs:1006`.
- Gap #6 (depth rebalance Telegram) — already wired at `main.rs:2586`.

## Problem (from live telegram 2026-04-17)

At 8:57 AM Parthiban saw only 3 of 5 `WebSocket #N connected` alerts. At 9:04
AM the app crashed (shutdown initiated after depth rebalance). At 9:05 AM it
restarted via FAST BOOT path and reported `ticks flowing, all services ready`
— but **zero per-connection alerts**. Root-cause audit exposed three separate
bugs plus a missing feature.

## Gaps being closed

| # | Gap | Fix |
|---|-----|-----|
| A | FAST BOOT path (`main.rs:830-840`) emits zero `WebSocketConnected` alerts while slow boot (`main.rs:1747-1766`) emits 5 | Extract helper + call from both paths; mechanical guard test for parity |
| B | When 5 alerts fire in a tight loop, Telegram drops some | Aggregate "N/5 online" summary alert so operator still gets signal even if individuals drop |
| C | 9:12 AM Phase 2 stock F&O subscription is commented / documented but not implemented in code | New `phase2_scheduler.rs`: tokio task spawned from both boot paths that waits until 9:12 IST, rebuilds subscription plan with real spot prices, issues subscribe messages, telegram-alerts on success + failure |
| D | `docs/architecture/websocket-complete-reference.md` §10.2 claims "no open gaps" — stale | Update to reflect A/B/C fix record |

## Plan Items

- [ ] **A1**: Extract `emit_websocket_connected_alerts(notifier, count)` helper.
  - Files: `crates/app/src/main.rs`
  - Tests: `test_emit_websocket_connected_alerts_fires_n_events`

- [ ] **A2**: Mechanical guard test for parity.
  - Files: `crates/app/tests/boot_path_notify_parity.rs` (new)
  - Tests: `fast_boot_and_slow_boot_both_call_emit_websocket_connected_alerts`

- [ ] **B1**: Aggregate summary alert + small stagger between per-connection alerts.
  - Files: `crates/app/src/main.rs` (helper from A1), `crates/core/src/notification/events.rs`
  - Tests: `test_websocket_pool_online_summary_event`

- [ ] **C1**: New `NotificationEvent::Phase2SubscriptionComplete` + failure variant.
  - Files: `crates/core/src/notification/events.rs`
  - Tests: `test_phase2_events_format`, `test_phase2_events_roundtrip`

- [ ] **C2**: New `phase2_scheduler.rs` — tokio task.
  - Files: `crates/core/src/instrument/phase2_scheduler.rs` (new), `crates/core/src/instrument/mod.rs`
  - Tests: `test_phase2_wait_duration_before_912`, `test_phase2_wait_duration_after_912_runs_immediately`, `test_phase2_skips_weekend`

- [ ] **C3**: Wire scheduler into both boot paths.
  - Files: `crates/app/src/main.rs`

- [ ] **D1**: Update WS reference doc + websocket-enforcement.md gap tables.
  - Files: `docs/architecture/websocket-complete-reference.md`, `.claude/rules/project/websocket-enforcement.md`

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Slow boot at 8:57 AM with 5 WS | 5 `WebSocketConnected` alerts + 1 summary "5/5 online" |
| 2 | Crash + FAST BOOT at 9:05 AM | Same 5 + 1 summary alerts (parity with slow boot) |
| 3 | FAST BOOT after 9:12 AM | Phase 2 scheduler detects past-due, runs immediately |
| 4 | Telegram drops some per-connection alerts | Summary alert still tells operator "5/5 online" |
| 5 | Phase 2 runs on Sunday | Scheduler no-ops, no bogus subscription |

## Rollback

Each fix = separate commit. Revert any one independently.
