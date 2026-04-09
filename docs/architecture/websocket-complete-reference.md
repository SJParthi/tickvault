# WebSocket Complete Reference — dhan-live-trader

> **Authority:** This doc + `docs/dhan-ref/03-live-market-feed-websocket.md` + `docs/dhan-ref/04-full-market-depth-websocket.md` + `docs/dhan-ref/10-live-order-update-websocket.md`
>
> **When to read:** Before touching ANY file in `crates/core/src/websocket/`, `crates/core/src/parser/`, `crates/app/src/main.rs` (WS spawn sections), or `crates/storage/src/deep_depth_persistence.rs`.
>
> **Last updated:** 2026-04-09

---

## 1. Architecture Overview

The system uses 4 independent WebSocket types, each with its own connection pool:

| Type | Endpoint | Protocol | Connections | Instruments | Pool |
|------|----------|----------|-------------|-------------|------|
| Live Market Feed | `wss://api-feed.dhan.co` | Binary (8-byte header) | 5 | 25,000 | Independent |
| 20-Level Depth | `wss://depth-api-feed.dhan.co/twentydepth` | Binary (12-byte header) | 4 | 200 (50 each) | Independent |
| 200-Level Depth | `wss://full-depth-api.dhan.co/` | Binary (12-byte header) | 4 | 4 (1 each, ATM) | Independent |
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
| 8-11 | u32 LE | Sequence Number (informational, ignore) |

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

### 3.8 NO TIMESTAMP

**Dhan does NOT include any timestamp in depth packets.** We use `received_at_nanos` (local system clock when frame arrives). Same approach as Dhan's own Python SDK.

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
- We use 4: NIFTY ATM, BANKNIFTY ATM, FINNIFTY ATM, MIDCPNIFTY ATM

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

### 4.7 Depth Rebalancer

Checks every 60s if spot price has drifted > 3 strikes from current ATM. If so, disconnects and reconnects with the new ATM strike's security ID.

### 4.8 Read Timeout

40 seconds. If no data for 40s, connection assumed dead.

### 4.9 Off-Hours Behavior

Outside market hours (before 08:55 or after 16:00 IST):
- Dhan accepts connection but sends no data
- Read timeout fires after 40s
- Sleep until 08:55 IST next day (safety net: wake every 5 min to recheck)
- During market hours: exponential backoff reconnect (1s to 30s max)

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
- **Timestamp:** `exchange_timestamp` (LTT from packet) + IST offset
- **Price:** f32 → f64 via `f32_to_f64_clean()` to prevent IEEE 754 widening artifacts

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
| `WebSocketDisconnected { connection_index, reason }` | Not currently wired (gap) | High |
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

### 10.2 Open Gaps

#### CRITICAL

| # | Gap | File:Line | Impact |
|---|-----|-----------|--------|
| C1 | No graceful unsubscribe on shutdown — connections close without sending unsubscribe messages | `connection.rs:193-202` | Dhan keeps instruments subscribed briefly, wasting bandwidth |
| C2 | Order update auth never validated — code enters read loop immediately after login, waits for implicit error on timeout | `order_update_connection.rs:187-193` | Silent auth failure for 30+ seconds |
| C3 | Order update token not wrapped in `Zeroizing` — exposed `.to_string()` stays in memory | `order_update_connection.rs:153` | Token not zeroized on drop (security) |

#### HIGH

| # | Gap | File:Line | Impact |
|---|-----|-----------|--------|
| H1 | `WebSocketDisconnected` event defined but never sent in production | `connection.rs:228-235` | Main feed disconnections not alerted via Telegram |
| H2 | No pool-level circuit breaker — if all 5 main feed connections die, no explicit alert fires | `connection_pool.rs` (missing entirely) | Silent total data loss |
| H3 | No per-security_id stale tick detection — if one instrument stops receiving ticks, no alert | `tick_processor.rs` | Silent data gap for single security while others stream fine |
| H4 | No dead-letter mechanism for unparseable packets — frames discarded without forensics | `tick_processor.rs:501` | Can't diagnose parse failures post-mortem |
| H5 | No Telegram alert when depth frame parsing fails | `depth_connection.rs` | Parse errors logged but not escalated |

#### MEDIUM

| # | Gap | File:Line | Impact |
|---|-----|-----------|--------|
| M1 | Telegram fires on connection spawn, not on first DATA frame received | `main.rs` (WS spawn) | Off-hours "connected" alerts are misleading |
| M2 | `WebSocketReconnected` event defined but never sent | `connection.rs` | No Telegram on successful reconnection |
| M3 | No pool-level health check / heartbeat monitor — passive snapshot only | `connection_pool.rs:191-193` | Single connection hang goes undetected at pool level |
| M4 | No token refresh acknowledgment log — "waiting for renewal" logged but not "renewal complete" | `connection.rs:225-226` | Hard to audit token refresh flow in logs |
| M5 | WebSocket URL with `?token=` could leak in error messages | `connection.rs:301-303` | Token visible in log aggregation |
| M6 | No CRITICAL alert if SPSC backpressure exceeds threshold | `tick_processor.rs:436-441` | Data loss silently accumulates |
| M7 | Order update JSON parse errors for non-order messages silently dropped | `order_update_connection.rs:248-251` | No metric for dropped messages |
| M8 | No 20-level depth sequence number validation — packet loss goes undetected | `depth_connection.rs:554` | Missing depth levels not caught |

#### LOW

| # | Gap | File:Line | Impact |
|---|-----|-----------|--------|
| L1 | No Telegram for depth rebalancer ATM strike change | `main.rs` (rebalancer spawn) | Silent rebalance, hard to audit |
| L2 | 20-level depth connections stay connected off-hours (no sleep like 200-level) | `depth_connection.rs` | Unnecessary open connections |
| L3 | 200-level depth read timeout 40s may be too aggressive for illiquid strikes during market hours | `depth_connection.rs:40` | Could disconnect during low-activity periods |
| L4 | No URL change detection — if Dhan changes endpoint URL, repeated failures until restart | config-based | No runtime URL override |
| L5 | No TLS handshake failure classification (transient vs permanent) | `depth_connection.rs` | All TLS errors treated same |
| L6 | Subscription builder allows duplicate SecurityIds without dedup | `subscription_builder.rs` | Dhan may reject or silently dedupe |
| L7 | Unknown response codes from Dhan silently treated as errors — no payload sample logged | `tick_processor.rs:540-550` | Can't diagnose new packet types |

#### ACCEPTED (Dhan Limitation / Design Decision)

| # | Decision | Rationale |
|---|----------|-----------|
| A1 | No exchange timestamp in depth packets | Dhan protocol has no timestamp — all SDKs use local clock |
| A2 | 200-level uses 1 connection per instrument | Dhan API limitation, not our choice |
| A3 | Depth data only flows during 09:15-15:30 IST | Dhan sends nothing outside market hours |
| A4 | PrevClose packets arrive on any subscription mode | Expected — used for day change % calculations |
| A5 | Unsubscribe depth = 25 (not SDK's 24) | SDK bug; we follow Dhan docs |

### 10.3 Missing Metrics

| Metric | Purpose |
|--------|---------|
| `dlt_websocket_pool_all_dead` | Bool gauge: true if all 5 main feed connections are down |
| `dlt_websocket_failed_connections_count` | Gauge: number of connections in Reconnecting state |
| `dlt_depth_20lvl_sequence_gaps_total` | Counter: detected sequence gaps in 20-level packets |
| `dlt_order_update_non_order_messages_dropped_total` | Counter: JSON messages that weren't order updates |
| `dlt_unknown_response_codes_total` | Counter: Dhan sent a response code we don't recognize |
| `dlt_packets_by_response_code` | Histogram: breakdown of all received packet types |

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

## 11. Code File Map

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
