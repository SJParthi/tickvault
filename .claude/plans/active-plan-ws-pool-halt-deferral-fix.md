# Implementation Plan: WS Pool Watchdog — pre-market deferral must not trip the 09:00 Halt

**Status:** VERIFIED
**Date:** 2026-06-03
**Approved by:** Parthiban (chose "Fix WS-pool-halt now (PR)" + "Fresh branch off main")

## Problem (live, observed 2026-06-03 09:00 IST)

Every market open fires two `[HIGH]` Telegram pages for a benign, self-healing event:
`WS POOL HALT — All connections down for 1055s. Exiting process…` → supervisor restart → `FAST BOOT`.

**Root cause (confirmed in code):**
- The main-feed pool is intentionally **DEFERRED until 09:00 IST** — it opens **zero**
  TCP sockets pre-market (Dhan resets idle pre-market sockets). So every connection reads
  "down" during 08:42→09:00.
- `PoolWatchdog` (`crates/core/src/websocket/pool_watchdog.rs`) stamps `AllDown { since }`
  at the first off-hours poll (~08:42) and accumulates.
- The `spawn_pool_watchdog_task` outer gate (`is_within_market_hours_ist()`) only gates the
  Telegram + `std::process::exit(2)` **side effects** — it does NOT reset the carried `since`.
- At **09:00:00** the gate flips true; the very first in-hours poll sees `down_for ≈ 1055s
  > POOL_HALT_SECS (300s)` → `Halt` → `WebSocketPoolHalt` `[HIGH]` + `exit(2)` → restart → `FAST BOOT`.

So the deferral window and the market-open boundary race, **guaranteeing one forced restart
+ two HIGH pages every trading day**. It self-heals in 0.8s but is avoidable churn + pager fatigue.

## Fix

Reset the watchdog on every **off-hours** poll so the pre-market deferred window never
accumulates a stale `AllDown { since }` into the 09:00 boundary. The first in-hours poll then
starts a **fresh 300s window**, giving the deferred feed its normal reconnect time. The genuine
in-market "all connections down for 300s → halt" safety property is **unchanged**.

## Plan Items

- [x] Item 1 — `PoolWatchdog::reset()` returns the watchdog to `Healthy` + clears `degraded_alert_fired`.
  - Files: `crates/core/src/websocket/pool_watchdog.rs`
  - Tests: `test_watchdog_reset_returns_to_healthy_from_alldown`, `test_watchdog_reset_restarts_down_timer`

- [x] Item 2 — `WebSocketConnectionPool::reset_watchdog()` thin lock+delegate to `PoolWatchdog::reset`.
  - Files: `crates/core/src/websocket/connection_pool.rs`
  - Tests: TEST-EXEMPT (thin lock+delegate; `PoolWatchdog::reset` is unit-tested) — matches the
    existing `poll_watchdog` TEST-EXEMPT precedent in the same file.

- [x] Item 3 — `spawn_pool_watchdog_task` calls `pool.reset_watchdog()` after the verdict match
  when `!in_market_hours`, so the deferred window can't carry a stale `since` into 09:00.
  - Files: `crates/app/src/main.rs`
  - Tests: (covered by the ratchet in Item 4 + behavioural unit tests in Item 1)

- [x] Item 4 — Extend the ratchet so a future refactor can't drop the off-hours reset.
  - Files: `crates/app/tests/post_market_pool_halt_guard.rs`
  - Tests: `pool_watchdog_is_reset_outside_market_hours`

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Pre-market deferred 08:42→09:00, feed connects at 09:00:05 | NO Halt, NO restart, NO HIGH page; Recovered |
| 2 | Genuine all-down DURING market hours for 300s | Halt + `exit(2)` still fires (unchanged) |
| 3 | Post-market all-down (Dhan idle after 15:30) | reset each poll → no Halt, no page (unchanged behaviour, now also no stale carry) |
| 4 | Feed fails to connect at open, still down at 09:05 | Halt fires legitimately (fresh 300s window from 09:00) |

## Out of scope (deliberate, one-PR-at-a-time per operator lock)

- `FAST BOOT` severity `[HIGH]`→`LOW`: deferred to a small follow-up PR. This fix removes the
  daily FAST-BOOT-at-open at the source; the severity question for **genuine** mid-market crash
  recoveries touches the notification taxonomy + `boot_path_notify_parity.rs` and deserves its own PR.

## Guarantee matrix

This is a focused bug-fix PR. Per `.claude/rules/project/per-wave-guarantee-matrix.md` it
cross-references the canonical 15-row + 7-row matrices rather than re-inlining them. Key cells:
- 100% testing coverage: unit (watchdog reset behaviour) + source-scan ratchet (Item 4) + existing
  `post_market_pool_halt_guard.rs` suite stays green.
- Zero new hot-path allocation: `reset()` is a `&mut self` field write; `reset_watchdog()` is a
  cold-path (5s cadence) lock; no allocation. O(1).
- No new tick-drop path; `SubscribeRxGuard` + pool reconnect untouched.
- Honest envelope: this removes a **spurious** restart; it does NOT promise "WS never restarts" —
  a genuine 300s in-market all-down still halts+restarts by design.
