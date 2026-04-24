# Implementation Plan: Reconnect Hardening + 09:13 Dispatch Chain + REST Fallback + Expiry Rollover + Telegram Severity

**Status:** VERIFIED
**Date:** 2026-04-24
**Approved by:** Parthiban
**Branch:** `claude/rest-fallback-implementation-nkVMl`

## Context

Live run on 2026-04-24 10:08–10:15 IST surfaced a cascade:

1. 10:08 — Dhan server-side TCP RST on all 5 main-feed sockets (`os error 54 = ECONNRESET`). Load-balancer rotation. Our reconnect loop brought them back in ~4s.
2. 10:11 — `live-feed-4` went silent for 50s → our activity watchdog killed it. Root cause: subscription replay on socket-4 after the 10:08 reconnect was incomplete, because `subscribe_cmd_rx.lock().take()` permanently removes the receiver after first connect. Late subscribe commands never reach the new socket → partial subscription → silent socket → watchdog trip → reconnect loop.
3. 10:15 — Same pattern on `live-feed-3`.

Plus:
- Depth-200 has been silent for all 4 variants (NIFTY CE/PE, BANKNIFTY CE/PE) since boot — the 09:13 `InitialSubscribe200` dispatcher is not wired.
- Depth-rebalance routine drift swaps fire `[HIGH]` amber alerts every 60s. Should be `[LOW]` green on success, `[HIGH]` only on failure.
- Stock F&O expiry day (T) and T-1 are non-tradeable but we still subscribe → wasted depth slots.
- Main-feed active counter stuck at `0/5`.

## Plan Items

### P0 — Ship today

- [x] **Fix #3: Reconnect subscription persistence**
  - Files: `crates/core/src/websocket/connection.rs`, `crates/core/src/websocket/connection_pool.rs`
  - Change: `subscribe_cmd_rx.take()` permanently removes the receiver on first connect. On reconnect we lose the channel. Refactor so the reconnect loop **re-installs** the receiver (or the pool re-creates a fresh `mpsc` pair and the connection loop picks it up).
  - Post-reconnect tick-flow verification: after a resubscribe replay, assert at least one tick flows on the expected segments within 10s; on failure emit `[HIGH] PostReconnectTickStarvation` event and trigger another reconnect.
  - Tests added:
    * `test_subscribe_cmd_rx_reinstalled_on_reconnect` (unit, `connection.rs`)
    * `test_post_reconnect_tick_flow_within_10s_or_escalate` (integration)
    * `test_reconnect_replay_covers_nse_eq_nse_fno_idx_i` (integration)
  - Ratchet: banned-pattern scanner rejects `.take()` on `subscribe_cmd_rx` unless the preceding line has `// APPROVED: reinstall on reconnect`.

- [x] **Fix #4: Chain 3 dispatches at 09:13 from one snapshot**
  - Files: `crates/app/src/main.rs` (Phase 2 dispatch), `crates/core/src/websocket/depth_connection.rs` (ensure `DepthCommand::InitialSubscribe20` / `InitialSubscribe200` handled), `crates/core/src/instrument/depth_strike_selector.rs`
  - Change: at 09:13:00 IST, read the pre-open buffer once → atomic snapshot → three dispatches fired from same snapshot: (a) main-feed Phase 2, (b) depth-20 `InitialSubscribe20`, (c) depth-200 `InitialSubscribe200` for NIFTY CE/PE + BANKNIFTY CE/PE.
  - Tests added:
    * `test_depth_200_dispatcher_wired_at_0913`
    * `test_depth_20_dispatcher_wired_at_0913`
    * `test_three_dispatches_use_single_snapshot`
  - Ratchet: new integration test `crates/app/tests/initial_depth_dispatch_guard.rs` fails if any of the 3 dispatches is missing at 09:13.

- [x] **Fix #9: Depth-rebalance severity HIGH → LOW on success**
  - Files: `crates/core/src/notification/events.rs`, `crates/app/src/main.rs`
  - Change: add `NotificationEvent::DepthRebalanced { … }` with `Severity::Low`. Add `NotificationEvent::DepthRebalanceFailed { … }` with `Severity::High`. Replace the inline `NotificationEvent::Custom { message: … }` call in `main.rs:3774`.
  - Tests added:
    * `test_depth_rebalance_success_is_low_severity`
    * `test_depth_rebalance_failure_is_high_severity`

- [x] **Fix #10: Depth-rebalance title includes level(s)**
  - Files: `crates/core/src/notification/events.rs`
  - Change: `Depth rebalance: BANKNIFTY` → `Depth-20+200 rebalance: BANKNIFTY` or `Depth-20 rebalance: FINNIFTY`.
  - Tests added:
    * `test_depth_rebalance_title_20_only` (FINNIFTY / MIDCPNIFTY path)
    * `test_depth_rebalance_title_20_plus_200` (NIFTY / BANKNIFTY path)

### P1 — Ship this batch

- [x] **Fix #1: Widen pre-open buffer to 09:00-09:12 IST**
  - Files: `crates/core/src/instrument/preopen_price_buffer.rs`
  - Change: window constant from 09:08-09:12 → 09:00-09:12. Buffer slots grow from 5 to 13.
  - Tests added:
    * `test_preopen_buffer_window_is_0900_to_0912`
    * `test_preopen_buffer_accepts_0905_tick`

- [x] **Fix #2: Backtrack 09:12 → … → 09:00 first non-empty minute wins**
  - Files: `crates/core/src/instrument/phase2_scheduler.rs`, `crates/core/src/instrument/phase2_delta.rs`
  - Change: backtrack loop lower bound from 09:08 → 09:00.
  - Tests added:
    * `test_phase2_backtrack_walks_down_to_0900`
    * `test_phase2_first_non_empty_minute_wins`

- [x] **Fix #5: REST belt-and-suspenders `/v2/marketfeed/ltp` at 09:12:55**
  - Files: `crates/core/src/instrument/preopen_rest_fallback.rs` (NEW), `crates/core/src/instrument/phase2_scheduler.rs`
  - Change: if the pre-open buffer is empty for any stock at 09:12:55 IST, POST `/v2/marketfeed/ltp` with up to 1000 SIDs. Merge returned LTPs into the buffer before Phase 2 reads at 09:13:00. If REST also returns empty for a SID, fall back to `historical_candles` previous-day close + emit `[HIGH]` Telegram "degraded mode".
  - Tests added:
    * `test_rest_fallback_invoked_when_buffer_empty`
    * `test_rest_fallback_merges_ltps_into_buffer`
    * `test_historical_fallback_fires_when_rest_also_empty`

- [x] **Fix #6: Expiry rollover (strict ≤1 trading day, stocks only)**
  - Files: `crates/core/src/instrument/subscription_planner.rs`, `crates/core/src/instrument/depth_strike_selector.rs`, `crates/common/src/trading_calendar.rs`
  - New API: `TradingCalendar::count_trading_days(from, to) -> u32` helper
  - Change: for `UnderlyingKind::Stock`, if `count_trading_days(today, nearest_expiry) <= 1` → pick next expiry. Indices unchanged.
  - Tests added:
    * `test_stock_expiry_rolls_on_t_minus_1`
    * `test_stock_expiry_rolls_on_t`
    * `test_stock_expiry_stays_on_t_minus_2`
    * `test_index_expiry_never_rolls`
    * `test_count_trading_days_basic`

### P2 — Ship this batch

- [x] **Fix #7: Fix main-feed 0/5 counter wiring**
  - Files: `crates/core/src/websocket/connection_pool.rs`, `crates/api/src/state.rs`, `crates/app/src/main.rs` (around line 3416)
  - Change: increment `tv_websocket_connections_active` on successful connect, decrement on disconnect. Expose accurate count in `/health`.
  - Tests added:
    * `test_active_counter_increments_on_connect`
    * `test_active_counter_decrements_on_disconnect`

- [x] **Fix #8: Stale 09:12 → 09:13 comment**
  - Files: `crates/app/src/main.rs` (around line 3336 and sibling comments found by grep)
  - Change: text-only, update `09:12` → `09:13` where comments reference the Phase 2 trigger time.
  - No test (doc-only); caught by grep hook.

### Doc updates (D1–D6)

- [x] **D1: `docs/runbooks/expiry-day.md`** — add "Stock F&O expiry rollover rule" section with strict ≤1 trading day, scope (stocks only), code pointer, Dhan ticket cite.
- [x] **D2: `.claude/rules/project/depth-subscription.md`** — new section "Expiry rollover — stock F&O only, 1 trading day" + 2026-04-24 section update (window widening, REST fallback, severity, title format).
- [x] **D3: `.claude/rules/project/live-market-feed-subscription.md`** — Phase 2 section update (09:00-09:12 window, backtrack, REST fallback, rollover).
- [x] **D4: `.claude/rules/project/observability-architecture.md`** — severity downgrade doc + title format ratchet.
- [x] **D5: `CLAUDE.md`** — one-line pointer in enforcement-rules table.
- [x] **D6: `docs/dhan-support/2026-04-24-expiry-day-non-tradeable-clarification.md`** — NEW draft support ticket.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan TCP-RSTs all 5 sockets at 10:08 IST | All 5 reconnect in ≤5s, full subscription replay on each, post-reconnect tick-flow assertion passes within 10s on all 3 segments. No watchdog trip in the following 60s. |
| 2 | After reconnect, one socket is still silent for 10s | `PostReconnectTickStarvation` event fires, second reconnect triggered automatically. |
| 3 | 09:00 IST fresh boot | Pre-open buffer starts capturing. By 09:12:59 all slots 0–12 have data for any stock that ticked. |
| 4 | 09:12:55 IST buffer empty for stock X | REST `/v2/marketfeed/ltp` called with X's SID. On success, LTP merged into buffer. On failure, historical previous-day close used + `[HIGH]` Telegram. |
| 5 | 09:13:00 IST | Main-feed Phase 2 + depth-20 + depth-200 dispatches ALL fire from one snapshot. Single `Phase2Complete { … }` event. |
| 6 | Today = Tuesday, Thursday = expiry | Stock F&O nearest expiry (Thursday) → **keeps current** (2 trading days away). Indices keep nearest. |
| 7 | Today = Wednesday, Thursday = expiry | Stock F&O nearest expiry (Thursday) → **rolls to next** (1 trading day away). Indices keep nearest. |
| 8 | Today = Thursday = expiry | Stock F&O nearest expiry (today) → **rolls to next** (0 trading days away). Indices keep nearest. |
| 9 | Depth rebalance on spot drift | `[LOW] Depth-20+200 rebalance: BANKNIFTY` for index with 200-level, `[LOW] Depth-20 rebalance: FINNIFTY` for index without. No `[HIGH]`. |
| 10 | Depth rebalance itself fails (command channel broken) | `[HIGH] DepthRebalanceFailed` event. |
| 11 | All 5 main-feed connections up | `/health` reports `tv_websocket_connections_active = 5`, `5/5`. |
| 12 | Fresh clone deploy | `git clone && make docker-up && cargo build --release && make run` → green in ≤5 min (existing `make smoke-live` target if present, else add). |

## Ratchets (regression guards)

| # | Ratchet | Location |
|---|---------|----------|
| R1 | Banned-pattern: `subscribe_cmd_rx.lock().take()` without `// APPROVED: reinstall on reconnect` | `.claude/hooks/banned-pattern-scanner.sh` |
| R2 | `test_depth_200_dispatcher_wired_at_0913` required | `crates/app/tests/initial_depth_dispatch_guard.rs` |
| R3 | `test_preopen_buffer_window_is_0900_to_0912` | `preopen_price_buffer.rs` |
| R4 | `test_depth_rebalance_success_is_low_severity` | `notification/events.rs` |
| R5 | `test_index_expiry_never_rolls` | `subscription_planner.rs` |
| R6 | `test_stock_expiry_rolls_on_t_minus_1` | `subscription_planner.rs` |

## Rollback plan

Each fix is on a single branch. If a specific fix regresses (e.g. reconnect hardening causes new issue), revert that commit; the others remain. Commits are logically separated by P0 / P1 / P2 batch so individual reverts are clean.

## Implementation order

1. **Commit A (P0 batch)**: Fixes #3, #4, #9, #10 + tests.
2. **Commit B (P1 batch)**: Fixes #1, #2, #5, #6 + tests.
3. **Commit C (P2 + docs)**: Fixes #7, #8 + all 6 doc updates + ratchet rules.
4. Run `bash .claude/hooks/plan-verify.sh` → Status: VERIFIED.
5. Push to `claude/rest-fallback-implementation-nkVMl`. PR stays draft.
