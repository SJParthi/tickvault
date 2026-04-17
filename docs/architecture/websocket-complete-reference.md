# WebSocket Complete Reference — tickvault

> **Authority:** This doc + `docs/dhan-ref/03-live-market-feed-websocket.md` + `docs/dhan-ref/04-full-market-depth-websocket.md` + `docs/dhan-ref/10-live-order-update-websocket.md`
>
> **When to read:** Before touching ANY file in `crates/core/src/websocket/`, `crates/core/src/parser/`, `crates/app/src/main.rs` (WS spawn sections), or `crates/storage/src/deep_depth_persistence.rs`.
>
> **Last updated:** 2026-04-16

---

## 1. Architecture Overview

The system uses 4 independent WebSocket types, each with its own connection pool:

| Type | Endpoint | Protocol | Connections | Instruments | Pool |
|------|----------|----------|-------------|-------------|------|
| Live Market Feed | `wss://api-feed.dhan.co` | Binary (8-byte header) | 5 | 25,000 | Independent |
| 20-Level Depth | `wss://depth-api-feed.dhan.co/twentydepth` | Binary (12-byte header) | 4 | 196 (49 each, ATM±24) | Independent |
| 200-Level Depth | `wss://full-depth-api.dhan.co/twohundreddepth` | Binary (12-byte header) | 4 | 4 (1 each, ATM CE/PE) | Independent |
| Order Update | `wss://api-order-update.dhan.co` | JSON | 1 | N/A (all orders) | Independent |
| **Total** | | | **14** | | |

**Connection pools are independent** (confirmed by Dhan team 2026-04-06). Using 5 main feed connections does NOT reduce depth connection slots.

---

## 2. Live Market Feed WebSocket

### 2.1 Connection

```
wss://api-feed.dhan.co?version=2&token=<ACCESS_TOKEN>&clientId=<CLIENT_ID>&authType=2
```

All 4 query params required. Token goes in URL (WebSocket exception to the "never in URL" rule).

### 2.2 Limits

- Max 5 connections per user
- Max 5,000 instruments per connection
- Max 100 instruments per single JSON subscribe message
- Send multiple messages to reach 5,000

### 2.3 Binary Header (8 bytes, every packet)

| Byte (0-based) | Type | Field |
|----------------|------|-------|
| 0 | u8 | Response Code |
| 1-2 | u16 LE | Message Length |
| 3 | u8 | Exchange Segment (numeric) |
| 4-7 | u32 LE | Security ID |

**Byte order:** Response code FIRST, then message length.

### 2.4 Response Codes

| Code | Name | Packet Size | Has Timestamp | Notes |
|------|------|-------------|---------------|-------|
| 2 | Ticker | 16 bytes | Yes (LTT) | LTP + LTT only |
| 4 | Quote | 50 bytes | Yes (LTT) | LTP, LTQ, LTT, ATP, Volume, OHLC |
| 5 | OI | 12 bytes | No | Open Interest only |
| 6 | PrevClose | 16 bytes | No | Arrives on EVERY subscription, any mode |
| 7 | MarketStatus | 8 bytes | No | Header only, no payload |
| 8 | Full | 162 bytes | Yes (LTT) | Everything + 5-level depth |
| 50 | Disconnect | 10 bytes | No | Header + 2-byte reason code |

### 2.5 Field Types (EXACT)

- **LTP, ATP, prices:** f32 (4 bytes, single precision)
- **Volume, OI, quantities:** u32
- **LTQ:** u16
- **Orders count:** u16
- **LTT (timestamps):** u32 — IST epoch seconds (NOT UTC; subtract 19800 for UTC)
- **Message length:** u16
- **Security ID:** u32

### 2.6 Full Packet (162 bytes) — Field Map

| Byte | Type | Field |
|------|------|-------|
| 0-7 | header | 8 bytes |
| 8-11 | f32 LE | LTP |
| 12-13 | u16 LE | LTQ |
| 14-17 | u32 LE | LTT (IST epoch seconds) |
| 18-21 | f32 LE | ATP |
| 22-25 | u32 LE | Volume |
| 26-29 | u32 LE | Total Sell Qty |
| 30-33 | u32 LE | Total Buy Qty |
| 34-37 | u32 LE | OI |
| 38-41 | u32 LE | Highest OI |
| 42-45 | u32 LE | Lowest OI |
| 46-49 | f32 LE | Day Open |
| 50-53 | f32 LE | Day Close |
| 54-57 | f32 LE | Day High |
| 58-61 | f32 LE | Day Low |
| 62-161 | depth | 5 levels x 20 bytes each |

### 2.7 Depth in Full Packet (5 levels, 20 bytes each)

Each level at `62 + (level * 20)`:

| Offset | Type | Field |
|--------|------|-------|
| 0-3 | u32 LE | Bid Quantity |
| 4-7 | u32 LE | Ask Quantity |
| 8-9 | u16 LE | Bid Orders |
| 10-11 | u16 LE | Ask Orders |
| 12-15 | f32 LE | Bid Price |
| 16-19 | f32 LE | Ask Price |

### 2.8 Subscription JSON

```json
{
  "RequestCode": 21,
  "InstrumentCount": 2,
  "InstrumentList": [
    { "ExchangeSegment": "NSE_EQ", "SecurityId": "1333" },
    { "ExchangeSegment": "NSE_FNO", "SecurityId": "52432" }
  ]
}
```

**SecurityId must be STRING** (`"1333"` not `1333`).

### 2.9 Ping/Pong

Server pings every 10s. Client must respond within 40s. Handled by WebSocket library — do NOT implement manual ping frames.

---

## 3. Full Market Depth 20-Level WebSocket

### 3.1 Connection

```
wss://depth-api-feed.dhan.co/twentydepth?token=<TOKEN>&clientId=<CLIENT_ID>&authType=2
```

### 3.2 Limits

- Max 5 connections (independent pool)
- Max 50 instruments per connection
- NSE only (NSE_EQ, NSE_FNO). No BSE, MCX, Currency.

### 3.3 Binary Header (12 bytes — NOT 8!)

| Byte (0-based) | Type | Field |
|----------------|------|-------|
| 0-1 | u16 LE | Message Length |
| 2 | u8 | Feed Code: 41=Bid (Buy), 51=Ask (Sell) |
| 3 | u8 | Exchange Segment |
| 4-7 | u32 LE | Security ID |
| 8-11 | u32 LE | Sequence Number (**stored in QuestDB as `exchange_sequence` for ordering + gap detection**) |

**CRITICAL: Byte order DIFFERS from main feed:**
- Main feed: byte 0 = response code
- Depth feed: bytes 0-1 = message length, byte 2 = response code

### 3.4 Per Level (16 bytes)

| Offset | Type | Field |
|--------|------|-------|
| 0-7 | f64 LE | Price (DOUBLE precision, not f32!) |
| 8-11 | u32 LE | Quantity |
| 12-15 | u32 LE | Number of Orders |

### 3.5 Packet Size

332 bytes = 12 header + 20 levels x 16 bytes

### 3.6 Bid/Ask Separate

Code 41 = Bid side, code 51 = Ask side. Two separate packets. You need BOTH to build the full order book.

### 3.7 Packet Stacking

Multiple instruments: `[Inst1 Bid][Inst1 Ask][Inst2 Bid][Inst2 Ask]...`. Split by message length from header.

### 3.8 NO TIMESTAMP — But Sequence Number for Ordering

**Dhan does NOT include any timestamp (LTT) in depth packets.** However:
- **20-level header bytes 8-11 = sequence number** from the exchange. This is the ONLY way to determine precise ordering of depth snapshots and detect gaps (missed packets).
- We store this as `exchange_sequence` (LONG) in the `deep_market_depth` QuestDB table.
- Gap detection: if `current_seq != last_seq + 1`, a depth snapshot was missed.
- We also use `received_at_nanos` (local system clock) for QuestDB timestamp partitioning.

### 3.9 Disconnect

14 bytes = 12-byte header (code 50) + 2-byte reason at bytes 12-13.

---

## 4. Full Market Depth 200-Level WebSocket

### 4.1 Connection

```
wss://full-depth-api.dhan.co/?token=<TOKEN>&clientId=<CLIENT_ID>&authType=2
```

### 4.2 Limits

- Max 5 connections (independent pool)
- **1 instrument per connection** (not 50!)
- We use 4: NIFTY ATM CE, NIFTY ATM PE, BANKNIFTY ATM CE, BANKNIFTY ATM PE
- FINNIFTY and MIDCPNIFTY do NOT get 200-level (only 20-level depth)

### 4.3 Subscribe JSON (DIFFERENT structure from 20-level)

```json
{ "RequestCode": 23, "ExchangeSegment": "NSE_FNO", "SecurityId": "80276" }
```

No `InstrumentList` array, no `InstrumentCount`. Flat fields.

### 4.4 Binary Header (12 bytes)

Same structure as 20-level EXCEPT bytes 8-11:

| Byte (0-based) | Type | Field |
|----------------|------|-------|
| 0-1 | u16 LE | Message Length |
| 2 | u8 | Feed Code: 41=Bid, 51=Ask |
| 3 | u8 | Exchange Segment |
| 4-7 | u32 LE | Security ID |
| 8-11 | u32 LE | **Row Count** (NOT sequence number!) |

### 4.5 Variable Packet Size

`12 + (row_count * 16)` bytes. Max 3212 bytes (200 levels). Parse ONLY `row_count` levels. Validate `row_count <= 200` before parsing.

### 4.6 NO TIMESTAMP

Same as 20-level — no exchange timestamp. We use `received_at_nanos`.

### 4.7 Depth Rebalancer (Zero-Disconnect ATM Swap)

Checks every 60s if spot price has drifted ≥3 strikes from current ATM.
If so, sends `DepthCommand::Swap200` through the command channel:
1. Unsubscribe old instrument (RequestCode 25)
2. Subscribe new ATM instrument (RequestCode 23)
3. **ZERO disconnect. ZERO reconnect.** Same WebSocket stays alive.

Command channel: `tokio::sync::mpsc::Sender<DepthCommand>` stored in
`depth_cmd_senders` map at boot. Each depth connection's `select!` loop
listens for commands alongside WebSocket frames.

PROOF: Every rebalance logs the old and new ATM contract labels + sends
Telegram alert.

### 4.8 Activity Watchdog

50-second threshold (40s Dhan server ping timeout + 10s margin). Per-connection
`ActivityWatchdog` detects zombie sockets via atomic counter. If no frames or
pings for 50s, watchdog fires and triggers reconnect.

**Note:** Raw read timeout was removed in STAGE-B — watchdog is the only
liveness check (along with Dhan's server-side 40s pong timeout).

### 4.9 Off-Hours / Retry Cap

- Depth connections cap at **20 consecutive reconnection attempts** (~10 min at 30s backoff)
- After 20 failures, connection gives up cleanly (no infinite loop)
- No multi-hour sleep — connections simply stop after retry cap
- Market-hours sleep was removed (2026-04-09); retry cap added (2026-04-16)

### 4.10 Boot-Time ATM Selection

At boot, depth connections WAIT up to 30s for the first index LTP from the
main Live Market Feed WebSocket. The spot price is captured in `SharedSpotPrices`
(RwLock HashMap) by a tick broadcast subscriber. Once available:
1. `select_depth_instruments()` finds nearest expiry ≥ today
2. Binary search on sorted option chain finds ATM strike closest to spot
3. 20-level: ATM ± 24 strikes = 49 instruments per underlying
4. 200-level: ATM CE + ATM PE (1 instrument per connection)

See `.claude/rules/project/depth-subscription.md` for full ATM selection rules.

---

## 5. Order Update WebSocket

### 5.1 Connection

```
wss://api-order-update.dhan.co
```

No query params. Auth via JSON message after connect.

### 5.2 Auth Message

```json
{
  "LoginReq": { "MsgCode": 42, "ClientId": "1000000123", "Token": "eyJ..." },
  "UserType": "SELF"
}
```

PascalCase field names. `MsgCode` is always 42.

### 5.3 Protocol

**JSON messages, NOT binary.** Parse with `serde_json`.

### 5.4 Message Format

```json
{ "Data": { ... }, "Type": "order_alert" }
```

PascalCase top-level keys.

### 5.5 Data Field Abbreviations (DIFFERENT from REST API)

| Field | Values | REST Equivalent |
|-------|--------|-----------------|
| `Product` | `C`=CNC, `I`=INTRADAY, `M`=MARGIN, `F`=MTF, `V`=CO, `B`=BO | `productType` (full word) |
| `TxnType` | `B`=Buy, `S`=Sell | `transactionType` |
| `OrderType` | `LMT`, `MKT`, `SL`, `SLM` | Same |
| `Segment` | `E`=Equity, `D`=Derivatives, `C`=Currency, `M`=Commodity | `exchangeSegment` |
| `Source` | `P`=API, `N`=Normal (Dhan web/app) | Not available in REST |

### 5.6 Critical Notes

- Receives ALL account orders — filter by `Source: "P"` for API orders
- Timestamps are IST strings (`YYYY-MM-DD HH:MM:SS`), NOT epoch
- `CorrelationId` matches what you set when placing orders
- `OffMktFlag: "1"` = AMO, `"0"` = normal
- `OptType: "XX"` = non-option. `"CE"` or `"PE"` for options
- `RefLtp` and `TickSize` use camelCase (NOT PascalCase like other fields)

### 5.7 Status Values

`TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `TRADED`, `EXPIRED`

---

## 6. Request Codes (All Types)

| Code | Action | Used By |
|------|--------|---------|
| 11 | Connect | Main feed |
| 12 | Disconnect | Main feed, Depth |
| 15 | Subscribe Ticker | Main feed |
| 16 | Unsubscribe Ticker | Main feed |
| 17 | Subscribe Quote | Main feed |
| 18 | Unsubscribe Quote | Main feed |
| 21 | Subscribe Full | Main feed |
| 22 | Unsubscribe Full | Main feed |
| 23 | Subscribe Depth | 20-level + 200-level |
| 25 | Unsubscribe Depth | 20-level + 200-level (**NOT 24!**) |

---

## 7. Key Differences Cheat Sheet

| Aspect | Main Feed | Depth 20 | Depth 200 | Order Update |
|--------|-----------|----------|-----------|--------------|
| Protocol | Binary | Binary | Binary | JSON |
| Header size | 8 bytes | 12 bytes | 12 bytes | N/A |
| Byte 0 | Response code | Msg length (low) | Msg length (low) | N/A |
| Price type | f32 | **f64** | **f64** | N/A (string) |
| Has timestamp | Yes (LTT) | **NO** | **NO** | Yes (IST string) |
| Bid+Ask combined | Yes (Full pkt) | No (separate) | No (separate) | N/A |
| Max instruments/conn | 5,000 | 50 | **1** | All orders |
| Bytes 8-11 | Payload data | Sequence (ignore) | **Row count** | N/A |
| All reads | Little Endian | Little Endian | Little Endian | UTF-8 JSON |
| Segments | All | NSE only | NSE only | All |

---

## 8. QuestDB Persistence

### 8.1 Tick Persistence (Main Feed)

- **Table:** `ticks` — TIMESTAMP(ts) PARTITION BY DAY WAL
- **DEDUP:** `(ts, security_id, segment)` — includes segment to prevent cross-segment collision
- **Timestamp:** `exchange_timestamp * 1_000_000_000` (IST epoch nanos, NO +5:30 offset)
- **Price:** f32 → f64 via `f32_to_f64_clean()` to prevent IEEE 754 widening artifacts
- **Persist guards (both must pass):**
  1. `is_within_persist_window(exchange_timestamp)` — LTT must be in [09:00, 15:30) IST
  2. `is_wall_clock_within_persist_window(received_at_nanos)` — current time must also be in [09:00, 15:30) IST
  3. `is_today_ist(exchange_timestamp)` — rejects stale ticks from previous days
- **Backfill worker:** DISABLED — historical candle data must NOT be injected into the `ticks` table. Historical candles go to `historical_candles` only.

### 8.2 Depth Persistence (20-level + 200-level)

- **Table:** `deep_market_depth` — TIMESTAMP(ts) PARTITION BY HOUR WAL
- **Columns:** security_id, segment, side (BID/ASK), level, price (f64), quantity, orders, depth_type (20/200), received_at, ts
- **Timestamp:** `received_at_nanos` (local system clock, NOT exchange time — depth has no timestamp)
- **IST offset:** Added in `write_record_to_ilp()` via `+ IST_UTC_OFFSET_NANOS`

---

## 9. Telegram Notification Events

| Event | When Fired | Severity |
|-------|------------|----------|
| `WebSocketConnected { connection_index }` | After all 5 main feed connections spawned | Low |
| `WebSocketDisconnected { connection_index, reason }` | Wired in all 3 disconnect paths (`connection.rs:341-392`) | High |
| `DepthTwentyConnected { underlying }` | After 20-level WS connected + subscribed (via oneshot signal) | Low |
| `DepthTwentyDisconnected { underlying, reason }` | On connection error/drop | High |
| `DepthTwoHundredConnected { underlying }` | After 200-level WS connected + subscribed (via oneshot signal) | Low |
| `DepthTwoHundredDisconnected { underlying, reason }` | On connection error/drop | High |
| `OrderUpdateConnected` | When order update WS task starts | Low |
| `OrderUpdateDisconnected { reason }` | When connection task exits | High |

---

## 10. Known Gaps and Audit Findings

### 10.1 Resolved (Fixed)

| # | Issue | Fix | Date |
|---|-------|-----|------|
| 1 | 200-level depth `.min(secs_until)` no-op in sleep calculation | Removed useless self-comparison | 2026-04-09 |
| 2 | No Telegram for Live Market Feed WS connections | Added `WebSocketConnected` for all 5 | 2026-04-09 |
| 3 | No Telegram for Order Update WS connection | Added `OrderUpdateConnected` | 2026-04-09 |
| 4 | Order update token not zeroized (security) | Wrapped in `zeroize::Zeroizing<String>` | 2026-04-09 |
| 5 | Non-order JSON messages silently dropped (no metric) | Added `tv_order_update_non_order_messages_total` counter | 2026-04-09 |
| 6 | 20-level depth sequence number DISCARDED (bytes 8-11 ignored) | Now persisted as `exchange_sequence` LONG in QuestDB | 2026-04-09 |
| 7 | No Telegram for depth rebalancer ATM strike change | Telegram fires on every rebalance event with old vs new contract labels | 2026-04-16 |
| 8 | 20-level depth connections stay connected off-hours (infinite retry) | Both 20-level and 200-level cap at 20 retries, then give up | 2026-04-16 |
| 9 | Depth rebalancer disconnect+reconnect for ATM swap | Zero-disconnect via `DepthCommand` command channel (unsub+resub on same WS) | 2026-04-16 |
| 10 | Depth strike selector picked arbitrary strikes (.first() on HashMap) | Real ATM from index LTP, nearest expiry, binary search, ATM ± 24 | 2026-04-16 |
| 11 | Backfill worker injecting synthetic ticks into `ticks` table | BackfillWorker disabled — historical data stays in `historical_candles` only | 2026-04-16 |
| 12 | Post-market stale ticks written to QuestDB | Wall-clock guard `is_wall_clock_within_persist_window()` added to tick processor | 2026-04-16 |
| 13 | (C1) No graceful unsubscribe on shutdown | Already sends RequestCode 12 (Disconnect); explicit unsub of 25K instruments would delay shutdown for no benefit — Dhan cleans up on disconnect | 2026-04-16 |
| 14 | (H1) WebSocketDisconnected not wired for main feed | Already wired in `connection.rs:341-392` — all 3 disconnect paths fire notification | 2026-04-16 |
| 15 | (C3) Order update token not wrapped in Zeroizing | Already wrapped in `zeroize::Zeroizing` at `order_update_connection.rs:168` | 2026-04-16 |
| 16 | (C2) Order update auth not validated after login | Auth IS validated in read loop via `classify_auth_response()` at line 309; Close frame detected immediately; 660s watchdog backstop | 2026-04-16 |
| 17 | (H2) No pool-level circuit breaker | Already implemented: `poll_watchdog()` fires ERROR after 60s all-down, HALT after 300s. Spawned in `main.rs` every 5s | 2026-04-16 |
| 18 | (H5) No Telegram on depth parse failures | Added consecutive error counter — escalates to ERROR (triggers Telegram) after 5 consecutive failures | 2026-04-16 |
| 19 | (H3) No per-security stale tick detection | Already implemented: `TickGapTracker` tracks per-security_id via `HashMap<u32, SecurityFeedState>`, ERROR alerts per security | 2026-04-16 |
| 20 | (M1) Telegram fires on spawn not first frame | Fires `WebSocketConnected` after `spawn_all()` — correct behavior (signals operational state) | 2026-04-16 |
| 21 | (M2) WebSocketReconnected never sent | Already wired: fires when `reconnection_count > 0` at `connection.rs:273-284` | 2026-04-16 |
| 22 | (M3) No pool-level heartbeat | Already implemented: `tv_ws_last_frame_epoch_secs` gauge per-connection via `build_heartbeat_gauge()` | 2026-04-16 |
| 23 | (M4) No token refresh ack log | Already logs `"token renewed successfully"` at INFO after renewal at `token_manager.rs:775-779` | 2026-04-16 |
| 24 | (M5) Token leak in WebSocket URL errors | Already safe: only base URL used in error messages, authenticated URL never exposed | 2026-04-16 |
| 25 | (M6) No SPSC backpressure alert | Already implemented: channel occupancy sampled every flush cycle via gauge metric | 2026-04-16 |
| 26 | (M7) Order update non-order messages dropped | Already tracked: `tv_order_update_non_order_messages_total` counter exists | 2026-04-16 |
| 27 | (L3) 200-level 40s timeout too aggressive | Watchdog threshold is 50s (40s Dhan + 10s margin) — correct | 2026-04-16 |
| 28 | (L4-L7) Config/doc-only items | All handled via config or existing mechanisms — no code changes needed | 2026-04-16 |
| 29 | (H4) No hex dump for unparseable packets | Added hex dump of first 64 bytes at ERROR level every 10th error + WARN with hex for others | 2026-04-16 |
| 30 | FAST BOOT path emitted zero `WebSocketConnected` Telegram alerts | Extracted `emit_websocket_connected_alerts(notifier, count)` helper, called from BOTH boot paths; added `WebSocketPoolOnline { connected, total }` summary event to survive Telegram rate-limit drops; mechanical guard test `crates/app/tests/boot_path_notify_parity.rs` | 2026-04-17 |
| 31 | Pool watchdog `Degraded` + `Recovered` verdicts silently discarded (only `Halt` was handled) | `spawn_pool_watchdog_task` now takes a notifier and fires `WebSocketPoolDegraded`, `WebSocketPoolRecovered`, `WebSocketPoolHalt` events with matching counters | 2026-04-17 |
| 32 | Depth index-LTP 30s timeout warned only via `warn!` — no Telegram, no ERROR log | Promoted to `error!` (auto-telegrams via Loki pipeline) and fires typed `NotificationEvent::DepthIndexLtpTimeout { waited_secs }` event | 2026-04-17 |
| 33 | (O3) Depth rebalancer could swap to wrong ATM if index LTP feed stalled — no freshness check on `SharedSpotPrices` | `SharedSpotPrices` now stores `SpotPriceEntry { price, updated_at }`. Rebalancer reads via `get_spot_price_entry`; if age ≥ 180s, skips + fires `DepthSpotPriceStale { underlying, age_secs }`. Counter `tv_depth_rebalancer_stale_spot_skips_total`. 6 new tests. | 2026-04-17 |
| 34 | (O2) `OrderUpdateConnected` fired on task spawn, not after Dhan auth ACK — operator saw "connected" seconds before orders were being processed | New `OrderUpdateAuthenticated { }` event fires exactly once per process lifetime via a `Notify` + `AtomicBool` CAS latch on first successful `parse_order_update` OR first `AuthResponseKind::Success`. 4 new tests. | 2026-04-17 |
| 35 | (O1-A) No Phase 2 scheduler — 09:12 IST spec was documented but zero code ran at that time | New `phase2_scheduler.rs` with pure `next_phase2_trigger` decision fn covering weekday / weekend / holiday / before-912 / at-912 / crash-recovery / late-start / post-market cases. `run_phase2_scheduler` async driver waits for LTPs (3 retries × 30s), emits `Phase2Started / Phase2RunImmediate / Phase2Complete / Phase2Failed / Phase2Skipped` events + `tv_phase2_runs_total{outcome}` + `tv_phase2_run_ms` histogram. Wired into the depth-rebalancer spawn block (runs on both boot paths). 15 new tests. Actual subscribe dispatch is the still-open O1-B. | 2026-04-17 |

### 10.2 Open Gaps

#### HIGH — Not Yet Implemented

| # | Gap | Design Sketch | Est. Effort |
|---|-----|---------------|-------------|
| O1-B | **9:12 AM Phase 2 stock F&O subscription plumbing** — O1-A (scheduler + events) landed in commit TBD. The scheduler correctly wakes at 09:12 IST (or runs immediately on late-start / crash-recovery), waits for LTPs, retries up to 3×, and emits `Phase2Complete { added_count }` / `Phase2Failed`. The remaining work is the **actual `SubscribeCommand` channel** on each main-feed `WebSocketConnection` (mirrors depth's `DepthCommand`): new `mpsc::Receiver<SubscribeCommand>` integrated into the `run()` `select!` loop, pool-level fan-out to the connection with spare capacity, delta computation from boot plan to new plan. | 1 day |
| ~~O2~~ | ~~OrderUpdateConnected fires on task spawn, not after successful auth ACK~~ | **✅ CLOSED 2026-04-17 (O2)** — new `OrderUpdateAuthenticated` event fires once per process lifetime after first successful parse or Dhan ACK. | — |
| ~~O3~~ | ~~Stale spot-price detection on depth rebalancer~~ | **✅ CLOSED 2026-04-17 (O3)** — `SharedSpotPrices` now stores `(price, updated_at: Instant)`. Rebalancer skips + emits `DepthSpotPriceStale` if age > 180s. | — |

#### MEDIUM — Observability

| # | Gap | Fix Shape |
|---|-----|-----------|
| M1 | Missing metrics per §10.3 (5 metrics) | Add counters/gauges, increment at relevant sites |
| M2 | Missing tests per §10.4 (6 tests) | Add integration tests in `crates/core/tests/` |

#### ACCEPTED (No Open Critical Gaps)

*No open critical gaps as of 2026-04-17.*

#### ACCEPTED (Dhan Limitation / Design Decision)

| # | Decision | Rationale |
|---|----------|-----------|
| A1 | No exchange timestamp in depth packets | Dhan protocol has no timestamp — all SDKs use local clock |
| A2 | 200-level uses 1 connection per instrument | Dhan API limitation, not our choice |
| A3 | Depth data only flows during 09:15-15:30 IST | Dhan sends nothing outside market hours |
| A4 | PrevClose packets arrive on any subscription mode | Expected — used for day change % calculations |
| A5 | Unsubscribe depth = 25 (not SDK's 24) | SDK bug; we follow Dhan docs |
| A6 | Main feed does NOT rebalance dynamically | 25,000 capacity covers all index derivatives + ATM±10 stocks; no gap from ATM drift |
| A7 | Depth rebalance uses command channel (no disconnect) | RequestCode 25 (unsub) + 23 (sub) on same WS; zero tick loss during ATM swap |

### 10.3 Missing Metrics

| Metric | Purpose | Status |
|--------|---------|--------|
| `tv_websocket_pool_all_dead` | Bool gauge: true if all 5 main feed connections are down | ✅ **landed 2026-04-17** — set in `pool_watchdog::PoolWatchdog::tick` every 5s |
| `tv_websocket_failed_connections_count` | Gauge: number of connections in Reconnecting state | ✅ **landed 2026-04-17** — set in `pool_watchdog::PoolWatchdog::tick` |
| `tv_depth_rebalancer_stale_spot_skips_total` | Counter: rebalance cycles skipped due to stale spot (O3) | ✅ **landed 2026-04-17** |
| `tv_phase2_runs_total{outcome}` | Counter: Phase 2 scheduler outcomes (O1-A) | ✅ **landed 2026-04-17** |
| `tv_phase2_run_ms` | Histogram: Phase 2 scheduler run duration | ✅ **landed 2026-04-17** |
| `tv_pool_degraded_alerts_total` | Counter: 60s-all-down Telegrams fired | ✅ **landed 2026-04-17** |
| `tv_pool_recoveries_total` | Counter: pool-recovered transitions | ✅ **landed 2026-04-17** |
| `tv_depth_20lvl_sequence_gaps_total` | Counter: detected sequence gaps in 20-level packets | ✅ **landed 2026-04-17** — `tick_processor.rs` per-`(security_id, side)` tracker, increments when `message_sequence != prev + 1` |
| `tv_order_update_non_order_messages_dropped_total` | Counter: JSON messages that weren't order updates | ✅ tracked via existing `tv_order_update_non_order_messages_total` |
| `tv_unknown_response_codes_total` | Counter: Dhan sent a response code we don't recognize | ✅ **landed 2026-04-17** — dispatcher `_` arm increments before returning `ParseError::UnknownResponseCode` |
| `tv_packets_by_response_code` | Counter (labelled): breakdown of all received packet types | ✅ **landed 2026-04-17** — labelled counter, every dispatch increments the matching label (`ticker`, `quote`, `full`, `oi`, `prev_close`, `market_status`, `disconnect`, `market_depth_v1`, `index_ticker`, or `unknown`) |

### 10.4 Missing Tests

| Test | Purpose |
|------|---------|
| All 5 connections fail simultaneously | Verify pool-level alert fires |
| Token refresh during active streaming | Verify no frame loss during 807 → refresh → reconnect |
| 200-level row_count = 500 (malformed) | Verify parser rejects with error (not buffer overflow) |
| Order update with wrong MsgCode | Verify auth error detection path |
| Graceful unsubscribe on shutdown | Verify unsubscribe messages sent before close |
| Per-security stale tick detection | Verify alert after N seconds of no data for one security |

---

## 11. Dhan Document → Code Verification Matrix

> **Purpose:** Every requirement from Dhan's Full Market Depth, Live Market Feed, and Order Update
> docs is mapped to the exact file:line in our code. If Dhan changes their protocol, update this
> matrix and the corresponding code. This section must be reviewed on every Dhan release.

### 11.1 Full Market Depth — Line-by-Line Proof

| # | Dhan Requirement | Our Code | File:Line | Tests |
|---|-----------------|----------|-----------|-------|
| 1 | NSE Equity and Derivatives only | Subscription filters to `NseFno` | `main.rs:1266` | `subscription_builder::tests` |
| 2 | 20-level URL: `wss://depth-api-feed.dhan.co/twentydepth` | `DHAN_TWENTY_DEPTH_WS_BASE_URL` | `constants.rs:972` | compile-time |
| 3 | 200-level URL: `wss://full-depth-api.dhan.co/twohundreddepth` | `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL` | `constants.rs:981` | compile-time |
| 4 | Query params: token, clientId, authType=2 | URL built with all 3 | `depth_connection.rs:243-254` | connection tests |
| 5 | 20-level: max 50 instruments/connection | `DEPTH_SUBSCRIPTION_BATCH_SIZE = 50` | `depth_connection.rs:53` | compile assert |
| 6 | 200-level: 1 instrument/connection | Flat JSON (no InstrumentList) | `subscription_builder.rs` | unit tests |
| 7 | 20-level subscribe JSON: `{RequestCode:23, InstrumentCount, InstrumentList}` | `build_twenty_depth_subscription_messages()` | `subscription_builder.rs` | `test_twenty_depth_*` |
| 8 | 200-level subscribe JSON: `{RequestCode:23, ExchangeSegment, SecurityId}` | `build_two_hundred_depth_subscription_message()` | `subscription_builder.rs` | `test_two_hundred_*` |
| 9 | SecurityId is STRING in JSON | `SecurityId: security_id.to_string()` | `subscription_builder.rs` | assertion tests |
| 10 | Ping every 10s, pong within 40s | `DEPTH_READ_TIMEOUT_SECS = 40`, explicit pong handler | `depth_connection.rs:40,338-345` | timeout tests |
| 11 | 12-byte header (NOT 8) | `DEEP_DEPTH_HEADER_SIZE = 12` | `constants.rs:346` | compile assert `constants.rs:1703` |
| 12 | Bytes 1-2: int16 message length | `read_u16_le(raw, 0)` | `deep_depth.rs:78` | `test_parse_deep_depth_header_valid` |
| 13 | Byte 3: feed code (41=Bid, 51=Ask) | `raw[2]` → `feed_code_to_side()` | `deep_depth.rs:79,90-96` | `test_feed_code_to_side_*` |
| 14 | Byte 4: exchange segment | `raw[3]` | `deep_depth.rs:80` | header tests |
| 15 | Bytes 5-8: int32 security ID | `read_u32_le(raw, 4)` | `deep_depth.rs:81` | `test_parse_deep_depth_header_valid` |
| 16 | 20-lvl bytes 9-12: sequence (persisted for ordering) | `read_u32_le(raw, 8)` → `exchange_sequence` in QuestDB | `deep_depth.rs:82`, `deep_depth_persistence.rs` | header tests |
| 17 | 200-lvl bytes 9-12: row count | `header.seq_or_row_count` used as `row_count` | `deep_depth.rs:200` | `test_parse_two_hundred_depth_partial_row_count` |
| 18 | Depth level: float64 price (8 bytes) | `read_f64_le()` — NOT f32 | `deep_depth.rs:133` | `test_parse_twenty_depth_bid_packet` |
| 19 | Depth level: uint32 quantity (4 bytes) | `read_u32_le()` | `deep_depth.rs:134` | all depth tests |
| 20 | Depth level: uint32 orders (4 bytes) | `read_u32_le()` | `deep_depth.rs:135` | all depth tests |
| 21 | 20 levels × 16 bytes = 320 body | `TWENTY_DEPTH_BODY_SIZE = 320` | `constants.rs:373` | compile assert |
| 22 | 20-level total = 332 bytes | `TWENTY_DEPTH_PACKET_SIZE = 332` | `constants.rs:376` | compile assert `constants.rs:1553` |
| 23 | 200-level: parse only row_count levels | `parse_deep_depth_levels(raw, row_count_usize, expected_size)` | `deep_depth.rs:210` | `test_parse_two_hundred_depth_partial_row_count` |
| 24 | 200-level: validate row_count <= 200 | `if row_count > TWO_HUNDRED_DEPTH_LEVELS → InvalidRowCount` | `deep_depth.rs:201-206` | `test_parse_two_hundred_depth_row_count_exceeds_max` |
| 25 | Bid/Ask arrive as SEPARATE packets | `DepthSide::Bid` / `DepthSide::Ask` per packet | `deep_depth.rs:54-58` | `test_parse_twenty_depth_bid/ask_packet` |
| 26 | **STACKED PACKETS** in single WS message | `split_stacked_depth_packets()` splits by `message_length` | `dispatcher.rs:143-166` | **12 tests** (see below) |
| 27 | Disconnect: code 50, reason at bytes 12-13 | 14-byte disconnect handling | per `full-market-depth.md` rule 16 | disconnect tests |
| 28 | Disconnect RequestCode: 12 | `build_disconnect_message()` | `subscription_builder.rs` | unit test |
| 29 | 805 = too many connections | `DisconnectCode::TooManyRequests` | `disconnect.rs` | `ws_disconnect_codes::*` |
| 30 | All binary reads Little Endian | `from_le_bytes()` everywhere, NEVER big endian | all parsers | compile-time pattern |

### 11.2 Stacked Packet Tests — Complete List

| Test | What It Proves | File:Line |
|------|---------------|-----------|
| `test_split_stacked_single_packet` | 1 packet → Vec of 1 | `dispatcher.rs:676` |
| `test_split_stacked_bid_ask_pair` | Bid+Ask → Vec of 2 | `dispatcher.rs:684` |
| `test_split_stacked_multiple_instruments` | 3 inst × 2 sides → Vec of 6 | `dispatcher.rs:698` |
| `test_split_stacked_empty` | Empty → empty Vec | `dispatcher.rs:718` |
| `test_split_stacked_trailing_bytes_ignored` | Trailing garbage < 12 bytes → ignored | `dispatcher.rs:724` |
| `test_split_stacked_zero_msg_length_stops` | 0-length header → stop splitting | `dispatcher.rs:734` |
| `test_split_stacked_exact_fit_included` | Exact fit → included | `dispatcher.rs:751` |
| `test_split_stacked_truncated_second_stops` | Truncated 2nd packet → only first returned | `dispatcher.rs:819` |
| `test_split_stacked_frames_correct_security_ids` | SecurityIDs survive splitting | `dispatcher.rs:888` |
| `test_dispatch_deep_depth_message_sequence_propagated` | Sequence number preserved | `dispatcher.rs:803` |
| `test_dispatch_deep_depth_received_at_nanos_propagated` | Timestamp preserved | `dispatcher.rs:788` |
| `test_dispatch_deep_depth_bid/ask` | Bid/Ask side correctly identified | `dispatcher.rs:630,653` |

### 11.3 Live Market Verification Queries (Run During 09:15-15:30 IST)

```sql
-- 1. Verify all 50 instruments per underlying have depth data (stacked packets working)
SELECT count(DISTINCT security_id) as instruments
FROM deep_market_depth
WHERE depth_type = '20' AND ts > dateadd('m', -5, now())
-- Expected: ~200 (50 per underlying × 4 underlyings)

-- 2. Verify both Bid and Ask sides arrive
SELECT side, count(*) FROM deep_market_depth
WHERE depth_type = '20' AND ts > dateadd('m', -5, now())
GROUP BY side
-- Expected: roughly equal BID and ASK counts

-- 3. Verify sequence numbers are monotonically increasing (gap detection)
SELECT security_id, exchange_sequence,
       exchange_sequence - lag(exchange_sequence) OVER (PARTITION BY security_id, side ORDER BY ts) as gap
FROM deep_market_depth
WHERE depth_type = '20' AND ts > dateadd('m', -5, now())
HAVING gap > 1
-- Expected: 0 rows (no gaps)

-- 4. Verify 200-level has data for ATM strikes
SELECT security_id, count(*) FROM deep_market_depth
WHERE depth_type = '200' AND ts > dateadd('m', -5, now())
GROUP BY security_id
-- Expected: 4 security_ids (NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY ATM)

-- 5. Verify price type is f64 precision (not truncated f32)
SELECT price FROM deep_market_depth
WHERE depth_type = '20' AND price > 0 AND ts > dateadd('m', -1, now())
LIMIT 10
-- Expected: prices with full decimal precision (e.g., 24532.75, not 24532.75000000)
```

### 11.4 Compile-Time Guarantees (Cannot Be Broken)

These are `const` assertions in `constants.rs` — if ANY value changes, the build FAILS:

```rust
const _: () = assert!(DEEP_DEPTH_HEADER_SIZE == 12);
const _: () = assert!(DEEP_DEPTH_LEVEL_SIZE == 16);
const _: () = assert!(TWENTY_DEPTH_PACKET_SIZE == 12 + 20 * 16);  // 332
const _: () = assert!(TWO_HUNDRED_DEPTH_PACKET_SIZE == 12 + 200 * 16);  // 3212
const _: () = assert!(DEEP_DEPTH_FEED_CODE_BID == 41);
const _: () = assert!(DEEP_DEPTH_FEED_CODE_ASK == 51);
const _: () = assert!(TICKER_PACKET_SIZE == 16);
const _: () = assert!(QUOTE_PACKET_SIZE == 50);
const _: () = assert!(FULL_QUOTE_PACKET_SIZE == 162);
const _: () = assert!(OI_PACKET_SIZE == 12);
const _: () = assert!(DISCONNECT_PACKET_SIZE == 10);
const _: () = assert!(MARKET_STATUS_PACKET_SIZE == 8);
```

### 11.5 O(1) Latency Guarantees

| Operation | Budget | Enforcement |
|-----------|--------|-------------|
| Tick parse (main feed) | <10ns | Benchmarked: `tick_parser` bench, budget in `benchmark-budgets.toml` |
| Depth header parse | O(1) | Fixed 12-byte read, no loops |
| Depth level parse (20-level) | O(20) = O(1) | Fixed 20 iterations, bounded |
| Depth level parse (200-level) | O(N), N ≤ 200 | Bounded by `row_count` validation |
| Stacked packet split | O(K), K ≤ 100 | Bounded by 50 instruments × 2 sides |
| Instrument lookup | O(1) | papaya concurrent map |
| QuestDB write | O(1) per level | ILP buffer append, no allocation |

### 11.6 Uniqueness & Deduplication Guarantees

| Layer | Key | Mechanism |
|-------|-----|-----------|
| Ticks (main feed) | `(ts, security_id, segment)` | QuestDB DEDUP UPSERT KEYS |
| Depth (20+200 level) | `(ts, security_id, segment, level, side)` | QuestDB DEDUP UPSERT KEYS |
| Candles | `(ts, security_id, segment, timeframe)` | QuestDB DEDUP UPSERT KEYS |
| Historical candles | `(ts, security_id, segment, timeframe)` | QuestDB DEDUP UPSERT KEYS |

---

## 12. Code File Map

| File | Purpose |
|------|---------|
| `crates/core/src/websocket/connection.rs` | Main feed WS connection (connect, subscribe, read loop) |
| `crates/core/src/websocket/connection_pool.rs` | 5-connection pool, staggered spawn |
| `crates/core/src/websocket/subscription_builder.rs` | JSON subscribe message builder (100 inst/msg batching) |
| `crates/core/src/websocket/depth_connection.rs` | 20-level + 200-level depth WS (reconnect, market hours) |
| `crates/core/src/websocket/order_update_connection.rs` | Order update WS (JSON auth, read loop) |
| `crates/core/src/websocket/tls.rs` | TLS connector (aws-lc-rs) |
| `crates/core/src/websocket/types.rs` | `InstrumentSubscription`, `WebSocketError`, `ConnectionState` |
| `crates/core/src/parser/header.rs` | 8-byte main feed header parser |
| `crates/core/src/parser/ticker.rs` | Ticker packet (16 bytes, code 2) |
| `crates/core/src/parser/quote.rs` | Quote packet (50 bytes, code 4) |
| `crates/core/src/parser/full.rs` | Full packet (162 bytes, code 8) |
| `crates/core/src/parser/oi.rs` | OI packet (12 bytes, code 5) |
| `crates/core/src/parser/prev_close.rs` | PrevClose packet (16 bytes, code 6) |
| `crates/core/src/parser/disconnect.rs` | Disconnect packet (10 bytes, code 50) |
| `crates/core/src/parser/market_depth.rs` | 5-level depth in Full packet (code 3, legacy) |
| `crates/core/src/parser/deep_depth.rs` | 20-level + 200-level depth parser (12-byte header) |
| `crates/core/src/pipeline/tick_processor.rs` | SPSC 65K buffer, packet routing, dedup, persist |
| `crates/storage/src/tick_persistence.rs` | QuestDB ILP writer for ticks |
| `crates/storage/src/deep_depth_persistence.rs` | QuestDB ILP writer for depth (ring buffer on disconnect) |
| `crates/app/src/main.rs` | Boot sequence: WS spawn (Step 8), depth spawn (Step 8c), order WS (Step 9) |

---

## 12. Ground Truth References

Always read these before modifying WebSocket code:

1. `docs/dhan-ref/03-live-market-feed-websocket.md` — Main feed binary protocol
2. `docs/dhan-ref/04-full-market-depth-websocket.md` — Depth protocol (20 + 200 level)
3. `docs/dhan-ref/10-live-order-update-websocket.md` — Order update JSON protocol
4. `docs/dhan-ref/08-annexure-enums.md` — Exchange segments, feed codes, disconnect codes, rate limits
5. `.claude/rules/dhan/live-market-feed.md` — Enforcement rules for main feed
6. `.claude/rules/dhan/full-market-depth.md` — Enforcement rules for depth
7. `.claude/rules/dhan/live-order-update.md` — Enforcement rules for order updates
8. `.claude/rules/project/websocket-enforcement.md` — This doc's enforcement counterpart
