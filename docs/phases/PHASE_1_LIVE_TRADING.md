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
| 5 | BSE_FNO |
| 7 | MCX_COMM |

### Response Code 2 — Ticker Packet (25 bytes total)

```
Bytes 0-7:   Header (response_code=2, length, segment, security_id)
Bytes 8-11:  LTP (f32, Little Endian)  — divide by 100 for actual price
Bytes 12-15: Reserved
Bytes 16-17: Open (i16, Little Endian)  — offset from LTP
Bytes 18-19: High (i16, Little Endian)  — offset from LTP
Bytes 20-21: Low (i16, Little Endian)   — offset from LTP
Bytes 22-23: Close (i16, Little Endian) — offset from LTP (prev close)
Byte 24:     Reserved
```

**Price decoding for Ticker:**
- LTP: Read f32 at offset 8, divide by 100
- Open: LTP + (read i16 at offset 16) / 100
- High: LTP + (read i16 at offset 18) / 100
- Low: LTP + (read i16 at offset 20) / 100
- Close (prev): LTP + (read i16 at offset 22) / 100

### Response Code 4 — Quote Packet (51 bytes total)

```
Bytes 0-7:   Header (response_code=4)
Bytes 8-11:  LTP (f32 LE)
Bytes 12-15: Reserved
Bytes 16-17: Open offset (i16)
Bytes 18-19: High offset (i16)
Bytes 20-21: Low offset (i16)
Bytes 22-23: Close offset (i16)
Byte 24:     Reserved
Bytes 25-28: Volume (i32 LE)
Bytes 29-32: Reserved
Bytes 33-36: Avg Trade Price (f32 LE)
Bytes 37-40: Reserved (Total Buy Qty placeholder)
Bytes 41-44: Reserved (Total Sell Qty placeholder)
Bytes 45-48: Reserved
Bytes 49-50: Reserved
```

### Response Code 8 — Full Packet (162 bytes total)

```
Bytes 0-7:   Header (response_code=8)
Bytes 8-24:  Ticker fields (same layout as Response Code 2)
Byte 24:     Reserved
Bytes 25-28: Volume (i32 LE)
Bytes 29-32: OI (Open Interest) (i32 LE)
Bytes 33-36: Avg Trade Price (f32 LE)
Bytes 37-40: Reserved
Bytes 41-44: Reserved
Bytes 45-48: Reserved
Bytes 49-50: Last Traded Quantity (i16 LE)
Bytes 51-54: Last Trade Time (i32 LE — Unix epoch seconds)
Bytes 55-58: Reserved

-- 5-Level Market Depth (Bytes 59-161) --
Each level = 20 bytes:
  Bytes 0-3:   Bid Quantity (i32 LE)
  Bytes 4-5:   Bid Orders (i16 LE)
  Bytes 6-9:   Bid Price (f32 LE — divide by 100)
  Bytes 10-13: Ask Price (f32 LE — divide by 100)
  Bytes 14-17: Ask Quantity (i32 LE)
  Bytes 18-19: Ask Orders (i16 LE)

5 levels × 20 bytes = 100 bytes (offsets 59-158)
Bytes 159-161: Reserved/padding (3 bytes)
```

### Response Code 1 — Index Ticker (25 bytes)

Same structure as Response Code 2 (Ticker), but specifically for IDX_I segment instruments. Same field layout, same decoding rules.

### Response Code 5 — OI Data (17 bytes total)

```
Bytes 0-7:   Header (response_code=5)
Bytes 8-11:  Open Interest (i32 LE)
Bytes 12-15: OI Day High (i32 LE)
Bytes 16:    OI Day Low or padding
```

### Response Code 6 — Previous Close (20 bytes total)

```
Bytes 0-7:   Header (response_code=6)
Bytes 8-11:  Previous Close Price (f32 LE — divide by 100)
Bytes 12-15: Previous OI (i32 LE)
Bytes 16-19: Previous Volume (i32 LE)
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
| Auth | Same as live feed (access-token + client-id headers) |
| Max connections | 5 |
| Max instruments per connection | 5,000 |
| Protocol | Binary, Little Endian |

**Subscription request:** Same JSON format as live feed but with `RequestCode: 21`.

**Response packet structure:**

20-Level Depth Header:
```
Bytes 0-1:   Message Length (i16 LE) — total packet size
Byte 2:      Exchange Segment code (u8)
Bytes 3-6:   Security ID (i32 LE)
```

Note: The 20-level depth has a DIFFERENT header layout than the live feed — only 7 bytes, no response_code byte.

20-Level Depth Body:
```
Each level = 20 bytes (same as 5-level depth format):
  Bid Quantity (i32 LE)
  Bid Orders (i16 LE)
  Bid Price (f32 LE — divide by 100)
  Ask Price (f32 LE — divide by 100)
  Ask Quantity (i32 LE)
  Ask Orders (i16 LE)

20 levels × 20 bytes = 400 bytes
Total packet: 7 (header) + 400 (body) = 407 bytes
```

### 200-Level Depth WebSocket

| Parameter | Value |
|-----------|-------|
| Endpoint | `wss://full-depth-api.dhan.co/twohundreddepth` (from config) |
| Auth | Same as live feed |
| Max connections | 5 |
| Max instruments per connection | 5,000 |

**Response packet structure:**

200-Level Depth Header:
```
Bytes 0-1:   Message Length (i16 LE)
Byte 2:      Exchange Segment code (u8)
Bytes 3-6:   Security ID (i32 LE)
Bytes 7-10:  Last Traded Price (f32 LE — divide by 100)
Bytes 11-14: Last Traded Time (i32 LE — Unix epoch)
Bytes 15-18: Last Traded Quantity (i32 LE)
Bytes 19:    Number of Bid Levels (u8)
Bytes 20:    Number of Ask Levels (u8)
```

200-Level Bid/Ask entries (variable count):
```
Each bid entry = 12 bytes:
  Bid Price (f32 LE — divide by 100)
  Bid Quantity (i32 LE)
  Bid Orders (i32 LE)

Each ask entry = 12 bytes:
  Ask Price (f32 LE — divide by 100)
  Ask Quantity (i32 LE)
  Ask Orders (i32 LE)

Max: 200 bid levels + 200 ask levels
Max packet size: 21 + (200 × 12) + (200 × 12) = 4,821 bytes
```

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
| Auth | Same headers (access-token + client-id) |
| Purpose | Real-time order status updates pushed by Dhan |

### Subscription

No subscription message needed. Upon connection with valid auth, Dhan automatically pushes all order updates for the authenticated account.

### Order Update Message Format

```json
{
    "type": "order_update",
    "data": {
        "orderId": "1234567890",
        "orderStatus": "TRADED",
        "exchangeSegment": "NSE_FNO",
        "transactionType": "BUY",
        "productType": "INTRADAY",
        "securityId": "52432",
        "quantity": 50,
        "price": 245.50,
        "tradedQuantity": 50,
        "tradedPrice": 245.50,
        "averageTradePrice": 245.50,
        "remainingQuantity": 0,
        "exchangeOrderId": "NSE-1234567",
        "exchangeTime": "2026-03-15T10:30:45",
        "updateTime": "2026-03-15T10:30:45",
        "rejectionReason": ""
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
| `api` | HTTP server, REST endpoints, WebSocket proxy to frontend | axum, tower, tower-http |
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
Stage 6:  Price decode — divide by 100, apply offsets (f32/i16 math)
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

### Block 02: Authentication & Token Management

- [ ] AWS SSM secret retrieval (client-id, client-secret, totp-secret)
- [ ] TOTP code generation (totp-rs)
- [ ] generateAccessToken REST call
- [ ] arc-swap token storage with O(1) reads
- [ ] Token renewal lifecycle (23h refresh, backoff, circuit breaker)
- [ ] renewToken REST call with fallback to full auth
- [ ] Token state machine (NoToken → Active → Renewing → Expired)
- [ ] zeroize on token drop
- [ ] Telegram alert on renewal failure

### Block 03: WebSocket Connection Manager

- [ ] WebSocket connection establishment (tokio-tungstenite)
- [ ] Auth header injection from arc-swap token
- [ ] Connection pool (up to 5 connections)
- [ ] Subscription request builder (JSON, max 100 per message)
- [ ] Keep-alive ping/pong (10s interval, 40s timeout)
- [ ] Reconnection with exponential backoff (backon)
- [ ] Disconnect code handling (803/804 → token refresh)
- [ ] Connection health monitoring + metrics

### Block 04: Binary Protocol Parser

- [ ] 8-byte header parser (zerocopy)
- [ ] Response code dispatcher (enum_dispatch)
- [ ] Ticker packet parser (25 bytes)
- [ ] Quote packet parser (51 bytes)
- [ ] Full packet parser (162 bytes)
- [ ] OI packet parser (17 bytes)
- [ ] Previous close parser (20 bytes)
- [ ] Market status parser (10 bytes)
- [ ] Price decoding (f32/100 + i16 offsets)
- [ ] 5-level market depth parser
- [ ] Fuzz tests for malformed packets
- [ ] Benchmark: <10ns per packet parse

### Block 05: Tick Processing Pipeline

- [ ] SPSC ring buffer (rtrb) — WebSocket → processor
- [ ] Tick deduplication (ring buffer hash check)
- [ ] 1-second candle aggregation (in-memory OHLCV)
- [ ] Instrument lookup (nohash-hasher HashMap)
- [ ] State update (papaya concurrent map)
- [ ] QuestDB ILP tick writer (batched flush)
- [ ] Thread pinning (core_affinity)
- [ ] Latency measurement (quanta + hdrhistogram)

### Block 06: Candle Builder & Timeframes

- [ ] QuestDB materialized views for 19 timeframes
- [ ] Rust aggregation for 3M and 1Y
- [ ] IST alignment for all candle boundaries
- [ ] Pre/post market candle separation
- [ ] Candle event notification to downstream consumers

### Block 07: Market Depth (20-Level & 200-Level)

- [ ] 20-level depth WebSocket connection
- [ ] 200-level depth WebSocket connection
- [ ] Different header parsing (7-byte vs 21-byte)
- [ ] Depth book maintenance (in-memory, fixed-size arrays)
- [ ] QuestDB persistence for depth snapshots

### Block 08: Top Gainers/Losers

- [ ] Real-time change_pct calculation from ticks
- [ ] In-memory sorted rankings (top N)
- [ ] Periodic QuestDB flush (5-second snapshots)
- [ ] API endpoint for current rankings

### Block 09: Historical Data & Warmup

- [ ] Dhan historical candle API client
- [ ] Warmup data fetch at 8:31 AM
- [ ] Indicator pre-population from historical data
- [ ] QuestDB backfill for historical candles

### Block 10: Order Management System

- [ ] OMS state machine (statig)
- [ ] Place/modify/cancel order API client (reqwest)
- [ ] Rate limiter (governor GCRA — 10 orders/sec)
- [ ] Circuit breaker (failsafe) for API failures
- [ ] Retry with backoff (backon) for transient failures
- [ ] Idempotency key generation (uuid) + Valkey check
- [ ] Position tracking and reconciliation
- [ ] Order audit logging to QuestDB

### Block 11: Order Update WebSocket

- [ ] JSON WebSocket connection (separate from binary feed)
- [ ] Order update message parser
- [ ] OMS state transition from updates
- [ ] Reconciliation on reconnect (REST fetch + compare)
- [ ] CRITICAL alert on state mismatch

### Block 12: HTTP API Server

- [ ] axum server with tower middleware
- [ ] CORS, compression, tracing middleware
- [ ] REST endpoints: ticks, candles, option chains, orders, positions
- [ ] WebSocket proxy for live data to frontend
- [ ] Health check endpoint

### Block 13: Observability Stack

- [ ] Prometheus metrics (tick latency, throughput, error rates)
- [ ] Grafana dashboards (market data, system health, trading P&L)
- [ ] Loki log aggregation via Alloy
- [ ] Jaeger V2 distributed tracing
- [ ] Telegram alerting pipeline

### Block 14: Risk Engine

- [ ] Max daily loss enforcement
- [ ] Position size limits
- [ ] P&L calculation (real-time, using blackscholes for Greeks)
- [ ] Auto-halt on risk breach
- [ ] Risk metrics to Prometheus

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
1. Block 01: Instrument Download      ← COMPLETE
2. Block 02: Authentication           ← NEXT
3. Block 03: WebSocket Connection
4. Block 04: Binary Parser
5. Block 05: Tick Pipeline
6. Block 06: Candle Builder
7. Block 07: Market Depth
8. Block 08: Top Gainers/Losers
9. Block 09: Historical Warmup
10. Block 10: Order Management
11. Block 11: Order Update WebSocket
12. Block 12: HTTP API Server
13. Block 13: Observability
14. Block 14: Risk Engine
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
