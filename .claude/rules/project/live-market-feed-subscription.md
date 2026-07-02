# Live Market Feed WebSocket Subscription Rules

> **Authority:** CLAUDE.md > this file.
> **Scope:** Any file touching main WebSocket connection pool, subscription planning, or instrument distribution.
> **Ground truth:** `docs/dhan-ref/03-live-market-feed-websocket.md`, `docs/dhan-ref/08-annexure-enums.md`

## 2026-07-02 Update — DailyUniverse activity-watchdog floor 3s → 15s (audit GAP-1, PENDING OPERATOR APPROVAL)

> **Status: `<PENDING OPERATOR APPROVAL 2026-07-02>`** — prepared on branch
> `claude/watchdog-threshold-15s`; NOT pushed/merged until the operator
> approves (this changes a value operator-locked 2026-05-13).

**The gap (verified):** `crates/app/src/main.rs` clamped the main-feed
`activity_watchdog_threshold_secs` to `WATCHDOG_THRESHOLD_IDX_I_SECS = 3`
for the `DailyUniverse` scope (carried over from the Indices4Only
dense-feed assumption; the code comment itself deferred re-tuning).
Dhan's server pings only every 10s (`live-market-feed.md` rule 16) and
the read loop counts pings as activity — so during any ≥3s data lull
inside the market-hours gate (thin pre-open 09:00–09:15 especially) the
watchdog force-reconnected a HEALTHY, still-pinging socket. This
violated the watchdog module's own P2.1 intent (never fire on a socket
the server itself still treats as alive) and contradicted the 50s config
default (40s Dhan server timeout + 10s margin).

**The fix:** new constant
`WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS = 15`
(`crates/core/src/websocket/activity_watchdog.rs`) used by the
DailyUniverse clamp arm. Rationale: 15s = one full Dhan ping interval +
50% margin — a healthy socket ALWAYS shows a counted frame within 10s,
so 15s can never fire on a healthy socket, while still detecting a truly
silent socket 3.3x faster than the 50s default. The Indices4Only arm
keeps its historical 3s value untouched.

**Ratchet:** `activity_watchdog.rs::tests::daily_universe_threshold_is_15_above_dhan_ping_cadence`
pins 15, `> 10` (ping cadence), `< 50` (legacy), and ≥3 poll windows.

**On operator approval:** replace every
`<PENDING OPERATOR APPROVAL 2026-07-02>` marker (this file,
`activity_watchdog.rs`, `main.rs`, `connection.rs`) with the dated
operator quote, flip the plan
`.claude/plans/active-plan-watchdog-threshold.md` to APPROVED, then push.

## 2026-05-26 Update — pre-market buffer + Dhan historical REMOVED

Per operator directive 2026-05-26, the `preopen_price_buffer` module
(plus the `preopen_rest_fallback`, `build_preopen_combined_lookup`,
`PREOPEN_INDEX_UNDERLYINGS`, etc.) was DELETED alongside the entire
Dhan historical fetch chain. The 4 IDX_I SIDs (`LOCKED_UNIVERSE`)
subscribe in Ticker mode and `day_open` derives from the FIRST live
tick LTP after the IST midnight reset (`DayOhlcTracker::update_tick`
auto-arms on its first call per trading day). Phase 2 trigger,
pre-open snapshotter task, REST `/marketfeed/ltp` fallback,
`build_preopen_combined_lookup`, etc. are all deleted. The rest of
this file's "2026-04-22/04-24 Updates" and "Architecture (POST-AWS-LIFECYCLE)"
sections are kept as historical context only.

## 2026-05-19 Update — AWS-lifecycle PR #7 LOCKED (`indices_4_only`)

The default `[subscription] scope` in `config/base.toml` is
`indices_4_only`. This is the ONLY legal scope — `SubscriptionScope`
is a single-variant enum after PR #7b (#712). The 3 legacy scope
variants (`FullUniverse`, `IndicesOnlyAllExpiries`,
`IndicesUnderlyingsOnly`) and the 3 dead `subscribe_*` flags have
been retired. See
`.claude/rules/project/websocket-connection-scope-lock.md` +
operator-charter §I.

### LOCKED universe (4 SIDs)

| Slot | Subscription | Mode | Count |
|---|---|---|---|
| Index values IDX_I | NIFTY=13, BANKNIFTY=25, SENSEX=51 | Ticker | 3 |
| Display index IDX_I | INDIA VIX=21 (only allowed display SID) | Ticker | 1 |
| Stock F&O / Index F&O / Cash equities / Sectoral indices | **gated off** — code paths deleted | — | 0 |
| **TOTAL** | locked in `LOCKED_UNIVERSE` (`crates/common/src/locked_universe.rs`) | | **4** |

The 4 IDX_I SIDs fit on a single main-feed WebSocket connection
(Dhan cap = 5,000 SIDs/conn), so `effective_main_feed_pool_size`
returns the constant `PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1`.

### Why this narrowed

- Depth-20 + depth-200 modules deleted in PR #4 (#707).
- Phase 2 stock-F&O dispatcher deleted in PR #5 (#708).
- Bhavcopy + universe-builder + 6 helper modules deleted in PRs #6a/#6b (#709, #710).
- Greeks pipeline + movers pipeline deleted in PRs #2/#3 (#705, #706).
- The trading strategy is intraday option-buying on the 3 major indices.
  None of the dropped surface contributed to the live decision path.

### Mechanical guards (post-PR #7b)

- `SubscriptionScope` single-variant enum (compile-time prevention).
- `should_subscribe_stock_derivatives(_) → false`, `should_subscribe_index_derivatives(_) → false` (planner helpers).
- `is_display_index_allowed_under_scope(_, sid)` returns `true` ONLY for `INDIA_VIX_SECURITY_ID = 21`.
- Source-scan ratchet `crates/core/tests/indices4only_scope_lock_guard.rs` (3 tests) blocks reappearance of any retired identifier in `crates/`.

### Historical narrative (deleted code paths — for context only)

Prior to AWS-lifecycle (PRs #2-#7b), the universe expanded from 222
SIDs (4 IDX_I + 218 NSE_EQ underlyings, "Phase 0 LEAN MVP" 2026-05-13)
to ~11K SIDs ("Wave 5 indices-only" 2026-05-05) to ~24,324 SIDs
("FullUniverse" 2026-04-25 — 3 indices + 26 display + 216 cash +
2,037 index F&O + 22,042 stock F&O). The boot orchestration had a
4-mode bootstrap (PreMarket / MidPreMarket / MidMarket / PostMarket),
a live-tick ATM resolver, and a Phase 2 stock-F&O dispatcher chain.
All of that infrastructure is **deleted** as of PR #6b. The current
boot path subscribes the 4 SIDs in a single message and is done.

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

## Architecture (POST-AWS-LIFECYCLE, PR #7b 2026-05-19)

> **The sections below describe the legacy FullUniverse / Wave 5 / Phase 0 architecture as it existed before PRs #2-#7b. The live code path is now far simpler:** boot subscribes the 4 IDX_I SIDs from `LOCKED_UNIVERSE` in a single message on 1 main-feed connection. No Phase 2 dispatcher, no depth-20/200 pools, no live-tick ATM resolver, no two-phase boot. The historical text is retained for context (audit reconstruction, reading old commits) but operators should treat the "2026-05-19 Update" section above as the canonical live state.

### Connection Pool (post-PR #7b)
- **1 main-feed connection** (constant — `effective_main_feed_pool_size` returns 1).
- **1 order-update connection** (the only other allowed WS — see `websocket-connection-scope-lock.md`).
- **5,000 instruments per connection** (Dhan cap — we use 4 of those 5,000).
- **Endpoint:** `wss://api-feed.dhan.co?version=2&token=<TOKEN>&clientId=<CLIENT_ID>&authType=2`

### Legacy Connection Pool (pre-PR #7b, retained for context)
- **5 connections** (max per Dhan account, independent from depth pools)
- **5,000 instruments per connection** (max per Dhan docs)
- **25,000 total capacity** (5 x 5,000)
- **Distribution:** Round-robin at boot (static, pre-allocated)
- **Endpoint:** same as above

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

### IDX_I Special Case — Quote mode (operator directive 2026-05-20)

Indices (segment code 0) are subscribed in **Quote mode** (`FeedRequestCode`
17). The `WebSocketConnection` constructor pre-sorts IDX_I instruments into a
separate Quote-mode subscription batch (`connection.rs`, the
`idx_instruments` partition).

**Why Quote, not Ticker:** the Quote packet (response code 4, 50 bytes)
carries `day_open` / `day_high` / `day_low` / `day_close` at fixed offsets
(parsed by `parse_quote_packet`). That is the exchange-computed session OHLC,
delivered in every packet — so the index 09:15 open and running day high/low
come straight from Dhan, with no app-side tracking.

**History:** before 2026-05-20 IDX_I was force-subscribed in Ticker mode
(LTP-only, 16 bytes) on the *assumption* that "Dhan silently ignores
Quote/Full for index feeds". That assumption was never live-verified — our
own code never sent a Quote subscription for an index. The operator directed
switching to Quote mode to obtain day OHLC directly. Worst case, if Dhan does
ignore Quote for IDX_I, it still streams Ticker-grade packets, so LTP capture
is unaffected. Verify against the live `ticks` table after a session: if the
IDX_I `open`/`high`/`low` columns are non-zero, Dhan honors Quote mode for
indices and `DayOhlcTracker` is redundant.

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
- Wrong feed mode for indices (IDX_I subscribes in Quote mode for day OHLC)
- Confusion with depth protocol (main feed = 8-byte header, f32 prices)

## Trigger

This rule activates when editing files matching:
- `crates/core/src/websocket/connection_pool.rs`
- `crates/core/src/websocket/connection.rs`
- `crates/core/src/instrument/subscription_planner.rs`
- `crates/core/src/websocket/subscription_builder.rs`
- Any file containing `WebSocketConnectionPool`, `WebSocketConnection`, `build_subscription_messages`, `build_subscription_plan`, `SubscriptionPlan`, `FeedRequestCode`, `api-feed.dhan.co`
