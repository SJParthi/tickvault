# PHASE 1 — LIVE TRADING SYSTEM

## Purpose

Build a complete, production-grade live F&O trading system for Indian markets (NSE/BSE). Phase 1 delivers: real-time market data ingestion, O(1) binary parsing, multi-timeframe candle aggregation, option chain analytics, order management, and full observability — all running in Docker containers with zero manual intervention.

---

## Table of Contents

1. [Critical Changes: V2 to V3](#1-critical-changes-v2-to-v3)
2. [QuestDB O(1) Database Architecture](#2-questdb-o1-database-architecture)
3. [21 Candle Timeframes](#3-21-candle-timeframes)
4. [Top Gainers & Losers](#4-top-gainers--losers)
5. [Dhan Live Market Feed — Binary WebSocket Protocol](#5-dhan-live-market-feed--binary-websocket-protocol)
6. [Full Market Depth Protocol](#6-full-market-depth-protocol)
7. [Dhan Annexure — Enums, Codes & Errors](#7-dhan-annexure--enums-codes--errors)
8. [Authentication & Token Management](#8-authentication--token-management)
9. [Order Management API](#9-order-management-api)
10. [Live Order Update WebSocket](#10-live-order-update-websocket)
11. [O(1) Component Scoring](#11-o1-component-scoring)
12. [Crate Dependency Map](#12-crate-dependency-map)
13. [Docker-First Stack](#13-docker-first-stack)
14. [Latency Budget](#14-latency-budget)
15. [End-to-End Processing Pipeline](#15-end-to-end-processing-pipeline)
16. [Warmup & Historical Data](#16-warmup--historical-data)
17. [Deliverables Checklist](#17-deliverables-checklist)
18. [Final Summary](#18-final-summary)

---

## 1. Critical Changes: V2 to V3

### What Changed from Phase 1 V2 to V3

| Area | V2 (Old) | V3 (Current) | Why |
|------|----------|--------------|-----|
| Candle timeframes | 7 timeframes | 21 timeframes | Complete coverage for all trading styles |
| QuestDB schema | Flat tables | Materialized views + partitioning | O(1) query for any timeframe |
| Top Gainers/Losers | Not included | Full implementation | Essential dashboard feature |
| Market depth | 5-level only | 5-level + 20-level + 200-level | Full depth book support |
| Order management | Basic | Complete lifecycle with all order types | Production-ready OMS |
| Authentication | Manual token | Fully automated TOTP → JWT → refresh | Zero manual intervention |
| Candle source | REST API polling | WebSocket tick aggregation | Lower latency, no rate limits |

### Key Architectural Decisions in V3

1. **Tick-based candle building**: All 21 timeframes built from raw WebSocket ticks, NOT from REST API polling. This gives sub-second candle updates and eliminates REST API rate limit concerns.

2. **QuestDB materialized views**: Base 1-second table feeds materialized views for all higher timeframes. QuestDB does the aggregation, Rust reads O(1).

3. **Separation of market data and order data WebSockets**: Market feed (binary protocol) and order updates (JSON protocol) are completely separate connections with independent reconnection logic.

---

## 2. QuestDB O(1) Database Architecture

### Design Principle

Every query the system makes must be O(1) — constant time regardless of data volume. QuestDB's time-series engine with designated timestamps and partitioning makes this achievable.

### Core Tables

#### Table: `ticks` (Base — All Raw Tick Data)

```sql
CREATE TABLE ticks (
    ts              TIMESTAMP,      -- exchange timestamp (designated)
    received_at     TIMESTAMP,      -- local receipt time (latency measurement)
    security_id     INT,            -- Dhan security ID
    segment         SYMBOL,         -- IDX_I, NSE_EQ, NSE_FNO, BSE_FNO
    ltp             DOUBLE,         -- last traded price
    open            DOUBLE,         -- day open
    high            DOUBLE,         -- day high
    low             DOUBLE,         -- day low
    close           DOUBLE,         -- previous close
    volume          LONG,           -- cumulative volume
    oi              LONG,           -- open interest (derivatives only)
    bid_price       DOUBLE,         -- best bid
    bid_qty         INT,            -- best bid quantity
    ask_price       DOUBLE,         -- best ask
    ask_qty         INT,            -- best ask quantity
    avg_price       DOUBLE,         -- volume-weighted average price
    turnover        DOUBLE,         -- day turnover
    last_trade_qty  INT,            -- last trade quantity
    total_buy_qty   LONG,           -- total buy quantity
    total_sell_qty  LONG            -- total sell quantity
) TIMESTAMP(ts) PARTITION BY HOUR
  DEDUP UPSERT KEYS(ts, security_id);
```

**Why PARTITION BY HOUR:** 375 minutes of trading = ~6 partitions. Each partition is a separate memory-mapped file. QuestDB reads only the relevant partition for any time-range query. With ~15K subscribed instruments producing ticks, hourly partitions keep partition sizes manageable (~50-100MB each).

**Why DEDUP UPSERT KEYS:** Dhan may send duplicate ticks on WebSocket reconnection. QuestDB automatically deduplicates based on (timestamp, security_id). Zero application-level dedup code needed for storage.

#### Table: `candles_1s` (1-Second Aggregation)

```sql
CREATE TABLE candles_1s (
    ts              TIMESTAMP,      -- second boundary (designated)
    security_id     INT,
    segment         SYMBOL,
    open            DOUBLE,
    high            DOUBLE,
    low             DOUBLE,
    close           DOUBLE,
    volume          LONG,           -- volume in this 1-second window
    oi              LONG,           -- latest OI in this window
    tick_count      INT             -- number of ticks aggregated
) TIMESTAMP(ts) PARTITION BY DAY
  DEDUP UPSERT KEYS(ts, security_id);
```

**Built by Rust**: Aggregate ticks into 1-second candles in the tick processing pipeline (in-memory, then flush to QuestDB). This is the base table for all higher timeframe materialized views.

### Materialized Views (QuestDB SAMPLE BY)

QuestDB's `SAMPLE BY` clause creates materialized views that automatically aggregate from lower timeframes. These are O(1) reads — QuestDB pre-computes the results.

```sql
-- 1-Minute candles from 1-second base
CREATE MATERIALIZED VIEW candles_1m AS
  SELECT ts, security_id, segment,
         first(open) AS open, max(high) AS high,
         min(low) AS low, last(close) AS close,
         sum(volume) AS volume, last(oi) AS oi,
         sum(tick_count) AS tick_count
  FROM candles_1s
  SAMPLE BY 1m
  ALIGN TO CALENDAR WITH OFFSET '05:30';

-- 5-Minute candles
CREATE MATERIALIZED VIEW candles_5m AS
  SELECT ts, security_id, segment,
         first(open) AS open, max(high) AS high,
         min(low) AS low, last(close) AS close,
         sum(volume) AS volume, last(oi) AS oi
  FROM candles_1m
  SAMPLE BY 5m
  ALIGN TO CALENDAR WITH OFFSET '05:30';
```

**ALIGN TO CALENDAR WITH OFFSET '05:30'**: Aligns candle boundaries to IST (UTC+5:30). A "1-minute candle at 9:15" starts at 9:15:00 IST and ends at 9:15:59 IST.

### Rust Integration with QuestDB

**Writes (Hot Path — ILP Protocol):**
```
Tick received → parse → aggregate 1s candle → ILP flush to QuestDB
Uses: questdb-rs 6.1.0 via TCP ILP (port 9009)
Batching: flush every 1 second or every 1000 rows, whichever comes first
```

**Reads (Cold Path — PostgreSQL Wire Protocol):**
```
Dashboard/API request → PG wire query → QuestDB → response
Uses: QuestDB PG wire protocol (port 8812)
Connection pool: managed by the app, bounded
```

**Reads (Warm Path — HTTP REST):**
```
Health checks, ad-hoc queries → HTTP REST API (port 9000)
Uses: reqwest to QuestDB HTTP endpoint
```

---

## 3. 21 Candle Timeframes

### Complete Timeframe Table

| # | Timeframe | QuestDB Source | Method | Trading Use |
|---|-----------|---------------|--------|-------------|
| 1 | 1 Second | `candles_1s` | Rust aggregation from ticks | Scalping, tick analysis |
| 2 | 5 Seconds | Materialized view from 1s | SAMPLE BY 5s | Ultra-short-term |
| 3 | 10 Seconds | Materialized view from 1s | SAMPLE BY 10s | Scalping |
| 4 | 15 Seconds | Materialized view from 1s | SAMPLE BY 15s | Scalping |
| 5 | 30 Seconds | Materialized view from 1s | SAMPLE BY 30s | Quick scalping |
| 6 | 1 Minute | Materialized view from 1s | SAMPLE BY 1m | Standard intraday |
| 7 | 2 Minutes | Materialized view from 1m | SAMPLE BY 2m | Short-term |
| 8 | 3 Minutes | Materialized view from 1m | SAMPLE BY 3m | Short-term |
| 9 | 5 Minutes | Materialized view from 1m | SAMPLE BY 5m | Intraday standard |
| 10 | 10 Minutes | Materialized view from 5m | SAMPLE BY 10m | Medium intraday |
| 11 | 15 Minutes | Materialized view from 5m | SAMPLE BY 15m | Swing intraday |
| 12 | 30 Minutes | Materialized view from 15m | SAMPLE BY 30m | Position intraday |
| 13 | 1 Hour | Materialized view from 30m | SAMPLE BY 1h | Swing trading |
| 14 | 2 Hours | Materialized view from 1h | SAMPLE BY 2h | Swing trading |
| 15 | 3 Hours | Materialized view from 1h | SAMPLE BY 3h | Position trading |
| 16 | 4 Hours | Materialized view from 1h | SAMPLE BY 4h | Position trading |
| 17 | 1 Day | Materialized view from 1h | SAMPLE BY 1d | Daily analysis |
| 18 | 1 Week | Materialized view from 1d | SAMPLE BY 7d | Weekly analysis |
| 19 | 1 Month | Materialized view from 1d | SAMPLE BY 1M | Monthly analysis |
| 20 | 3 Months | Computed from 1M | Rust aggregation | Quarterly |
| 21 | 1 Year | Computed from 1M | Rust aggregation | Annual |

### Building Rules

- **Timeframes 1-19**: QuestDB materialized views handle aggregation. Rust reads pre-computed results.
- **Timeframes 20-21** (3M, 1Y): Computed in Rust from monthly candles (QuestDB doesn't support `SAMPLE BY 3M` natively).
- **All timeframes aligned to IST** via `ALIGN TO CALENDAR WITH OFFSET '05:30'`.
- **Trading session candles** (9:15-15:30): 375 one-minute candles per day.
- **Pre/post market candles** (9:00-9:15, 15:30-16:00): Stored separately, not in trading timeframes.

---

## 4. Top Gainers & Losers

### What It Tracks

Real-time ranking of F&O underlyings by price change percentage. Updated on every tick for subscribed instruments.

### QuestDB Table

```sql
CREATE TABLE top_movers (
    ts              TIMESTAMP,      -- tick timestamp (designated)
    security_id     INT,
    symbol          SYMBOL,         -- underlying symbol
    segment         SYMBOL,         -- exchange segment
    ltp             DOUBLE,         -- current price
    prev_close      DOUBLE,         -- previous day close
    change_pct      DOUBLE,         -- percentage change
    change_abs      DOUBLE,         -- absolute change
    volume          LONG,           -- current day volume
    oi              LONG            -- current open interest
) TIMESTAMP(ts) PARTITION BY DAY;
```

### Computation

```
On each tick for an F&O underlying:
  1. change_abs = ltp - prev_close
  2. change_pct = (change_abs / prev_close) * 100
  3. Update in-memory sorted structure (top N gainers, top N losers)
  4. Periodically flush snapshot to QuestDB (every 5 seconds)
```

### Categories

| Category | Instruments | Sort |
|----------|------------|------|
| Index Gainers | 8 F&O indices | change_pct DESC |
| Index Losers | 8 F&O indices | change_pct ASC |
| Stock Gainers | 207 F&O stocks | change_pct DESC, top 20 |
| Stock Losers | 207 F&O stocks | change_pct ASC, top 20 |
| Most Active (Volume) | All F&O underlyings | volume DESC, top 20 |
| Most Active (OI Change) | All F&O derivatives | oi_change DESC, top 20 |

### API Response

```json
{
  "timestamp": "2026-03-15T10:30:45+05:30",
  "gainers": {
    "indices": [
      {"symbol": "NIFTY", "ltp": 22450.50, "change_pct": 1.25, "volume": 1234567}
    ],
    "stocks": [
      {"symbol": "RELIANCE", "ltp": 2890.00, "change_pct": 3.45, "volume": 9876543}
    ]
  },
  "losers": {
    "indices": [...],
    "stocks": [...]
  },
  "most_active": {
    "by_volume": [...],
    "by_oi_change": [...]
  }
}
```

---

## 5. Dhan Live Market Feed — Binary WebSocket Protocol

### Connection Details

| Parameter | Value |
|-----------|-------|
| Endpoint | `wss://api-feed.dhan.co` (from config TOML, not hardcoded) |
| Protocol | Binary frames (Little Endian byte order) |
| Auth | `Authorization: access-token` header on connection |
| Client ID | `Client-Id: <dhan_client_id>` header on connection |
| Max connections | 5 per Dhan account |
| Max instruments per connection | 5,000 |
| Total capacity | 25,000 instruments across all connections |
| Keep-alive | Client sends ping every 10 seconds |
| Server timeout | Disconnects if no ping received within 40 seconds |

### Subscription Request (JSON)

After WebSocket connection is established, subscribe to instruments by sending a JSON message:

```json
{
  "RequestCode": 15,
  "InstrumentCount": 3,
  "InstrumentList": [
    {
      "ExchangeSegment": "IDX_I",
      "SecurityId": "13"
    },
    {
      "ExchangeSegment": "NSE_EQ",
      "SecurityId": "2885"
    },
    {
      "ExchangeSegment": "NSE_FNO",
      "SecurityId": "52432"
    }
  ]
}
```

**RequestCode values:**

| Code | Feed Mode | Description |
|------|-----------|-------------|
| 15 | Ticker | Compact price data (LTP, open, high, low, close, volume) |
| 17 | Quote | Ticker + best bid/ask |
| 21 | Full | Quote + OI + 5-level market depth |

**Rules:**
- Maximum 100 instruments per subscription message
- For >100 instruments, send multiple subscription messages
- `SecurityId` is sent as a STRING in JSON, even though it's numeric
- `ExchangeSegment` values: `"IDX_I"`, `"NSE_EQ"`, `"NSE_FNO"`, `"BSE_EQ"`, `"BSE_FNO"`, `"MCX_COMM"`

### Unsubscription Request

```json
{
  "RequestCode": 12,
  "InstrumentCount": 1,
  "InstrumentList": [
    {
      "ExchangeSegment": "NSE_FNO",
      "SecurityId": "52432"
    }
  ]
}
```

### Binary Response Format

All binary responses start with an 8-byte header:

```
Byte 0:     Response Code (u8)
Bytes 1-2:  Message Length (i16, Little Endian) — total bytes including header
Byte 3:     Exchange Segment code (u8)
Bytes 4-7:  Security ID (i32, Little Endian)
```

**Exchange Segment Codes (binary protocol):**

| Code | Segment |
|------|---------|
| 0 | IDX_I |
| 1 | NSE_EQ |
| 2 | NSE_FNO |
| 3 | NSE_CURRENCY |
| 4 | BSE_EQ |
| 5 | MCX_COMM |
| 7 | BSE_CURRENCY |
| 8 | BSE_FNO |

### Response Code 2 — Ticker Packet (16 bytes total)

SDK format: `<BHBIfI>`

```
Bytes 0-7:   Header (response_code=2, length, segment, security_id)
Bytes 8-11:  LTP (f32, Little Endian) — direct price in rupees, NO division
Bytes 12-15: LTT (u32, Little Endian) — last trade time (epoch seconds, IST-naive)
```

**Price decoding for Ticker:**
- LTP: Read f32 at offset 8 — value IS the price in rupees (no /100)
- LTT: Read u32 at offset 12 — IST-naive epoch seconds

### Response Code 4 — Quote Packet (50 bytes total)

SDK format: `<BHBIfHIfIIIffff>`

CRITICAL: OHLC starts at offset 34 (diverges from Full packet at this point).

```
Bytes 0-7:   Header (response_code=4)
Bytes 8-11:  LTP (f32 LE)
Bytes 12-13: LTQ — Last Trade Quantity (u16 LE)
Bytes 14-17: LTT — Last Trade Time (u32 LE, epoch seconds)
Bytes 18-21: ATP — Average Traded Price (f32 LE)
Bytes 22-25: Volume (u32 LE)
Bytes 26-29: Total Sell Quantity (u32 LE)
Bytes 30-33: Total Buy Quantity (u32 LE)
Bytes 34-37: Day Open (f32 LE)
Bytes 38-41: Day Close / Prev Close (f32 LE)
Bytes 42-45: Day High (f32 LE)
Bytes 46-49: Day Low (f32 LE)
```

### Response Code 8 — Full Packet (162 bytes total)

SDK format: `<BHBIfHIfIIIIIIffff100s>`

CRITICAL: Diverges from Quote at offset 34. Full has OI (u32) where Quote has OHLC (f32).

```
Bytes 0-7:   Header (response_code=8)
Bytes 8-11:  LTP (f32 LE)
Bytes 12-13: LTQ — Last Trade Quantity (u16 LE)
Bytes 14-17: LTT — Last Trade Time (u32 LE, epoch seconds)
Bytes 18-21: ATP — Average Traded Price (f32 LE)
Bytes 22-25: Volume (u32 LE)
Bytes 26-29: Total Sell Quantity (u32 LE)
Bytes 30-33: Total Buy Quantity (u32 LE)
Bytes 34-37: OI — Open Interest (u32 LE)        ← Quote has Day Open here!
Bytes 38-41: OI Day High (u32 LE)               ← Quote has Day Close here!
Bytes 42-45: OI Day Low (u32 LE)                ← Quote has Day High here!
Bytes 46-49: Day Open (f32 LE)
Bytes 50-53: Day Close / Prev Close (f32 LE)
Bytes 54-57: Day High (f32 LE)
Bytes 58-61: Day Low (f32 LE)

-- 5-Level Market Depth (Bytes 62-161) --
Each level = 20 bytes:
  Bytes 0-3:   Bid Quantity (u32 LE)
  Bytes 4-7:   Ask Quantity (u32 LE)
  Bytes 8-9:   Bid Orders (u16 LE)
  Bytes 10-11: Ask Orders (u16 LE)
  Bytes 12-15: Bid Price (f32 LE)
  Bytes 16-19: Ask Price (f32 LE)

5 levels × 20 bytes = 100 bytes (offsets 62-161)
```

### Response Code 1 — Index Ticker (16 bytes)

Same structure as Response Code 2 (Ticker), but specifically for IDX_I segment instruments. Same 16-byte layout, same decoding rules.

### Response Code 5 — OI Data (12 bytes total)

SDK format: `<BHBII>`

```
Bytes 0-7:   Header (response_code=5)
Bytes 8-11:  Open Interest (u32 LE)
```

### Response Code 6 — Previous Close (16 bytes total)

SDK format: `<BHBIfI>`

```
Bytes 0-7:   Header (response_code=6)
Bytes 8-11:  Previous Close Price (f32 LE) — direct price, NO division
Bytes 12-15: Previous OI (u32 LE)
```

**When received:** Sent once per instrument on subscription or market open. Used to calculate day change percentages.

### Response Code 7 — Market Status (10 bytes total)

```
Bytes 0-7:   Header (response_code=7)
Bytes 8-9:   Market Status Code (i16 LE)
```

**Market Status Codes:**

| Code | Status |
|------|--------|
| 0 | Market Closed |
| 1 | Pre-Open |
| 2 | Market Open |
| 3 | Post-Close |

### Response Code 50 — Disconnect (variable length)

```
Bytes 0-7:   Header (response_code=50)
Bytes 8+:    Disconnect reason (string, variable length)
```

**Disconnect Error Codes:**

| Code | Meaning | Action |
|------|---------|--------|
| 801 | Exceeded max connections (5) | Cannot connect — reduce connections |
| 802 | Exceeded max instruments per connection (5000) | Reduce subscription count |
| 803 | Authentication failed — invalid access token | Refresh token immediately |
| 804 | Token expired | Refresh token + reconnect |
| 805 | Disconnected by server (maintenance) | Wait + reconnect with backoff |
| 806 | Connection timeout (no ping in 40s) | Bug in ping logic — fix immediately |
| 807 | Invalid subscription request | Fix request format |
| 808 | Subscription limit exceeded (25000 total) | Reduce instrument count |
| 810 | Force-disconnected (Dhan kill switch) | Wait + retry later |
| 814 | Invalid client ID | Check SSM credentials |

### Keep-Alive Protocol

```
Client → Server: Ping frame every 10 seconds
Server → Client: Pong frame in response
If server doesn't receive ping within 40 seconds → disconnects (code 806)
```

**Implementation:**
- Dedicated ping task running on tokio interval timer
- Ping interval: 10 seconds (from config, not hardcoded)
- Pong timeout detection: if no pong within 10 seconds after ping, log WARN
- If 2 consecutive pong failures, close + reconnect

---

## 6. Full Market Depth Protocol

### 20-Level Depth WebSocket

| Parameter | Value |
|-----------|-------|
| Endpoint | `wss://depth-api-feed.dhan.co/twentydepth` (from config) |
| Auth | Token + client ID as URL query params (`?token=<JWT>&clientId=<id>&authType=2`) |
| Max connections | 5 (shared limit with standard feed) |
| Max instruments per connection | 50 |
| Protocol | Binary, Little Endian |
| Segments | NSE Equity and NSE Derivatives ONLY |

**Subscription request:** Same JSON format as live feed but with `RequestCode: 23`.

```json
{
  "RequestCode": 23,
  "InstrumentCount": 1,
  "InstrumentList": [
    {"ExchangeSegment": "NSE_EQ", "SecurityId": "2885"}
  ]
}
```

**Response packet structure:**

Bid and Ask sides arrive as **SEPARATE** binary packets (unlike the standard 5-level feed which combines them). Each packet has feed code 41 (Bid) or 51 (Ask).

12-Byte Deep Depth Header:
```
Bytes 0-1:   Message Length (u16 LE) — total packet size
Byte 2:      Feed Response Code (u8) — 41=Bid, 51=Ask
Byte 3:      Exchange Segment code (u8)
Bytes 4-7:   Security ID (u32 LE)
Bytes 8-11:  Message Sequence (u32 LE)
```

Body (per side):
```
Each level = 16 bytes:
  Price (f64 LE — 8 bytes, rupees, NO division needed)
  Quantity (u32 LE — 4 bytes)
  Orders (u32 LE — 4 bytes)

20 levels × 16 bytes = 320 bytes
Total per-side packet: 12 (header) + 320 (body) = 332 bytes
```

**Stacking:** When multiple instruments are subscribed, packets are stacked sequentially in a single WebSocket message. Parse by splitting on message length boundaries: instrument1-bid, instrument1-ask, instrument2-bid, instrument2-ask, etc.

**Key differences from 5-level depth:**
- Separate WebSocket endpoint (not `api-feed.dhan.co`)
- 12-byte header (vs 8-byte standard header)
- Bid/Ask arrive as separate packets (feed codes 41/51)
- Prices are f64 (double, 8 bytes) instead of f32 (4 bytes)
- Per-level layout differs: `price(f64) + qty(u32) + orders(u32)` = 16 bytes vs `bid_qty(u32) + ask_qty(u32) + bid_orders(u16) + ask_orders(u16) + bid_price(f32) + ask_price(f32)` = 20 bytes

### 200-Level Depth WebSocket

| Parameter | Value |
|-----------|-------|
| Endpoint | `wss://full-depth-api.dhan.co/twohundreddepth` (from config) |
| Auth | Token + client ID as URL query params (same as 20-depth) |
| Max connections | 5 (shared limit) |
| Max instruments per connection | **1** (critical limitation) |
| Segments | NSE Equity and NSE Derivatives ONLY |

**Response packet structure:**

Identical format to 20-level depth, but with 200 levels per side instead of 20.

12-Byte Header: Same as 20-depth (feed codes 41/51).

Body (per side):
```
200 levels × 16 bytes = 3,200 bytes
Total per-side packet: 12 (header) + 3,200 (body) = 3,212 bytes
```

**Limitation:** No total volume traded data in the 200-depth feed — only price, quantity, and orders per level.

**NOTE:** 200-depth is limited to **1 instrument per connection**. To monitor N instruments at 200-level depth, N WebSocket connections are required.

---

## 7. Dhan Annexure — Enums, Codes & Errors

### Exchange Segment Enum

| Segment String | Binary Code | Description |
|---------------|-------------|-------------|
| IDX_I | 0 | Indices |
| NSE_EQ | 1 | NSE Equity |
| NSE_FNO | 2 | NSE Futures & Options |
| NSE_CURRENCY | 3 | NSE Currency Derivatives |
| BSE_EQ | 4 | BSE Equity |
| BSE_FNO | 5 | BSE Futures & Options |
| MCX_COMM | 7 | MCX Commodities |

Note: Code 6 is unused/skipped in the protocol.

### Transaction Type Enum

| Value | Meaning |
|-------|---------|
| BUY | Buy order |
| SELL | Sell order |

### Product Type Enum

| Value | Meaning | Description |
|-------|---------|-------------|
| CNC | Cash & Carry | Delivery-based equity holding |
| INTRADAY | Intraday | Squared off same day |
| MARGIN | Margin | Leveraged delivery |
| MTF | Margin Trading Facility | Broker-funded leverage |
| CO | Cover Order | With stop-loss |
| BO | Bracket Order | With target + stop-loss |

### Order Type Enum

| Value | Meaning |
|-------|---------|
| LIMIT | Limit order — price specified |
| MARKET | Market order — best available price |
| STOP_LOSS | SL order — triggers at stop price, then limit |
| STOP_LOSS_MARKET | SL-M order — triggers at stop price, then market |

### Validity Enum

| Value | Meaning |
|-------|---------|
| DAY | Valid for the trading day |
| IOC | Immediate or Cancel |

### Order Status Enum

| Value | Meaning |
|-------|---------|
| TRANSIT | Order in transit to exchange |
| PENDING | Pending at exchange |
| CONFIRMED | Order confirmed/open |
| TRADED | Fully executed |
| CANCELLED | Cancelled by user or system |
| REJECTED | Rejected by exchange or RMS |
| EXPIRED | Expired at end of day |

### Dhan REST API Error Codes

| HTTP Status | Error Code | Meaning |
|-------------|-----------|---------|
| 400 | DH-901 | Invalid request parameters |
| 400 | DH-902 | Missing required fields |
| 400 | DH-903 | Invalid order type |
| 400 | DH-904 | Invalid product type |
| 400 | DH-905 | Invalid exchange segment |
| 401 | DH-100 | Authentication failed |
| 401 | DH-101 | Token expired |
| 401 | DH-102 | Invalid access token |
| 403 | DH-200 | Insufficient permissions |
| 429 | DH-300 | Rate limit exceeded (10 orders/sec) |
| 500 | DH-500 | Internal server error |
| 502 | DH-501 | Exchange connectivity issue |
| 503 | DH-502 | Service temporarily unavailable |

---

## 8. Authentication & Token Management

### Authentication Flow

```
Step 1: Read credentials from AWS SSM Parameter Store
        - /dlt/<env>/dhan/client-id      → Dhan Client ID
        - /dlt/<env>/dhan/client-secret   → Dhan Client Secret (password)
        - /dlt/<env>/dhan/totp-secret     → TOTP Base32 secret

Step 2: Generate TOTP code
        - Library: totp-rs 5.7.0 (from Bible)
        - Algorithm: SHA-1, 6 digits, 30-second period
        - Generate current TOTP code from the base32 secret

Step 3: Call Dhan REST API to get JWT access token
        POST {config.dhan.rest_api_base_url}/v2/generateAccessToken
        Content-Type: application/json

        {
            "client_id": "<from SSM>",
            "client_secret": "<from SSM>",
            "totp": "<generated TOTP code>"
        }

        Response:
        {
            "status": "success",
            "data": {
                "access_token": "<JWT token>",
                "token_type": "Bearer",
                "expires_in": 86400
            }
        }

Step 4: Store token via arc-swap
        - ArcSwap<TokenState> holds the active token
        - O(1) atomic pointer swap — zero lock contention
        - All WebSocket connections and REST calls read via arc-swap load()
        - Old token zeroized on drop (zeroize crate)

Step 5: Schedule renewal
        - Token valid for 24 hours (86400 seconds)
        - Refresh at 23 hours (1 hour before expiry)
        - Retry: exponential backoff via backon (100ms → 30s, 5 attempts)
        - Circuit breaker: failsafe (trip after 3 consecutive failures)
        - Alert: Telegram after 2nd failure, SNS after 5th
```

### Token Renewal via RenewToken

```
Before expiry (at refresh window):

POST {config.dhan.rest_api_base_url}/v2/renewToken
Authorization: <current_access_token>
Content-Type: application/json

{
    "client_id": "<from SSM>",
    "totp": "<fresh TOTP code>"
}

Response (same structure as generateAccessToken):
{
    "status": "success",
    "data": {
        "access_token": "<new JWT token>",
        "token_type": "Bearer",
        "expires_in": 86400
    }
}
```

**If renewal fails:** Fall back to full authentication (Step 1-3) using generateAccessToken.

### Token State Machine

```
                    ┌─────────────┐
                    │  No Token   │
                    └──────┬──────┘
                           │ generateAccessToken
                           ▼
                    ┌─────────────┐
             ┌──────│   Active    │──────┐
             │      └──────┬──────┘      │
             │             │ 23h elapsed │ token invalid
             │             ▼             ▼
             │      ┌─────────────┐ ┌────────────┐
             │      │  Renewing   │ │  Expired   │
             │      └──────┬──────┘ └─────┬──────┘
             │             │              │
             │     success │              │ generateAccessToken
             │             │              │
             └─────────────┘──────────────┘
```

### Security Requirements

- All credentials wrapped in `Secret<String>` (secrecy crate)
- Token memory zeroized on drop (zeroize crate)
- NEVER log token values — only log token_expiry_time, token_age_hours
- NEVER store tokens in Valkey, QuestDB, or any persistent store
- NEVER transmit tokens in query parameters — always in Authorization header
- Token access failures halt the system — no silent degradation

---

## 9. Order Management API

### Place Order

```
POST {config.dhan.rest_api_base_url}/v2/orders
Authorization: <access_token>
Content-Type: application/json

{
    "dhanClientId": "<client_id>",
    "transactionType": "BUY",
    "exchangeSegment": "NSE_FNO",
    "productType": "INTRADAY",
    "orderType": "LIMIT",
    "validity": "DAY",
    "securityId": "52432",
    "quantity": 50,
    "price": 245.50,
    "triggerPrice": 0,
    "disclosedQuantity": 0,
    "afterMarketOrder": false,
    "correlationId": "<uuid-idempotency-key>"
}
```

**Response:**
```json
{
    "orderId": "1234567890",
    "orderStatus": "TRANSIT",
    "correlationId": "<echoed-back>"
}
```

### Modify Order

```
PUT {config.dhan.rest_api_base_url}/v2/orders/{orderId}
Authorization: <access_token>
Content-Type: application/json

{
    "dhanClientId": "<client_id>",
    "orderId": "1234567890",
    "orderType": "LIMIT",
    "quantity": 50,
    "price": 246.00,
    "triggerPrice": 0,
    "validity": "DAY",
    "disclosedQuantity": 0
}
```

### Cancel Order

```
DELETE {config.dhan.rest_api_base_url}/v2/orders/{orderId}
Authorization: <access_token>
```

### Get Order by ID

```
GET {config.dhan.rest_api_base_url}/v2/orders/{orderId}
Authorization: <access_token>
```

### Get All Orders (Today)

```
GET {config.dhan.rest_api_base_url}/v2/orders
Authorization: <access_token>
```

### Get Positions

```
GET {config.dhan.rest_api_base_url}/v2/positions
Authorization: <access_token>
```

### Order Response Fields

```json
{
    "orderId": "1234567890",
    "correlationId": "uuid-from-request",
    "orderStatus": "TRADED",
    "transactionType": "BUY",
    "exchangeSegment": "NSE_FNO",
    "productType": "INTRADAY",
    "orderType": "LIMIT",
    "validity": "DAY",
    "securityId": "52432",
    "quantity": 50,
    "price": 245.50,
    "triggerPrice": 0.0,
    "tradedQuantity": 50,
    "tradedPrice": 245.50,
    "remainingQuantity": 0,
    "filledQty": 50,
    "averageTradePrice": 245.50,
    "exchangeOrderId": "NSE-1234567",
    "exchangeTime": "2026-03-15T10:30:45",
    "createTime": "2026-03-15T10:30:44",
    "updateTime": "2026-03-15T10:30:45",
    "rejectionReason": "",
    "tag": ""
}
```

### Rate Limiting

- SEBI mandates: Maximum 10 orders per second
- Enforced using: governor crate (GCRA algorithm — Generic Cell Rate Algorithm)
- If rate limit hit at broker: HTTP 429 with error code DH-300
- Action: Back off, do NOT retry immediately — SEBI violation risk

---

## 10. Live Order Update WebSocket

### Connection Details

| Parameter | Value |
|-----------|-------|
| Endpoint | `wss://api-order-update.dhan.co` (from config) |
| Protocol | JSON (NOT binary — different from market feed) |
| Auth | Login message sent after connection (NOT URL params) |
| Purpose | Real-time order status updates pushed by Dhan |

### Authentication

After connecting, send a JSON login message:

```json
{
    "LoginReq": {
        "MsgCode": 42,
        "ClientId": "1000000001",
        "Token": "JWT_ACCESS_TOKEN"
    },
    "UserType": "SELF"
}
```

Upon successful login, Dhan automatically pushes all order updates for the authenticated account. No subscription message needed.

### Order Update Message Format

Messages arrive as JSON with a `"Data"` wrapper. Fields use **PascalCase** (unlike REST API which uses camelCase).

```json
{
    "Data": {
        "Exchange": "NSE",
        "Segment": "D",
        "SecurityId": "52432",
        "ClientId": "1000000001",
        "OrderNo": "1234567890",
        "ExchOrderNo": "NSE-1234567",
        "Product": "I",
        "TxnType": "B",
        "OrderType": "LMT",
        "Validity": "DAY",
        "Quantity": 50,
        "TradedQty": 50,
        "RemainingQuantity": 0,
        "Price": 245.50,
        "TriggerPrice": 0.0,
        "TradedPrice": 245.50,
        "AvgTradedPrice": 245.50,
        "Status": "TRADED",
        "Symbol": "NIFTY",
        "DisplayName": "NIFTY 27MAR 24500 CE",
        "CorrelationId": "uuid-123",
        "Remarks": "",
        "ReasonDescription": "CONFIRMED",
        "OrderDateTime": "2026-03-15 10:30:45",
        "ExchOrderTime": "2026-03-15 10:30:45",
        "LastUpdatedTime": "2026-03-15 10:30:46",
        "Instrument": "OPTIDX",
        "LotSize": 50,
        "StrikePrice": 24500.0,
        "ExpiryDate": "2026-03-27",
        "OptType": "CE",
        "Isin": "",
        "DiscQuantity": 0,
        "DiscQtyRem": 0,
        "LegNo": 0,
        "ProductName": "INTRADAY",
        "refLtp": 245.0,
        "tickSize": 0.05
    }
}
```

### OMS State Machine Transitions from WebSocket Updates

```
TRANSIT    → PENDING     (order reached exchange)
TRANSIT    → REJECTED    (exchange rejected immediately)
PENDING    → CONFIRMED   (exchange accepted, in order book)
CONFIRMED  → TRADED      (fully filled)
CONFIRMED  → CANCELLED   (user cancelled)
CONFIRMED  → EXPIRED     (end of validity)
PENDING    → TRADED      (immediate fill, skips CONFIRMED)
PENDING    → CANCELLED   (cancelled before confirmation)

Invalid transitions → log ERROR, trigger alert, reconciliation check
```

### Reconnection Strategy

- On disconnect: Exponential backoff (backon), same as market feed
- On reconnect: Fetch all today's orders via REST API to reconcile
- Any mismatch between REST state and last-known state → CRITICAL alert
- Order update WebSocket is CRITICAL — reconnect aggressively

---

## 11. O(1) Component Scoring

> Core O(1) principles in CLAUDE.md §Principle 6. This section provides Phase 1 scoring specifics.

### Scoring Matrix

| Component | Hot Path? | Target Latency | O(1) Technique |
|-----------|-----------|----------------|----------------|
| Binary tick parsing | YES | <10ns | zerocopy struct cast |
| Exchange segment decode | YES | <1ns | array index lookup (segment code → enum) |
| Security ID → instrument | YES | <5ns | nohash-hasher HashMap (identity hash) |
| Tick → candle aggregation | YES | <100ns | In-memory struct update, no alloc |
| Ring buffer enqueue | YES | <10ns | rtrb wait-free SPSC |
| Ring buffer dequeue | YES | <10ns | rtrb wait-free SPSC |
| Token read (for WebSocket) | YES | <5ns | arc-swap atomic load |
| Market depth update | YES | <50ns | Fixed-size array, no alloc |
| Option chain lookup | WARM | <1μs | papaya concurrent HashMap |
| QuestDB ILP write | COLD | <10μs | Batch flush, off hot path |
| REST API call | COLD | 5-50ms | reqwest async, off hot path |
| Config read | COLD | N/A | Read once at startup |

### Allocation Rules by Path

| Path | Heap Allocation | Clone | Dynamic Dispatch |
|------|----------------|-------|-----------------|
| Hot (tick processing) | ZERO | ZERO | ZERO (enum_dispatch) |
| Warm (lookups) | Bounded | By reference only | Allowed (bounded) |
| Cold (API, logging) | Allowed | Allowed | Allowed |

---

## 12. Crate Dependency Map

### Workspace Crate Structure

```
crates/
├── common/     → Shared types, constants, config, errors
├── core/       → WebSocket, binary parsing, tick pipeline, candle aggregation
├── trading/    → OMS, risk engine, order execution, strategy interface
├── storage/    → QuestDB writer, Valkey cache, persistence logic
├── api/        → axum HTTP server, REST endpoints, WebSocket proxy
└── app/        → Binary entry point, orchestration, boot sequence
```

### Dependency Direction (strict DAG — no cycles)

```
app → api → trading → core → storage → common
                  ↘              ↗
                    → common ←
```

**Rule:** Every crate depends on `common`. No crate depends on `app`. No circular dependencies.

### Crate Responsibilities

| Crate | Contains | Key Dependencies |
|-------|----------|-----------------|
| `common` | Types, enums, constants, config, errors | serde, chrono, thiserror |
| `core` | WebSocket client, binary parser, tick pipeline, candle builder | tokio, tokio-tungstenite, zerocopy, rtrb, crossbeam |
| `trading` | OMS state machine, risk engine, order API client, strategy trait | statig, governor, failsafe, blackscholes |
| `storage` | QuestDB ILP writer, Valkey cache, instrument persistence | questdb-rs, redis/deadpool-redis |
| `api` | HTTP server, REST endpoints, health/stats/instruments | axum, tower, tower-http |
| `app` | main(), boot sequence, signal handling, orchestration | clap, signal-hook, sd-notify |

---

## 13. Docker-First Stack

> Core Docker principles in CLAUDE.md §Principle 1. This section provides Phase 1 container specifics.

### Docker Compose Services

| Service | Image | Ports | Purpose |
|---------|-------|-------|---------|
| `dlt-questdb` | QuestDB (Bible version) | 9000, 8812, 9009 | Time-series DB (HTTP, PG wire, ILP) |
| `dlt-valkey` | Valkey (Bible version) | 6379 | Cache (session, state, rate limits) |
| `dlt-prometheus` | Prometheus (Bible version) | 9090 | Metrics collection |
| `dlt-grafana` | Grafana (Bible version) | 3000 | Dashboards + alerting |
| `dlt-loki` | Loki (Bible version) | 3100 | Log aggregation |
| `dlt-alloy` | Grafana Alloy (Bible version) | 4317 | Log/trace collector (replaces Promtail) |
| `dlt-jaeger` | Jaeger V2 (Bible version) | 16686, 4318 | Distributed tracing |
| `dlt-traefik` | Traefik (Bible version) | 80, 443, 8080 | Reverse proxy, TLS, blue-green |

### Health Check Strategy

Every container has a health check. The app waits for ALL infrastructure to be healthy before starting.

### Docker DNS

All inter-container communication uses Docker DNS hostnames (see CLAUDE.md §Principle 1). Config TOML contains these hostnames — never `localhost`.

---

## 14. Latency Budget

### End-to-End Target: <92 microseconds

From Dhan WebSocket binary frame received to QuestDB ILP row flushed.

### Breakdown

| Stage | Budget | Technique |
|-------|--------|-----------|
| WebSocket frame receive | ~0 (kernel) | tokio epoll/kqueue |
| Binary frame parse | <10ns | zerocopy, no alloc |
| Exchange segment decode | <1ns | Array index |
| Security ID lookup | <5ns | nohash-hasher identity hash |
| Tick validation | <5ns | Bounds check, no alloc |
| Ring buffer enqueue | <10ns | rtrb SPSC wait-free |
| Ring buffer dequeue | <10ns | rtrb SPSC wait-free |
| 1s candle aggregation | <100ns | In-memory update |
| Indicator update | <500ns | yata streaming |
| State update (papaya) | <50ns | Lock-free concurrent map |
| ILP buffer append | <1μs | questdb-rs buffer |
| ILP flush (batched) | <10μs | TCP write, batched |
| **TOTAL** | **<~92μs** | |

### Measurement

- **quanta** crate for nanosecond-precision timestamps on hot path (TSC-based, no syscall)
- **Criterion** benchmarks for every hot-path function
- **hdrhistogram** for percentile latency tracking (p50, p99, p99.9)
- **metrics** + **metrics-exporter-prometheus** for real-time dashboards

---

## 15. End-to-End Processing Pipeline

### 18 Stages — From Binary Frame to Dashboard

```
Stage 1:  WebSocket frame received (tokio-tungstenite)
Stage 2:  Frame validation (length check, min size)
Stage 3:  Header parse — extract response_code, segment, security_id (zerocopy)
Stage 4:  Response code dispatch (enum_dispatch — jump table, no dyn)
Stage 5:  Body parse — decode fields per response code (zerocopy)
Stage 6:  Price decode — direct f32 read from wire (no division needed in V2)
Stage 7:  Instrument lookup — security_id → InstrumentInfo (nohash HashMap)
Stage 8:  Tick struct assembly — TickData on stack (arrayvec, no heap)
Stage 9:  Dedup check — skip if duplicate (ring buffer hash check)
Stage 10: Ring buffer enqueue — SPSC to processing thread (rtrb)
Stage 11: Ring buffer dequeue — processing thread receives tick
Stage 12: 1-second candle update — in-memory OHLCV aggregation
Stage 13: Indicator update — yata streaming indicators (SMA, EMA, RSI, etc.)
Stage 14: State update — papaya concurrent map for latest prices
Stage 15: Top movers update — sorted by change_pct
Stage 16: QuestDB ILP append — buffer tick row
Stage 17: QuestDB ILP flush — batch flush every 1s or 1000 rows
Stage 18: Prometheus metrics update — latency histogram, tick counter
```

### Thread Model

```
Thread 1: WebSocket receiver (tokio async)
  → Stages 1-10 (parse + enqueue)
  → ZERO allocation, ZERO blocking

Thread 2: Tick processor (dedicated OS thread, core-pinned)
  → Stages 11-18 (process + store)
  → core_affinity pinned for cache locality
  → rtrb consumer: wait-free dequeue

Thread 3: Candle flusher (tokio async)
  → Periodic QuestDB ILP flush
  → Off hot path, allocation allowed

Thread 4: API server (tokio async, multi-threaded)
  → axum HTTP handlers
  → Read from papaya state maps
  → No interference with tick processing
```

---

## 16. Warmup & Historical Data

### What Warmup Means

When the system starts at 8:30 AM, indicators (SMA, EMA, RSI, etc.) need historical candle data to produce valid values. Without warmup, the first ~20 minutes of trading have unreliable indicator values.

### Warmup Strategy

```
8:30 AM — System starts, instruments loaded
8:31 AM — Fetch previous N days of 1-minute candles from Dhan REST API
           N depends on longest indicator period (e.g., 200-period SMA needs 200 candles)
           For daily data: last 200 trading days
           For intraday: last 5 trading days × 375 candles = 1,875 candles

8:35 AM — Historical candles loaded into QuestDB + in-memory indicator state
           All indicators are now "warm" with valid values

9:00 AM — Pre-market data collection begins via WebSocket
           Ticks feed into already-warm indicators

9:15 AM — Market opens. Indicators produce valid signals from the first candle.
```

### Dhan Historical Data API

```
GET {config.dhan.rest_api_base_url}/v2/charts/intraday
Authorization: <access_token>
Content-Type: application/json

{
    "securityId": "13",
    "exchangeSegment": "IDX_I",
    "instrument": "INDEX",
    "interval": "1",
    "fromDate": "2026-03-10",
    "toDate": "2026-03-15"
}
```

Response: Array of OHLCV candles with timestamps.

### What Data to Fetch

| Data Type | Instruments | Period | Purpose |
|-----------|------------|--------|---------|
| 1-min candles | All 215 F&O underlyings | Last 5 trading days | Intraday indicator warmup |
| Daily candles | All 215 F&O underlyings | Last 200 trading days | Daily indicator warmup |
| Previous close | All subscribed instruments | Previous trading day | Change % calculation |

---

## 17. Deliverables Checklist

### Block 01: Master Instrument Download — COMPLETE

- [x] CSV download with retry + fallback
- [x] 5-pass mapping algorithm
- [x] FnoUniverse object with all lookup maps
- [x] WebSocket subscription plan generation
- [x] Validation (must-exist checks, count bounds)
- [x] QuestDB persistence (Block 01.1)

### Block 02: Authentication & Token Management — COMPLETE

- [x] AWS SSM secret retrieval (client-id, client-secret, totp-secret)
- [x] TOTP code generation (totp-rs)
- [x] generateAccessToken REST call
- [x] arc-swap token storage with O(1) reads
- [x] Token renewal lifecycle (23h refresh, backoff, circuit breaker)
- [x] renewToken REST call with fallback to full auth
- [x] Token state machine (NoToken → Active → Renewing → Expired)
- [x] zeroize on token drop
- [x] Telegram alert on renewal failure

### Block 03: WebSocket Connection Manager — COMPLETE

- [x] WebSocket connection establishment (tokio-tungstenite)
- [x] Auth header injection from arc-swap token
- [x] Connection pool (up to 5 connections)
- [x] Subscription request builder (JSON, max 100 per message)
- [x] Keep-alive ping/pong (10s interval, 40s timeout)
- [x] Reconnection with exponential backoff
- [x] Disconnect code handling (803/804 → token refresh)
- [x] Connection health monitoring + metrics

### Block 04: Binary Protocol Parser — COMPLETE

- [x] 8-byte header parser
- [x] Response code dispatcher
- [x] Ticker packet parser (16 bytes)
- [x] Quote packet parser (50 bytes)
- [x] Full packet parser (162 bytes)
- [x] OI packet parser (12 bytes)
- [x] Previous close parser (16 bytes)
- [x] Market status parser (8 bytes)
- [x] Price decoding (direct f32 read)
- [x] 5-level market depth parser
- [x] 20-level and 200-level deep depth parsers
- [x] Shared read_helpers for zero-copy parsing

### Block 05: Tick Processing Pipeline — COMPLETE

- [x] mpsc channel — WebSocket → processor
- [x] Tick deduplication (O(1) ring buffer hash check)
- [x] 1-second candle aggregation (CandleAggregator — O(1) per tick)
- [x] QuestDB ILP tick writer (batched flush)
- [x] QuestDB ILP depth writer (batched flush)
- [x] NaN/Infinity guard on all price fields
- [x] Latency measurement (metrics histograms)

### Block 06: Candle Builder & Timeframes — PARTIAL

- [x] CandleAggregator: in-memory 1s OHLCV from live ticks
- [x] Historical 1-minute candle fetch + QuestDB persistence
- [x] Cross-verification of historical vs live candles
- [ ] QuestDB materialized views for higher timeframes
- [ ] IST alignment for all candle boundaries

### Block 07: Market Depth (20-Level & 200-Level) — COMPLETE

- [x] 20-level depth parser
- [x] 200-level depth parser
- [x] 5-level depth persistence to QuestDB
- [x] Depth book types (DeepDepthLevel, MarketDepthLevel)

### Block 08: Top Gainers/Losers — COMPLETE

- [x] Real-time change_pct calculation from ticks (TopMoversTracker)
- [x] In-memory sorted rankings (top 20 gainers, losers, most active)
- [x] Periodic snapshot computation (cold path, O(N log N))
- [ ] API endpoint for current rankings

### Block 09: Historical Data & Warmup — COMPLETE

- [x] Dhan historical candle API client
- [x] Automated fetch after market close
- [x] QuestDB backfill for historical 1m candles
- [x] Cross-verification audit

### Block 10: Order Management System

- [ ] OMS state machine
- [ ] Place/modify/cancel order API client
- [ ] Rate limiter (10 orders/sec)
- [ ] Circuit breaker for API failures
- [ ] Idempotency key generation + Valkey check
- [ ] Position tracking and reconciliation

### Block 11: Order Update WebSocket — COMPLETE

- [x] JSON WebSocket connection (separate from binary feed)
- [x] Order update message parser (parse_order_update)
- [x] Login message builder (build_order_update_login)
- [x] Broadcast channel for order updates
- [x] Exponential backoff reconnection
- [ ] OMS state transition from updates
- [ ] Reconciliation on reconnect

### Block 12: HTTP API Server — COMPLETE

- [x] axum server with tower middleware
- [x] Health check endpoint
- [x] Stats endpoint (QuestDB query)
- [x] Portal frontend (TradingView terminal)
- [x] Instrument rebuild endpoint
- [x] CORS middleware

### Block 13: Observability Stack — PARTIAL

- [x] Prometheus metrics (tick latency, throughput, error rates)
- [x] OpenTelemetry tracing layer
- [x] Telegram alerting pipeline
- [ ] Grafana dashboards
- [ ] Loki log aggregation

### Block 14: Risk Engine

- [ ] Max daily loss enforcement
- [ ] Position size limits
- [ ] P&L calculation
- [ ] Auto-halt on risk breach

### Block 15: Valkey Cache — COMPLETE

- [x] deadpool-redis async connection pool (ValkeyPool)
- [x] Typed helpers: get, set, set_ex, del, exists, set_nx_ex
- [x] Health check via PING
- [x] Integrated into boot sequence (best-effort step 6.5)

---

## 18. Final Summary

### System Characteristics

| Property | Value |
|----------|-------|
| Language | Rust 2024 Edition (1.93.1) |
| Runtime | Docker containers (Mac dev → AWS prod) |
| Tick parse latency | <10ns (zerocopy) |
| End-to-end latency | <92μs (tick → QuestDB) |
| Candle timeframes | 21 (1s through 1Y) |
| Max subscriptions | 25,000 instruments |
| Order rate limit | 10/second (SEBI) |
| Data retention | 90 days hot, 5 years cold |
| Availability | Auto-reconnect, circuit breaker, failover |
| Secrets | AWS SSM only, zero on disk |
| Monitoring | Prometheus + Grafana + Loki + Jaeger V2 |
| Alerting | Telegram → Grafana → SNS |

### Build Order

The boot sequence determines implementation order:

```
1. Block 01: Instrument Download        ✅ COMPLETE
2. Block 02: Authentication             ✅ COMPLETE
3. Block 03: WebSocket Connection       ✅ COMPLETE
4. Block 04: Binary Parser              ✅ COMPLETE
5. Block 05: Tick Pipeline              ✅ COMPLETE
6. Block 06: Candle Builder             🔶 PARTIAL (1s aggregation done, materialized views pending)
7. Block 07: Market Depth               ✅ COMPLETE
8. Block 08: Top Gainers/Losers         ✅ COMPLETE
9. Block 09: Historical Warmup          ✅ COMPLETE
10. Block 10: Order Management          ⬜ PENDING
11. Block 11: Order Update WebSocket    ✅ COMPLETE (OMS integration pending)
12. Block 12: HTTP API Server           ✅ COMPLETE
13. Block 13: Observability             🔶 PARTIAL (metrics + tracing done, dashboards pending)
14. Block 14: Risk Engine               ⬜ PENDING
15. Block 15: Valkey Cache              ✅ COMPLETE
```

Each block is self-contained with its own tests, benchmarks, and documentation. No block ships without passing ALL quality gates (fmt, clippy, test, benchmark, coverage).

### What Success Looks Like

Phase 1 is complete when:
1. System starts at 8:30 AM IST automatically
2. Instruments downloaded and built by 8:31 AM
3. WebSocket connected and subscribed by 8:35 AM
4. Pre-market ticks flowing at 9:00 AM
5. Live trading operational at 9:15 AM
6. All 21 candle timeframes updating in real-time
7. Orders placed/modified/cancelled with full audit trail
8. All monitoring dashboards showing live data
9. Zero manual intervention required for normal operation
10. System self-heals from all documented failure scenarios
