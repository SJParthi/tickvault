# Depth Subscription Rules — ATM Selection & Rebalancing

> **Authority:** CLAUDE.md > this file.
> **Scope:** Any file touching depth WebSocket connections, strike selection, rebalancing, or depth persistence.
> **Ground truth:** `docs/dhan-ref/04-full-market-depth-websocket.md`, `docs/dhan-ref/08-annexure-enums.md`

## 2026-04-22 Updates (this session)

The following changes shipped on branch `claude/market-feed-depth-explanation-RynUx` (PR #324) and are mandatory for any new depth/Phase-2 work:

1. **`DepthCommand::InitialSubscribe20` / `InitialSubscribe200` variants exist** — added for the unified 09:13 dispatch flow. Boot-time depth subscribe is unchanged today (Item B not shipped yet); these variants will be used once Item B defers boot subscribe. Do not delete them.

2. **Depth rebalance Telegram MUST NOT use the words "aborting" or "spawning new"** — they describe a disconnect-and-respawn flow which is NOT what happens. The correct mechanism is `Swap20`/`Swap200` zero-disconnect. Use "zero-disconnect swap" wording in any new depth-rebalance message.

3. **Depth rebalance Telegram MUST be feed-aware** — only NIFTY and BANKNIFTY have 200-level. FINNIFTY/MIDCPNIFTY messages must NOT mention "200-level" — see the per-feed message branch in `main.rs` (Plan item C label fix, commit 6f6edc5).

4. **Phase 2 trigger time is 09:13:00 IST, not 09:12:00** (commit 0340a7c). Reading the buffer at 09:12:00 means slot 4 (09:12:00–09:12:59) is empty — backtrack walks down to 09:11/...09:08 which are typically also empty during pre-open. 09:13:00 guarantees the 09:12 close minute bucket is fully captured.

5. **Phase 2 empty plan MUST fire `Phase2Failed`, NOT `Phase2Complete { added_count: 0 }`** (commit 4aaa0fb). The diagnostic must include `buffer_entries`, `skipped_no_price`, `skipped_no_expiry`, and a sample of skipped stocks. Silent "Added 0" is banned.

6. **Off-hours WebSocket disconnects route to `WebSocketDisconnectedOffHours` (Severity::Low)**, not `WebSocketDisconnected` (Severity::High) (commit 996b0cc). In-market disconnects still use the High variant. Use `tickvault_common::market_hours::is_within_market_hours_ist()` to branch.

7. **Order-update WS activity watchdog is 14400s (4h), NOT 1800s** (commit 55452c2). Dhan's order-update server goes silent on idle accounts; 1800s caused every-30-min false reconnects. TCP-RST detection in the read loop's `Err` branch is the actual liveness backstop.

8. **`MarketOpenStreamingConfirmation` Telegram fires once per trading day at 09:15:30 IST** with active counts for main feed / depth-20 / depth-200 / order update (commit de1784a). Severity::Info. Operator's "am I streaming" question is answered by this single message.

9. **`MarketOpenDepthAnchor` Telegram fires once per index at 09:13:00 IST** showing the 09:12 close used + derived ATM strike (commit 427bf2d). Severity::Info. Audit trail for "what 09:12 close anchored today's depth".

10. **Pre-open price buffer captures NIFTY (id=13) + BANKNIFTY (id=25) on IDX_I segment** (commit f641315), in addition to F&O stocks. `PREOPEN_INDEX_UNDERLYINGS` constant is the source of truth for which indices feed depth ATM selection.

## Architecture

### Two Depth Types (independent WebSocket pools)

| Type | Endpoint | Instruments/conn | Connections | Total |
|------|----------|-----------------|-------------|-------|
| 20-level | `wss://depth-api-feed.dhan.co/twentydepth` | Up to 50 | 4 (NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY) | Up to 200 instruments |
| 200-level | `wss://full-depth-api.dhan.co/twohundreddepth` | Exactly 1 | 4 (NIFTY CE, NIFTY PE, BANKNIFTY CE, BANKNIFTY PE) | 4 instruments |

### ATM Selection Rules (MANDATORY)

1. **Always use REAL spot price from main WebSocket index LTP.** Never median, never hardcoded.
   - Spot source: `SharedSpotPrices` map (RwLock HashMap), updated by tick broadcast subscriber
   - Index security IDs: NIFTY=13, BANKNIFTY=25, FINNIFTY=27, MIDCPNIFTY=442
   - ATM = nearest strike to spot via binary search on sorted option chain

2. **Always use NEAREST expiry only.** Never far-month, never arbitrary.
   - `expiry_calendars.get(symbol).find(|e| e >= today)` — first expiry >= today
   - Enforced by `select_depth_instruments()` in `depth_strike_selector.rs`

3. **20-level: ATM ± 24 strikes = 49 instruments per underlying.**
   - 24 CE above ATM + ATM + 24 PE below ATM
   - Config: `twenty_depth_max_instruments = 49` (max 50 per Dhan, we use 49)
   - Constant: `DEPTH_ATM_STRIKES_EACH_SIDE = 24`

4. **200-level: ATM CE + ATM PE only (2 connections per underlying).**
   - Only NIFTY and BANKNIFTY get 200-level (4 connections total, within Dhan's 5 limit)
   - FINNIFTY and MIDCPNIFTY use 20-level only

### Boot Sequence

1. Main WebSocket connects → starts streaming index LTPs
2. Spot price updater captures LTP into `SharedSpotPrices` map
3. Depth setup waits up to 30s for NIFTY + BANKNIFTY LTP to arrive
4. `select_depth_instruments()` finds ATM for each underlying using real spot price
5. PROOF log emitted: underlying, spot price, ATM strike, expiry, instrument count
6. Depth connections spawned with correct instruments

### Rebalancing (Every 60 Seconds)

1. `run_depth_rebalancer()` reads latest spot prices from `SharedSpotPrices`
2. For each underlying: checks if spot has drifted ±3 strikes from previous ATM
3. If threshold exceeded → `RebalanceEvent` published via `watch::Sender`
4. Listener receives event → sends `DepthCommand::Swap200` via command channel
5. **ZERO DISCONNECT:** The existing WebSocket connection sends RequestCode 25 (unsubscribe old) then RequestCode 23 (subscribe new)
6. PROOF log + Telegram alert with old vs new contract labels

### Command Channel Pattern

Each depth connection has a `mpsc::Receiver<DepthCommand>` integrated into its `select!` loop:
- `DepthCommand::Swap20` — unsubscribe old instruments, subscribe new (20-level)
- `DepthCommand::Swap200` — unsubscribe old, subscribe new (200-level)

The rebalancer holds `mpsc::Sender<DepthCommand>` for each connection (stored in `depth_cmd_senders` map).

### Dhan Protocol Facts

- Subscribe: RequestCode 23 (both 20-level and 200-level)
- Unsubscribe: RequestCode **25** (NOT 24 — Dhan SDK has a bug here)
- 20-level JSON: `{ "RequestCode": 23, "InstrumentCount": N, "InstrumentList": [...] }`
- 200-level JSON: `{ "RequestCode": 23, "ExchangeSegment": "NSE_FNO", "SecurityId": "1333" }` (flat, no InstrumentList)
- Header: 12 bytes (NOT 8 like main feed), prices are f64 (NOT f32)
- Bid/Ask arrive as SEPARATE packets (code 41=Bid, 51=Ask)

## Key Files

| File | Purpose |
|------|---------|
| `crates/core/src/instrument/depth_strike_selector.rs` | ATM selection (binary search), `select_depth_instruments()` |
| `crates/core/src/instrument/depth_rebalancer.rs` | 60s spot drift check, `SharedSpotPrices`, `RebalanceEvent` |
| `crates/core/src/websocket/depth_connection.rs` | 20-level + 200-level WS, `DepthCommand` enum, command channel in select! loop |
| `crates/app/src/main.rs` | Boot wiring (Step 8c), spot updater, rebalance listener |

## What This Prevents

- Wrong ATM (median instead of real spot) → subscribing to useless far-OTM strikes
- Far expiry (Dec2027 instead of current month) → no data from Dhan
- Disconnect+reconnect on rebalance → tick gap during switch
- Infinite retry after market close → CPU/bandwidth waste
- Post-market stale ticks → polluting `ticks` table

## Trigger

This rule activates when editing files matching:
- `crates/core/src/instrument/depth_strike_selector.rs`
- `crates/core/src/instrument/depth_rebalancer.rs`
- `crates/core/src/websocket/depth_connection.rs`
- `crates/app/src/main.rs` (depth sections)
- Any file containing `DepthCommand`, `SharedSpotPrices`, `select_atm_strikes`, `select_depth_instruments`, `run_twenty_depth_connection`, `run_two_hundred_depth_connection`, `DEPTH_ATM_STRIKES_EACH_SIDE`, `depth_rebalancer`, `depth_cmd_senders`, `Swap200`, `Swap20`
