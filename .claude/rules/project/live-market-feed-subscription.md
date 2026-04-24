# Live Market Feed WebSocket Subscription Rules

> **Authority:** CLAUDE.md > this file.
> **Scope:** Any file touching main WebSocket connection pool, subscription planning, or instrument distribution.
> **Ground truth:** `docs/dhan-ref/03-live-market-feed-websocket.md`, `docs/dhan-ref/08-annexure-enums.md`

## 2026-04-24 Updates (PR #337)

Applies to Phase 2 scheduler + main-feed reconnect path:

1. **Pre-open buffer window WIDENED to 09:00..=09:12 IST** (Fix #1). See
   `preopen_price_buffer.rs` — `PREOPEN_MINUTE_SLOTS = 13`,
   `PREOPEN_FIRST_MINUTE_SECS_IST = 9 * 3600`. Any 09:00..09:08 tick
   that would previously have been dropped now lands in the buffer.
   Ratchet: `test_preopen_buffer_window_is_0900_to_0912`.

2. **Backtrack walks 09:12 → 09:11 → … → 09:00** (Fix #2). First
   non-empty minute wins. Automatically follows from the widened
   buffer because `PreOpenCloses::backtrack_latest` iterates the full
   `closes` array. Ratchet:
   `test_compute_phase2_backtracking_uses_0900_when_later_minutes_missing`.

3. **REST /v2/marketfeed/ltp fallback** (Fix #5). At 09:12:55 IST, for
   any F&O stock still absent from the buffer, the scheduler calls the
   LTP endpoint and merges the result into the buffer's last slot.
   Module: `crates/core/src/instrument/preopen_rest_fallback.rs`. See
   `depth-subscription.md` 2026-04-24 Updates §2 for the policy
   (historical-close fallback if REST is also empty). Pure-logic
   primitives shipped in PR #337; scheduler integration is follow-up.

4. **Stock F&O expiry rollover** (Fix #6) — STRICT ≤ 1 trading day.
   `select_stock_expiry_with_rollover` (subscription_planner.rs) picks
   the NEXT expiry when today is T or T-1 relative to the nearest
   stock expiry. Indices unchanged — NIFTY / BANKNIFTY / FINNIFTY /
   MIDCPNIFTY keep the nearest expiry. See `depth-subscription.md`
   2026-04-24 Updates for the full rule + ratchets. Runbook:
   `docs/runbooks/expiry-day.md`.

5. **Reconnect subscription persistence** (Fix #3). `SubscribeRxGuard`
   in `crates/core/src/websocket/connection.rs` ensures the
   subscribe-command receiver is reinstalled on every read-loop exit,
   so post-reconnect subscribe commands reach the new socket. Prior
   behaviour left the channel `None` after first connect → cascading
   silent-socket → watchdog-trip → reconnect loop. Ratchets:
   `test_subscribe_rx_guard_reinstalls_on_drop`,
   `test_subscribe_rx_guard_survives_many_cycles`.

6. **Main-feed `tv_websocket_connections_active` counter wired** (Fix #7).
   `spawn_pool_watchdog_task` now writes
   `health.set_websocket_connections(active_count)` on every 5s tick.
   `/health` and the 09:15:30 heartbeat now report the live count,
   not `0/5`.

## 2026-04-22 Updates (this session, branch `claude/market-feed-depth-explanation-RynUx` / PR #324)

1. **Phase 2 trigger time = 09:13:00 IST** (was 09:12:00, commit 0340a7c). Reading the preopen buffer at 09:12:00 reads slot 4 (09:12:00–09:12:59) before the close has been written. 09:13:00 guarantees the 09:12 minute is fully closed.

2. **Phase 2 fires `Phase2Failed` with diagnostic when plan is empty** (commit 4aaa0fb). Includes `buffer_entries`, `skipped_no_price`, `skipped_no_expiry`, sample stocks. Silent "Added 0" is forbidden.

3. **`Phase2Complete` event includes depth counts** (commit 6fd9c2a): `depth_20_underlyings`, `depth_200_contracts`. Today these are 0 (depth dispatch from 09:12 close not yet wired); they'll populate once Items B+C are merged together.

4. **Off-hours WS disconnect spam fixed** (commit 996b0cc). Pre-market Dhan TCP-RSTs route to `WebSocketDisconnectedOffHours` (Severity::Low). In-market disconnects unchanged.

5. **Pre-open price buffer also captures NIFTY + BANKNIFTY IDX_I closes** (commit f641315). Use `build_preopen_combined_lookup` instead of the legacy stock-only lookup. `PREOPEN_INDEX_UNDERLYINGS` constant is the source of truth.

6. **Streaming-live heartbeat at 09:15:30 IST** (commit de1784a). Once-per-trading-day Telegram with feed counts. Operator's "am I connected" question now has a positive signal, not just disconnect EDGES.

7. **Depth anchor at 09:13:00 IST** (commit 427bf2d). Once-per-trading-day Telegram per index showing the 09:12 close + derived ATM strike. Audit trail for "what anchored today's depth".

8. **Items still pending** (do NOT pretend these are done): B (defer boot depth subscribe), real C (re-dispatch using InitialSubscribe variants), E (rebalancer 09:15 first-check gate), F (snapshot schema for depth), I (no-boot-depth-subscribe guard test), J (Phase2DispatchSource enum), K (edge-triggered Phase 2 alerts), L (audit log table), M (Grafana panels), N (tracing spans), Q (benchmark), R (runbooks).

## Architecture

### Connection Pool
- **5 connections** (max per Dhan account, independent from depth pools)
- **5,000 instruments per connection** (max per Dhan docs)
- **25,000 total capacity** (5 x 5,000)
- **Distribution:** Round-robin at boot (static, pre-allocated)
- **Endpoint:** `wss://api-feed.dhan.co?version=2&token=<TOKEN>&clientId=<CLIENT_ID>&authType=2`

### Subscription Strategy (Two-Phase)

**Phase 1 — Boot (before 9:00 AM):**
| Category | What's subscribed | Mode |
|----------|------------------|------|
| Major index values | NIFTY, BANKNIFTY, SENSEX, MIDCPNIFTY, FINNIFTY | Ticker (IDX_I forced) |
| Major index derivatives | ALL contracts (every expiry, every strike) | Quote/Full |
| Display indices | INDIA VIX, sector indices (~23) | Ticker |
| Stock equities | All NSE_EQ (~206 stocks) | Quote/Full |
| Stock derivatives | NOT subscribed yet — waiting for pre-market price | — |

**Phase 2 — 9:12 AM (pre-market finalized price available):**
| Category | What's subscribed | Mode |
|----------|------------------|------|
| Stock derivatives | Current expiry only, ATM ± 25 CE + PE + Future | Quote/Full |

ATM is calculated from the **9:08 AM finalized pre-market price** (or the latest
available LTP at 9:12 AM — whichever Dhan sent most recently). Binary search on
sorted option chain finds the nearest strike to the spot price. No median fallback.
If a stock has no pre-market price at 9:12 (didn't trade in pre-market), its F&O
is SKIPPED.

**Fixed for the entire day.** No dynamic resubscription for stocks — ATM ± 25
covers ± 18-62% of price depending on stock, which exceeds the NSE 20% circuit
breaker limit. Mathematically guaranteed sufficient.

**Stage 2 — Progressive Fill:**
- Remaining stock derivatives (further expiries) fill remaining capacity to 25K
- Sorted: nearest expiry first, then by symbol, then by security_id
- Skipped instruments logged with count

### Why NO Dynamic Rebalancing for Stocks (by design)

- ATM ± 25 covers ≥ 18% of stock price (even for highest-priced stocks)
- NSE circuit breaker = 20% — no stock can move beyond that intraday
- Therefore ATM ± 25 is **mathematically guaranteed** to cover any intraday move
- ATM is recalculated fresh every day at 9:12 AM from real pre-market price

**Contrast with depth feed:**
- Depth 20-level = 49 instruments (ATM ± 24, needs rebalancing — narrow band)
- Depth 200-level = 1 instrument (exact ATM, needs rebalancing)
- Main feed stocks = ATM ± 25 (wide band, no rebalancing needed)

### RequestCodes

| Code | Action | Status |
|------|--------|--------|
| 15 | Subscribe Ticker | Used at boot |
| 16 | Unsubscribe Ticker | Implemented, unused operationally |
| 17 | Subscribe Quote | Used at boot |
| 18 | Unsubscribe Quote | Implemented, unused operationally |
| 21 | Subscribe Full | Used at boot |
| 22 | Unsubscribe Full | Implemented, unused operationally |
| 12 | Disconnect | Used on graceful shutdown |

Unsubscribe codes 16/18/22 are built by `subscription_builder.rs` but only used
during graceful shutdown (A5). No live instrument swaps are performed.

### Post-Market Behavior

After 3 consecutive reconnection failures outside [09:00, 15:30) IST, the main
feed connections **stop reconnecting** and give up cleanly. This prevents Telegram
spam from the watchdog disconnect/reconnect cycle that occurs when Dhan sends no
data after market close. During market hours, infinite retries are maintained
(Dhan pings every 10s, so consecutive failures = real problem worth retrying).

### IDX_I Special Case

Indices (segment code 0) are forced to Ticker mode regardless of configured feed
mode. Dhan silently ignores Quote/Full subscriptions for IDX_I. The subscription
builder pre-sorts IDX_I instruments into separate Ticker-mode batches.

## Key Files

| File | Purpose |
|------|---------|
| `crates/core/src/websocket/connection_pool.rs` | Pool of 5 connections, round-robin distribution |
| `crates/core/src/websocket/connection.rs` | Individual WS connection, read loop, reconnect |
| `crates/core/src/instrument/subscription_planner.rs` | Two-stage instrument selection |
| `crates/core/src/websocket/subscription_builder.rs` | JSON message batching (max 100 per msg) |

## What This Prevents

- Subscribing to wrong instruments (ATM filtering ensures relevance)
- Exceeding Dhan limits (5 connections, 5000/conn, 100/msg enforced)
- Numeric SecurityId in JSON (must be string: `"1333"` not `1333`)
- Wrong feed mode for indices (IDX_I forced to Ticker)
- Confusion with depth protocol (main feed = 8-byte header, f32 prices)

## Trigger

This rule activates when editing files matching:
- `crates/core/src/websocket/connection_pool.rs`
- `crates/core/src/websocket/connection.rs`
- `crates/core/src/instrument/subscription_planner.rs`
- `crates/core/src/websocket/subscription_builder.rs`
- Any file containing `WebSocketConnectionPool`, `WebSocketConnection`, `build_subscription_messages`, `build_subscription_plan`, `SubscriptionPlan`, `FeedRequestCode`, `api-feed.dhan.co`
