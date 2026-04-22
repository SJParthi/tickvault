# Implementation Plan: Unified 09:12 Close Subscription for Main Feed + Depth-20 + Depth-200

**Status:** DRAFT
**Date:** 2026-04-22
**Approved by:** pending (Parthiban)
**Branch:** `claude/market-feed-depth-explanation-RynUx`

## Background

Today the three subscription paths are split:

- **Main feed stock F&O** subscribes at 09:12:30 IST using the 09:12 close (Phase 2 scheduler — correct).
- **Depth-20** picks initial ATM at boot using whatever index LTP is available (pre-open ~09:00 or stale), then 60s rebalancer corrects it.
- **Depth-200** same as Depth-20 (boot-time guess + rebalancer correction).

Parthiban directive (2026-04-22): **all three must key off the same 09:12 close.** Depth-20 and Depth-200 should not subscribe until 09:12:30 IST, using the 09:12 NIFTY/BANKNIFTY close. Dynamic 60s rebalancer continues only from 09:15 IST onwards.

Tradeoff accepted: pre-open depth blackout 09:00–09:12:30 (12 min).

## Plan Items

- [ ] A. Extend `preopen_price_buffer` to also capture index closes (NIFTY, BANKNIFTY)
  - Files: `crates/core/src/instrument/preopen_price_buffer.rs`
  - Current lookup table is F&O-stocks-only (line 226 comment). Add parallel index path so ticks for IDX_I security_ids (13, 25) also record into the buffer under the symbol key ("NIFTY", "BANKNIFTY").
  - Tests: `test_preopen_buffer_records_nifty_idx_i_close`, `test_preopen_buffer_records_banknifty_idx_i_close`

- [ ] B. Defer depth-20 + depth-200 subscribe until 09:12:30 IST
  - Files: `crates/app/src/main.rs` (Step 8c block ~line 2044-2925)
  - Remove boot-time "wait 30s for LTP → select_depth_instruments → spawn depth" flow.
  - Instead: spawn depth WS tasks in a holding state (connected + authenticated but NOT subscribed). They sit idle waiting for a "depth-subscribe" command.
  - Tests: `test_depth_tasks_idle_before_0912_ist` (integration)

- [ ] C. Phase 2 scheduler extension — dispatch depth subscriptions at 09:12:30
  - Files: `crates/core/src/instrument/phase2_scheduler.rs`
  - After the existing stock-F&O dispatch, call a new `dispatch_depth_subscriptions()` that:
    1. Reads 09:12 close from preopen buffer for NIFTY + BANKNIFTY
    2. Picks ATM ± 24 strikes per index (depth-20) + ATM CE + ATM PE (depth-200)
    3. Sends `DepthCommand::InitialSubscribe` to each depth connection's mpsc channel
  - Tests: `test_phase2_dispatches_depth_at_0912`, `test_phase2_depth_uses_0912_close_not_earlier`

- [ ] D. New `DepthCommand::InitialSubscribe` variant + handling
  - Files: `crates/core/src/websocket/depth_connection.rs`
  - Add variant alongside existing `Swap20` / `Swap200`. First subscribe on the socket. After this fires, socket accepts subsequent `Swap*` commands.
  - Tests: `test_depth_initial_subscribe_sends_request_code_23`, `test_depth_initial_subscribe_only_fires_once`

- [ ] E. Depth rebalancer 09:15 IST gate for first check
  - Files: `crates/core/src/instrument/depth_rebalancer.rs`
  - Current `is_within_market_hours_ist` gate uses 09:00 start (tick_persist window). Add second gate: first rebalance check only fires at ≥ 09:15 IST so we don't rebalance immediately on top of the 09:12:30 initial subscribe (3-minute settle).
  - Tests: `test_rebalancer_first_check_skipped_before_0915_ist`, `test_rebalancer_runs_normally_after_0915_ist`

- [ ] F. Crash-recovery path — 09:12:30 snapshot extended to depth
  - Files: `crates/core/src/instrument/phase2_scheduler.rs` + the on-disk snapshot writer (PROMPT C path)
  - Snapshot now contains: {stock F&O subscriptions, depth-20 ATM contracts, depth-200 ATM contracts}
  - On crash-restart after 09:12:30 → replay all three from snapshot, not just stock F&O.
  - Tests: `test_snapshot_roundtrip_includes_depth_selections`, `test_crash_recovery_dispatches_depth_from_snapshot`

- [ ] G. Telegram notification for unified Phase 2 complete
  - Files: `crates/core/src/notification/events.rs`
  - Extend `Phase2Complete` event to include depth counts: `stock_fno_added`, `depth_20_underlyings`, `depth_200_underlyings`.
  - Message format: "Phase 2 complete at 09:12:30 — stock F&O: +6,123 | depth-20: NIFTY/BANKNIFTY/FINNIFTY/MIDCPNIFTY | depth-200: NIFTY CE/PE, BANKNIFTY CE/PE"
  - Tests: `test_phase2_complete_telegram_includes_depth_counts`

- [ ] H. Rule file update — record the new canonical behavior
  - Files: `.claude/rules/project/depth-subscription.md`, `.claude/rules/project/live-market-feed-subscription.md`
  - Replace "boot waits up to 30s for LTPs" with "09:12:30 IST scheduler dispatches all three subscriptions from the 09:12 close".
  - No test (docs) — but the integration tests in C/E cover the behavior.

- [ ] I. Ratchet guard test
  - Files: `crates/core/tests/phase2_unified_depth_guard.rs` (new)
  - Source-scans `main.rs` to verify depth connections no longer subscribe at boot (no `select_depth_instruments` call in boot path). Source-scans `phase2_scheduler.rs` for the new `dispatch_depth_subscriptions` call.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot at 08:00, market day | Main feed subscribes Phase 1 (indices + index F&O + stock equities). Depth WS tasks spawn idle. No depth subscribe until 09:12:30. |
| 2 | 09:00–09:12 IST | Pre-open ticks flow for indices + stocks. Buffer captures NIFTY, BANKNIFTY, and F&O stock closes per minute. No depth data flowing. |
| 3 | 09:12:30 IST | Phase 2 fires. Stock F&O subscribe burst + Depth-20 subscribe (4 underlyings × ~49 contracts) + Depth-200 subscribe (4 contracts). Single Telegram notification. |
| 4 | 09:15 IST onwards | Depth rebalancer starts 60s loop. Main feed stock F&O list frozen for the day. |
| 5 | Crash at 11:00, restart at 11:01 | Snapshot read from disk → stock F&O + depth selections replayed in one dispatch. Depth sockets reconnect and subscribe from snapshot. Rebalancer runs immediately (already past 09:15). |
| 6 | Boot at 10:30 (late start) | Phase 2 `RunImmediate` path fires with `minutes_late: 78`. Same unified dispatch. Rebalancer active immediately. |
| 7 | NIFTY didn't tick 09:12 (extremely rare) | Fallback to 09:11 → 09:10 → 09:09 → 09:08 slot. If all empty → Phase 2 aborts with CRITICAL Telegram, depth stays idle for the day. |
| 8 | Weekend boot | Phase 2 scheduler returns `SkipToday { reason: "weekend" }`. Depth never subscribes. Correct. |

## Out of Scope

- Changing the main feed Phase 1 subscribe (indices + stock equities) — unchanged.
- Changing the 60s depth rebalancer cadence or drift threshold — unchanged.
- Changing the order update WebSocket — unchanged.
- Removing the SharedSpotPrices map — still needed by the 60s rebalancer post-09:15.

## Files Touched (9 files)

1. `crates/core/src/instrument/preopen_price_buffer.rs`
2. `crates/core/src/instrument/phase2_scheduler.rs`
3. `crates/core/src/websocket/depth_connection.rs`
4. `crates/core/src/instrument/depth_rebalancer.rs`
5. `crates/core/src/notification/events.rs`
6. `crates/app/src/main.rs`
7. `crates/core/tests/phase2_unified_depth_guard.rs` (new)
8. `.claude/rules/project/depth-subscription.md`
9. `.claude/rules/project/live-market-feed-subscription.md`

## Risks

- **Pre-open depth blackout (accepted):** no depth data 09:00–09:12:30. Strategies that watch pre-open book lose this signal.
- **09:12 single-slot dependency:** if NIFTY/BANKNIFTY had no tick in the 09:12 minute bucket, fallback walks backward through 09:11..09:08. If all 5 slots are empty (extreme outage), Phase 2 aborts — depth idle for the day. Mitigation: CRITICAL Telegram.
- **Snapshot schema change:** existing on-disk snapshot format must be extended. Old snapshots become unreadable — one-time skip on first deploy (documented in PROMPT C comment).
