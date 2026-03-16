# Technical Flow — dhan-live-trader

> **Audience:** Developers, code reviewers, new contributors.
> **Think of this as:** "HOW does this system work?" — the engineering deep dive.

---

## Architecture at a Glance

```
6-crate Rust workspace:

  app        → Binary entry point. Orchestrates boot + shutdown.
  common     → Shared types, config, enums, constants. No business logic.
  core       → Runtime engine: auth, instruments, WebSocket, parser, pipeline.
  trading    → OMS, risk engine, strategies, indicators.
  storage    → QuestDB + Valkey persistence.
  api        → Axum HTTP server with REST endpoints.

Dependency rule: app → {api, trading, core, storage} → common
No circular dependencies. common is the foundation.
```

---

## Boot Sequence (15 Steps)

The system has **two boot paths** that share the same steps but differ in order and blocking behavior.

### Slow Boot (Normal Start — 2–5 seconds)

Used on first run, outside market hours, or non-trading days.

```
Step 0: rustls CryptoProvider
  │  Install TLS provider (required before ANY HTTPS/WSS call)
  │  aws-lc-rs backend. Fatal if fails.
  ▼
Step 1: Config Load
  │  Figment merges config/base.toml + config/local.toml
  │  Validates all subsystems (API URLs, QuestDB ports, etc.)
  │  Builds TradingCalendar (NSE holidays from CSV)
  ▼
Step 2: Prometheus Metrics
  │  Starts metrics exporter on :9090
  │  Registers: process stats, tick latency, candle writes, WS frames
  ▼
Step 3: Structured Logging
  │  tracing-subscriber with 3 layers:
  │    Console (text or JSON) + File (JSON for Alloy→Loki) + OpenTelemetry
  ▼
Step 4 (parallel):
  │  ┌─ Notification Init (Telegram bot from SSM)
  │  └─ Docker Infra Check (probe QuestDB:9000, auto-start if down)
  ▼
Step 5: IP Verification ← BLOCKS BOOT ON FAILURE
  │  Fetch public IP → compare with SSM static IP
  │  Mismatch = HALT + Telegram alert (Dhan rejects orders from wrong IP)
  ▼
Step 6: Authentication
  │  Try token cache (crash recovery) → if miss:
  │    SSM fetch TOTP secret → generate 6-digit code (RFC 6238)
  │    → POST auth.dhan.co/generateAccessToken → receive JWT
  │    → store in token_cache file + arc-swap handle
  │  One active token at a time. 24h validity.
  ▼
Step 7 (parallel): QuestDB DDL
  │  6 concurrent CREATE TABLE IF NOT EXISTS queries:
  │    ticks, depth_updates, prev_close_prices,
  │    instruments, instrument_derivatives,
  │    candles_1s, trading_calendar
  │  + 18 materialized views (candles_5s through candles_1Y)
  ▼
Step 8: Instrument Universe
  │  Try rkyv binary cache (<1ms) → if miss:
  │    Download detailed CSV (images.dhan.co, ~100K rows, no auth needed)
  │    → Parse → Build FnoUniverse (indices, stocks, derivatives)
  │    → SecurityId registry (HashMap<u32, Instrument>)
  │    → Validate derivative count >= 100
  │    → Serialize to rkyv cache for next boot
  │    → Persist snapshot to QuestDB
  ▼
Step 9: WebSocket Pool
  │  Build subscription plan (batch instruments into connections)
  │  For each batch:
  │    Connect WSS to api-feed.dhan.co (version=2, authType=2)
  │    Subscribe: RequestCode=21 (Full mode = quote + depth + OI)
  │    Max 100 instruments per JSON message, 5000 per connection
  │    Stagger: 10s during market hours, 1s off-hours
  ▼
Step 10: Tick Processor
  │  Spawns tokio task: WS frames → binary parse → broadcast
  │  Consumers: CandleAggregator, TopMovers, TradingPipeline, Persistence
  ▼
Step 11: Historical Candles (background)
  │  Fetch daily + intraday candles from Dhan REST API
  │  90-day chunking, 200ms sleep between calls
  │  Cross-verify against live data
  ▼
Step 12: Order Update WebSocket
  │  Connect WSS to api-order-update.dhan.co
  │  Auth: JSON message with MsgCode=42
  │  Receives ALL order status changes (filter by Source="P" for API orders)
  ▼
Step 13: API Server
  │  Axum on 0.0.0.0:8080
  │  Endpoints: /health, /api/stats, /api/quote, /api/top-movers, /portal
  ▼
Step 14: Token Renewal Task
  │  Background loop: check expiry every hour
  │  Renew 1 hour before expiry via arc-swap atomic swap
  ▼
Step 15: Await Shutdown
      tokio::signal::ctrl_c() → graceful shutdown sequence
```

### Fast Boot (Crash Recovery — ~400ms)

Used ONLY when: token cache exists + trading day + market hours (09:00–15:30 IST).

```
Config → Logging → IP Verify → Load Instruments (rkyv, <1ms)
  → WebSocket Connect (~400ms) ← ONLY BLOCKING STEP
  → Tick Processor (in-memory, no persistence writers yet)
  ─── TICKS FLOWING ───
  → Background: Docker + QuestDB DDL + SSM validation + everything else
```

Key difference: persistence is NOT on the critical path. QuestDB tables already exist from pre-crash run. Writers reconnect in background.

---

## Data Flow: Tick to Trade

### Hot Path (Microsecond Latency, Zero Allocation)

```
Dhan WebSocket (binary frame)
  │
  ▼
PacketHeader::parse()                        ← 8 bytes, zerocopy
  │  byte 0: response code (2/4/5/6/7/8/50)
  │  bytes 1-2: message length (i16 LE)
  │  byte 3: exchange segment (u8)
  │  bytes 4-7: security_id (i32 LE)
  ▼
Dispatcher (enum_dispatch, O(1) routing)
  │  code 2 → TickerPacket (16B)  → ltp, ltt
  │  code 4 → QuotePacket (50B)   → ltp, ltq, ltt, atp, vol, ohlc
  │  code 5 → OiPacket (12B)      → open_interest
  │  code 6 → PrevClose (16B)     → prev_close, prev_oi
  │  code 8 → FullPacket (162B)   → quote + 5-level depth
  │  code 50 → Disconnect (10B)   → reason code
  ▼
ParsedTick { tick_id, ts, security_id, ltp, volume, ... }
  │
  ├──→ CandleAggregator::update()            ← O(1), rolling OHLCV window
  │      On 1-second boundary → emit Candle1s
  │
  ├──→ TopMoversTracker::update()             ← O(1), sorted insert
  │      Maintains top-20 gainers/losers/most-active
  │
  ├──→ broadcast::Sender<ParsedTick>          ← Cold path consumers
  │      └──→ TradingPipeline (see below)
  │
  └──→ TickPersistenceWriter (if storage gate open)
         QuestDB ILP batch (flush on 1000 rows or 1s timer)
```

**Zero allocation guarantee:** Parser uses `zerocopy` crate. No heap allocations. No `Vec::push`. No `HashMap::insert`. Fixed-size structs only. Verified by `dhat` allocation profiling tests.

### Cold Path: Trading Pipeline

```
ParsedTick (broadcast receiver)
  │
  ▼
IndicatorEngine::update(security_id, ltp, volume, ts)
  │  For each registered indicator:
  │    EMA:  new_ema = α * price + (1-α) * prev_ema           ← O(1)
  │    RSI:  update avg_gain/avg_loss with Wilder's smoothing  ← O(1)
  │    MACD: update fast_ema, slow_ema, signal_ema             ← O(1)
  │    BB:   update rolling mean + std dev                     ← O(1)
  │    ATR:  update true range EMA                             ← O(1)
  │  All indicators: O(1) per tick. No window buffers.
  ▼
StrategyInstance::evaluate(indicators) → Signal
  │  Check conditions (typically 2-5 per strategy):
  │    "RSI < 30 AND price > EMA_200 AND MACD_hist > 0"
  │  Returns: Signal::Buy / Signal::Sell / Signal::Hold
  ▼
RiskEngine::check_order(signal) → RiskCheck
  │  Check 1: auto_halted? → Reject
  │  Check 2: abs(realized + unrealized P&L) >= max_daily_loss? → Halt + Reject
  │  Check 3: abs(current_lots + new_lots) > max_position_size? → Reject
  │  All O(1). No external calls.
  ▼
OrderRateLimiter::try_acquire() → allowed?
  │  GCRA algorithm: 10 orders/sec (SEBI mandate)
  │  DailyRequestTracker: 7,000/day cap
  ▼
OmsEngine::place_order(request)
  │  dry_run=true (default): simulate with PAPER-xxx ID
  │  dry_run=false: POST /v2/orders to Dhan REST API
  │    Headers: access-token, Content-Type: application/json
  │    securityId = STRING ("11536")
  │    correlationId = UUID for tracking
  ▼
Order Update WebSocket → OmsEngine::handle_update()
      State machine: Transit → Pending → Traded/Rejected/Cancelled
      Circuit breaker: 3 consecutive failures → open → block orders
      Reconciliation: compare local state with Dhan order book
```

---

## Data Persistence

### QuestDB (Time-Series Database)

```
ILP (InfluxDB Line Protocol) over TCP:9009

Tables:
  ticks               ← Live tick data (DEDUP on ts + security_id + segment)
  depth_updates       ← 5-level market depth snapshots
  prev_close_prices   ← Previous day close (arrives once per subscription)
  candles_1s          ← 1-second OHLCV candles (DEDUP on ts + security_id)
  instruments         ← Daily instrument master snapshot
  trading_calendar    ← NSE holidays and trading sessions

Materialized Views (QuestDB SAMPLE BY):
  candles_1s
    → candles_5s → candles_10s → candles_15s → candles_30s
    → candles_1m → candles_2m → candles_3m → candles_5m
    → candles_10m → candles_15m → candles_30m
    → candles_1h → candles_2h → candles_3h → candles_4h
    → candles_1d → candles_7d → candles_1M

  All aligned to IST: ALIGN TO CALENDAR WITH OFFSET '05:30'

Write path:
  ParsedTick → TickPersistenceWriter → batch 1000 rows → ILP TCP flush
  Candle1s   → CandlePersistenceWriter → same batching
  Depth      → DepthPersistenceWriter → same batching

f32→f64 precision: Custom converter prevents IEEE 754 widening artifacts
  21004.95_f32 → "21004.95" (string round-trip) → 21004.95_f64
```

### Valkey (Redis-Compatible Cache)

```
deadpool-redis async pool

Used for:
  - Token cache (encrypted, Secret<String>)
  - Instrument registry cache
  - Idempotency tracking (correlation_id → order_id)
  - General key-value cache

Typed helpers: get/set/del/exists/set_nx_ex + PING health check
```

---

## Authentication Flow

```
┌─────────────────────── TOKEN LIFECYCLE ───────────────────────┐
│                                                               │
│  Generate (once per 24h):                                     │
│    1. Fetch TOTP secret from AWS SSM                          │
│    2. Generate 6-digit TOTP (RFC 6238, 30s window)            │
│    3. POST auth.dhan.co/generateAccessToken                   │
│         params: dhanClientId, pin, totp                       │
│    4. Receive: JWT (eyJ...) + expiryTime (ISO format, IST)    │
│    5. Store: arc-swap handle + token_cache file                │
│                                                               │
│  Read (every API call, hot path):                             │
│    token_handle.load() → Arc<Option<TokenState>>              │
│    Zero allocation. No lock. No clone.                        │
│                                                               │
│  Renew (background, at 23h mark):                             │
│    GET /v2/RenewToken → extends by 24h                        │
│    arc-swap atomic swap → all readers see new token instantly  │
│                                                               │
│  Crash Recovery:                                              │
│    token_cache file → load → skip full auth → fast boot       │
│                                                               │
│  Error (DH-901):                                              │
│    Rotate token → retry ONCE → still fails → HALT + CRITICAL  │
│                                                               │
│  Storage:                                                     │
│    Secret<String> (secrecy crate) → zeroize on drop           │
│    NEVER logged, NEVER in DB, NEVER in URLs                   │
│                                                               │
└───────────────────────────────────────────────────────────────┘
```

---

## WebSocket Protocol Details

### Market Feed (Binary)

```
Endpoint: wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2
Limits:   5 connections × 5,000 instruments × 100 per subscribe message

Header (8 bytes, every packet):
  [0]    u8   Response Code (2=Ticker, 4=Quote, 5=OI, 6=PrevClose, 8=Full, 50=Disconnect)
  [1-2]  i16  Message Length (LE)
  [3]    u8   Exchange Segment (0=IDX, 1=NSE_EQ, 2=NSE_FNO, ...)
  [4-7]  i32  Security ID (LE)

All values Little Endian. Prices are f32 (NOT f64 — that's depth).
Ping/pong handled by WebSocket library (server pings every 10s).

Subscribe message (JSON):
  {
    "RequestCode": 21,              // Full mode
    "InstrumentCount": 3,
    "InstrumentList": [
      {"ExchangeSegment": "NSE_FNO", "SecurityId": "52432"}
    ]
  }
  Note: SecurityId is STRING in JSON, integer in binary header.
```

### Full Market Depth (Binary, Separate WebSocket)

```
20-level:  wss://depth-api-feed.dhan.co/twentydepth?...
200-level: wss://full-depth-api.dhan.co/twohundreddepth?...

Header: 12 bytes (NOT 8):
  [0-1]   i16  Message Length (LE)      ← DIFFERENT byte order from market feed
  [2]     u8   Response Code (41=Bid, 51=Ask)
  [3]     u8   Exchange Segment
  [4-7]   i32  Security ID (LE)
  [8-11]  u32  Sequence (20-lvl) or Row Count (200-lvl)

Depth prices: f64 (8 bytes) — DIFFERENT from market feed's f32
Each level: 16 bytes (f64 price + u32 qty + u32 orders)
Bid/Ask arrive as SEPARATE packets (not combined).
```

### Order Update (JSON, Not Binary)

```
Endpoint: wss://api-order-update.dhan.co
Auth message: { "LoginReq": { "MsgCode": 42, "ClientId": "...", "Token": "JWT" }, "UserType": "SELF" }

Messages: { "Data": { ... }, "Type": "order_alert" }
  Product: C=CNC, I=INTRADAY, M=MARGIN
  TxnType: B=Buy, S=Sell
  Source:  P=API (filter for this), N=Dhan web app
  Timestamps: IST strings "YYYY-MM-DD HH:MM:SS" (not epoch)
```

---

## Error Handling & Rate Limits

```
Trading API Errors:
  DH-901  Auth failed       → rotate token → retry ONCE → HALT
  DH-904  Rate limit        → backoff: 10s → 20s → 40s → 80s → CRITICAL
  DH-905  Bad input         → NEVER retry (fix the request)
  DH-906  Order error       → NEVER retry (fix the order)

Data API Errors:
  805     Too many conns    → STOP ALL connections 60s → audit → reconnect one-by-one
  807     Token expired     → trigger token refresh

Rate Limits (per dhanClientId):
  Orders:       10/sec, 250/min, 1000/hr, 7000/day
  Data APIs:    5/sec, 100K/day
  Quote REST:   1/sec
  Non-Trading:  20/sec
  Modifications: max 25 per order
```

---

## Shutdown Sequence

```
Ctrl+C received
  ▼
1. Abort token renewal task
2. Abort order update WebSocket
3. Abort all market feed WebSocket connections
     (dropping senders causes tick processor to exit)
4. Wait for tick processor graceful shutdown (5s timeout):
     - Drain in-flight QuestDB ILP batches
     - Flush remaining candles
5. Abort trading pipeline
6. Abort API server
7. Drop OpenTelemetry provider (flush traces)
8. Send Telegram "ShutdownComplete" notification

Second Ctrl+C during shutdown → immediate exit (code 1)
```

---

## Key Types & Where They Live

| Type | Crate | File | Purpose |
|------|-------|------|---------|
| `ApplicationConfig` | common | config.rs | All configuration (TOML) |
| `ParsedTick` | common | tick_types.rs | Parsed market data tick |
| `OrderUpdate` | common | order_types.rs | Order status change event |
| `FnoUniverse` | common | instrument_types.rs | All F&O instruments + lookup maps |
| `TokenManager` | core | auth/token_manager.rs | JWT lifecycle (generate/renew/cache) |
| `PacketHeader` | core | parser/header.rs | 8-byte WS binary header |
| `TickProcessor` | core | pipeline/tick_processor.rs | Main tick processing loop |
| `CandleAggregator` | core | pipeline/candle_aggregator.rs | 1-second candle builder |
| `ConnectionPool` | core | websocket/connection_pool.rs | WebSocket connection manager |
| `OmsEngine` | trading | oms/engine.rs | Order management system |
| `RiskEngine` | trading | risk/engine.rs | Pre-trade risk checks |
| `IndicatorEngine` | trading | indicator/engine.rs | Technical indicator computation |
| `StrategyInstance` | trading | strategy/evaluator.rs | Signal evaluation |
| `TickPersistenceWriter` | storage | tick_persistence.rs | QuestDB ILP batch writer |

---

## Configuration

```toml
# config/base.toml (committed)

[dhan]
api_base_url = "https://api.dhan.co/v2"
ws_url = "wss://api-feed.dhan.co"
client_id_ssm_path = "/dlt/prod/dhan/client-id"

[questdb]
http_host = "dlt-questdb"
http_port = 9000
pg_port = 8812
ilp_port = 9009

[trading]
dry_run = true                     # Paper trading by default
max_daily_loss = 5000.0
max_position_lots = 10
holidays_csv = "data/nse-holidays.csv"

[observability]
metrics_port = 9090
tracing_enabled = false

# config/local.toml (git-ignored, dev overrides)
[questdb]
http_host = "localhost"            # Override Docker DNS for cargo run
```

---

## Observability Stack

```
Prometheus (:9090)  → scrapes metrics → Grafana dashboards
Alloy               → watches data/logs/app.log → pushes to Loki
Loki                → log aggregation → Grafana log explorer
Jaeger V2           → receives OpenTelemetry traces → trace visualization
Traefik             → reverse proxy for all web UIs

All running in Docker alongside the application.
```
