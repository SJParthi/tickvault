# Diagram Flow — tickvault

> **Audience:** Visual learners. The "YouTube whiteboard" version.
> **How to read:** Top to bottom. Each diagram is self-contained.

---

## Diagram 1: The Big Picture

```
                    ┌─────────────────────────────────┐
                    │        INDIAN STOCK MARKET       │
                    │            (NSE)                 │
                    └────────────┬──────────────────────┘
                                 │
                    Live prices (binary WebSocket)
                                 │
                                 ▼
┌────────────────────────────────────────────────────────────────────┐
│                                                                    │
│                      tickvault                              │
│                                                                    │
│  ┌──────────┐    ┌──────────┐    ┌──────────┐    ┌──────────┐     │
│  │  WATCH   │───▶│ ANALYZE  │───▶│  DECIDE  │───▶│ EXECUTE  │     │
│  │          │    │          │    │          │    │          │     │
│  │ WebSocket│    │ Candles  │    │ Strategy │    │   OMS    │     │
│  │ Parser   │    │Indicators│    │Evaluator │    │  Orders  │     │
│  └──────────┘    └──────────┘    └──────────┘    └────┬─────┘     │
│       │                                               │           │
│       │          ┌──────────┐                         │           │
│       └─────────▶│  STORE   │◀────────────────────────┘           │
│                  │          │                                      │
│                  │ QuestDB  │    ┌──────────┐                     │
│                  │ Valkey   │    │ PROTECT  │ ← Risk Engine       │
│                  └──────────┘    │ Kill SW  │   checks every      │
│                                  │ Halt     │   order before      │
│                                  └──────────┘   execution         │
│                                                                    │
└────────────────────────────────────────────────────────────────────┘
                                 │
                    Orders (REST API)
                                 │
                                 ▼
                    ┌─────────────────────────────────┐
                    │         DHAN BROKER              │
                    │    (sends to NSE exchange)       │
                    └─────────────────────────────────┘
```

---

## Diagram 2: Boot Sequence (Slow Path)

```
START
  │
  ▼
┌─────────────────┐
│ 0. TLS Provider  │  rustls + aws-lc-rs
└────────┬────────┘
         ▼
┌─────────────────┐
│ 1. Load Config   │  base.toml + local.toml → ApplicationConfig
└────────┬────────┘
         ▼
┌─────────────────┐
│ 2. Prometheus    │  Metrics exporter on :9090
└────────┬────────┘
         ▼
┌─────────────────┐
│ 3. Logging       │  Console + File (JSON) + OpenTelemetry
└────────┬────────┘
         ▼
    ┌────┴────┐
    │ PARALLEL │
    ├─────────┤
    │ 4a.     │  Telegram bot init (from SSM)
    │ Notify  │
    ├─────────┤
    │ 4b.     │  Docker health check (QuestDB, Valkey)
    │ Infra   │  Auto-start if down
    └────┬────┘
         ▼
┌─────────────────┐
│ 5. IP Verify     │  Public IP == SSM static IP?
│   ⚠ BLOCKS BOOT │  Mismatch → HALT + Telegram alert
└────────┬────────┘
         ▼
┌─────────────────┐
│ 6. Authenticate  │  TOTP → JWT token (24h)
│                  │  Stored in arc-swap (O(1) reads)
└────────┬────────┘
         ▼
    ┌────┴────┐
    │ PARALLEL │
    │ 7.      │  CREATE TABLE IF NOT EXISTS × 6
    │ QuestDB │  + 18 materialized views
    │ DDL     │  All idempotent
    └────┬────┘
         ▼
┌─────────────────┐
│ 8. Instruments   │  rkyv cache (<1ms) or CSV download (~2s)
│                  │  Build FnoUniverse + subscription plan
└────────┬────────┘
         ▼
┌─────────────────┐
│ 9. WebSocket     │  Connect to api-feed.dhan.co (WSS)
│    Pool          │  Subscribe instruments (Full mode)
└────────┬────────┘
         ▼
┌─────────────────┐
│ 10. Tick         │  WS frames → parse → broadcast
│     Processor    │  + candle agg + top movers
└────────┬────────┘
         ▼
═══════════════════
  TICKS FLOWING
═══════════════════
         │
    ┌────┴────┐
    │BACKGROUND│  All non-blocking from here
    ├─────────┤
    │ 11. Historical candle fetch (backfill)
    │ 12. Order update WebSocket
    │ 13. API server (:8080)
    │ 14. Token renewal (every hour)
    └────┬────┘
         ▼
┌─────────────────┐
│ 15. Await        │  Ctrl+C → graceful shutdown
│     Shutdown     │
└─────────────────┘
```

---

## Diagram 3: Boot Sequence (Fast Path — Crash Recovery)

```
START (crash during market hours, token cache exists)
  │
  ▼
Config → Logging → IP Verify
  │
  ▼
┌──────────────────────────────────────────┐
│ Load instruments from rkyv cache (<1ms)  │
└────────────────┬─────────────────────────┘
                 ▼
┌──────────────────────────────────────────┐
│ WebSocket connect (~400ms)               │ ← ONLY blocking step
└────────────────┬─────────────────────────┘
                 ▼
┌──────────────────────────────────────────┐
│ Tick processor (IN-MEMORY, no QuestDB)   │
└────────────────┬─────────────────────────┘
                 │
    ═════════════╪═════════════
      TICKS FLOWING (~400ms)
    ═════════════╪═════════════
                 │
                 ▼
    ┌────────────────────────┐
    │ BACKGROUND (parallel): │
    │  • Docker infra check  │
    │  • QuestDB DDL         │
    │  • SSM validation      │
    │  • Token renewal       │
    │  • Historical fetch    │
    │  • API server          │
    └────────────────────────┘
```

---

## Diagram 4: Tick Processing Pipeline (Hot Path)

```
Dhan WebSocket
  │
  │  Binary frame (16–162 bytes)
  ▼
┌─────────────────────────────────────────────────────┐
│  PACKET HEADER (8 bytes)                             │
│  ┌────┬────────┬─────────┬──────────────┐           │
│  │ u8 │ i16 LE │  u8     │   i32 LE     │           │
│  │code│msg_len │segment  │ security_id  │           │
│  └──┬─┴────────┴─────────┴──────────────┘           │
│     │                                                │
│     ├─ code=2  → Ticker  (16B)  → ltp, ltt          │
│     ├─ code=4  → Quote   (50B)  → ltp, vol, ohlc    │
│     ├─ code=5  → OI      (12B)  → open_interest     │
│     ├─ code=6  → PrevCls (16B)  → prev_close        │
│     ├─ code=8  → Full   (162B)  → quote + 5L depth  │
│     └─ code=50 → Disconnect     → reason code       │
│                                                      │
│  ALL LITTLE ENDIAN. ALL ZERO-COPY. ALL O(1).         │
└──────────────────────┬──────────────────────────────┘
                       │
                       ▼
              ┌─── ParsedTick ───┐
              │  tick_id         │
              │  timestamp       │
              │  security_id     │
              │  ltp (f32)       │
              │  volume (i32)    │
              │  ...             │
              └───────┬──────────┘
                      │
         ┌────────────┼────────────┬──────────────┐
         ▼            ▼            ▼              ▼
    ┌─────────┐ ┌──────────┐ ┌─────────┐  ┌──────────┐
    │ Candle  │ │  Top     │ │Trading  │  │  Tick    │
    │Aggregat.│ │ Movers   │ │Pipeline │  │Persister │
    │         │ │          │ │         │  │          │
    │ 1s OHLCV│ │ Gainers  │ │Indicator│  │ QuestDB  │
    │ builder │ │ Losers   │ │→Strategy│  │ ILP TCP  │
    │         │ │ Active   │ │→Risk    │  │ batch    │
    └────┬────┘ └──────────┘ │→OMS     │  └──────────┘
         │                   └─────────┘
         ▼
    ┌─────────┐
    │ QuestDB │
    │candles_1s│
    │         │
    │    ↓    │  Materialized views:
    │  5s,10s │  candles_5s ... candles_1Y
    │  15s,30s│  (18 views total)
    │  1m...1Y│
    └─────────┘
```

---

## Diagram 5: Trading Pipeline (Cold Path)

```
ParsedTick (from broadcast channel)
  │
  ▼
┌─────────────────────────────────────────────────────┐
│  INDICATOR ENGINE                                    │
│                                                      │
│  For each security_id:                               │
│    EMA:   α × price + (1-α) × prev     → trend      │
│    RSI:   avg_gain / avg_loss           → momentum   │
│    MACD:  fast_ema - slow_ema           → crossover  │
│    BB:    mean ± 2σ                     → range      │
│    ATR:   true range smoothed           → volatility │
│                                                      │
│  ALL O(1) per tick. No buffers. No allocations.      │
└──────────────────────┬──────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────┐
│  STRATEGY EVALUATOR                                  │
│                                                      │
│  Check conditions (from TOML config):                │
│    IF rsi < 30 AND price > ema_200 → BUY             │
│    IF rsi > 70 AND price < ema_20  → SELL            │
│    ELSE                            → HOLD            │
│                                                      │
│  O(C) where C = number of conditions (typically 2-5) │
│  Hot-reloadable: change TOML → picks up automatically│
└──────────────────────┬──────────────────────────────┘
                       │
              Signal::Buy / Sell / Hold
                       │
                       ▼
┌─────────────────────────────────────────────────────┐
│  RISK ENGINE                                         │
│                                                      │
│  ┌─ Check 1: Auto-halted? ─────────── REJECT ──┐   │
│  │                                              │   │
│  ├─ Check 2: Daily loss limit hit? ─── HALT ────┤   │
│  │           |P&L| >= max_daily_loss             │   │
│  │                                              │   │
│  ├─ Check 3: Position too large? ──── REJECT ───┤   │
│  │           |lots| > max_position               │   │
│  │                                              │   │
│  └─ All passed? ──────────────────── APPROVE ───┘   │
│                                                      │
│  ALL O(1). No DB calls. No network.                  │
└──────────────────────┬──────────────────────────────┘
                       │
                  APPROVE / REJECT
                       │
                       ▼
┌─────────────────────────────────────────────────────┐
│  ORDER MANAGEMENT SYSTEM (OMS)                       │
│                                                      │
│  Rate Limiter (GCRA):   10/sec, 7000/day             │
│  Circuit Breaker:       3 failures → open → block    │
│  Idempotency:           correlation_id → UUID v4     │
│                                                      │
│  dry_run=true:  PAPER-001, PAPER-002, ...            │
│  dry_run=false: POST /v2/orders → Dhan API           │
│                                                      │
│  State Machine:                                      │
│    New → Transit → Pending → Traded                  │
│                           → Rejected                 │
│                           → Cancelled                │
│                  → PartTraded → Traded               │
└─────────────────────────────────────────────────────┘
```

---

## Diagram 6: Crate Dependency Graph

```
                    ┌───────┐
                    │  app  │  Binary entry point
                    └───┬───┘
                        │
           ┌────────────┼────────────┐
           ▼            ▼            ▼
      ┌─────────┐ ┌─────────┐ ┌─────────┐
      │   api   │ │ trading │ │  core   │
      │         │ │         │ │         │
      │  Axum   │ │ OMS     │ │ Auth    │
      │ HTTP    │ │ Risk    │ │ WS      │
      │ server  │ │ Strategy│ │ Parser  │
      │         │ │ Indicat.│ │ Pipeline│
      └────┬────┘ └────┬────┘ └────┬────┘
           │            │          │
           │       ┌────┘     ┌────┘
           │       │          │
           ▼       ▼          ▼
      ┌─────────┐       ┌─────────┐
      │ storage │       │         │
      │         │       │         │
      │ QuestDB │       │         │
      │ Valkey  │       │         │
      └────┬────┘       │         │
           │            │         │
           └────────┬───┘─────────┘
                    ▼
              ┌──────────┐
              │  common  │  Types, config, constants
              └──────────┘

No circular dependencies. common is the foundation.
```

---

## Diagram 7: Authentication & Token Lifecycle

```
┌──── FIRST RUN ─────────────────────────────────────────────┐
│                                                            │
│  AWS SSM                                                   │
│  /tickvault/prod/dhan/totp-secret ──▶ TOTP secret               │
│  /tickvault/prod/dhan/client-id   ──▶ Client ID                 │
│  /tickvault/prod/dhan/pin         ──▶ PIN                       │
│                                    │                       │
│                                    ▼                       │
│                            ┌───────────────┐               │
│                            │ TOTP Generate │               │
│                            │ (RFC 6238)    │               │
│                            │ 6 digits,     │               │
│                            │ 30s window    │               │
│                            └──────┬────────┘               │
│                                   │                        │
│                                   ▼                        │
│                  POST auth.dhan.co/generateAccessToken      │
│                  ?dhanClientId=X&pin=Y&totp=Z               │
│                                   │                        │
│                                   ▼                        │
│                            ┌───────────────┐               │
│                            │   JWT Token   │               │
│                            │  (24h valid)  │               │
│                            └──────┬────────┘               │
│                                   │                        │
│                      ┌────────────┼────────────┐           │
│                      ▼            ▼            ▼           │
│               arc-swap      token_cache    Secret<T>       │
│              (O(1) read)    (file, for    (zeroize on      │
│                              crash recovery) drop)         │
│                                                            │
└────────────────────────────────────────────────────────────┘

┌──── RENEWAL (hourly check) ────────────────────────────────┐
│                                                            │
│  Is token expiring within 1 hour?                          │
│     NO  → sleep until next check                           │
│     YES → GET /v2/RenewToken                               │
│              ↓                                              │
│           New JWT (24h extended)                            │
│              ↓                                              │
│           arc-swap atomic swap                              │
│           (all readers see new token instantly)             │
│                                                            │
└────────────────────────────────────────────────────────────┘

┌──── CRASH RECOVERY ───────────────────────────────────────┐
│                                                            │
│  token_cache file exists?                                  │
│     NO  → full auth (slow boot)                            │
│     YES → load token → validate → fast boot (~400ms)       │
│                                                            │
└────────────────────────────────────────────────────────────┘
```

---

## Diagram 8: Data Storage Architecture

```
┌─── Live Ticks ─────────────────────────────────────────────┐
│                                                            │
│  ParsedTick ──▶ TickPersistenceWriter                      │
│                   │                                        │
│                   │ Batch 1000 rows or 1s timer             │
│                   ▼                                        │
│                 QuestDB (ILP TCP :9009)                     │
│                   │                                        │
│                   ▼                                        │
│  ┌─────────────────────────────────────────────────────┐   │
│  │  TABLE: ticks                                       │   │
│  │  DEDUP UPSERT KEYS(ts, security_id, segment)        │   │
│  │  PARTITION BY HOUR                                  │   │
│  │                                                     │   │
│  │  Columns: ts, security_id, segment, ltp, volume,    │   │
│  │           atp, oi, day_open, day_high, day_low,     │   │
│  │           day_close, total_buy_qty, total_sell_qty   │   │
│  └─────────────────────────────────────────────────────┘   │
│                                                            │
└────────────────────────────────────────────────────────────┘

┌─── Candle Cascade ─────────────────────────────────────────┐
│                                                            │
│  CandleAggregator ──▶ candles_1s (base table)              │
│                          │                                  │
│         ┌────────────────┼────────────────┐                │
│         ▼                ▼                ▼                │
│     SAMPLE BY 5s    SAMPLE BY 1m    SAMPLE BY 1h           │
│     candles_5s      candles_1m      candles_1h             │
│         │               │               │                  │
│         ▼               ▼               ▼                  │
│     candles_10s     candles_2m      candles_2h             │
│     candles_15s     candles_3m      candles_3h             │
│     candles_30s     candles_5m      candles_4h             │
│                     candles_10m         │                   │
│                     candles_15m         ▼                   │
│                     candles_30m     candles_1d              │
│                                    candles_7d              │
│                                    candles_1M              │
│                                                            │
│  21 timeframes total. All aligned to IST (+05:30).         │
│  QuestDB materialized views handle all aggregation.        │
│                                                            │
└────────────────────────────────────────────────────────────┘
```

---

## Diagram 9: Risk Protection Layers

```
                Order Signal
                    │
                    ▼
        ┌───────────────────────┐
Layer 1 │   AUTO-HALT CHECK     │  Is system halted?
        │   halted? → REJECT    │  (previous loss triggered halt)
        └───────────┬───────────┘
                    │ not halted
                    ▼
        ┌───────────────────────┐
Layer 2 │   DAILY LOSS CHECK    │  |realized + unrealized P&L|
        │   >= max_loss?        │  >= config.max_daily_loss?
        │   → HALT + REJECT    │  (e.g., 5000 INR)
        └───────────┬───────────┘
                    │ within limit
                    ▼
        ┌───────────────────────┐
Layer 3 │   POSITION SIZE       │  |current + new lots|
        │   > max_lots?         │  > config.max_position_lots?
        │   → REJECT           │  (e.g., 10 lots)
        └───────────┬───────────┘
                    │ within limit
                    ▼
        ┌───────────────────────┐
Layer 4 │   RATE LIMITER        │  GCRA: 10 orders/sec
        │   (SEBI mandate)      │  5,000 orders/day
        │   exceeded? → REJECT  │
        └───────────┬───────────┘
                    │ allowed
                    ▼
        ┌───────────────────────┐
Layer 5 │   CIRCUIT BREAKER     │  3 consecutive failures?
        │   open? → REJECT      │  → open → block all orders
        │   half-open? → 1 try  │  → half-open → allow 1 probe
        └───────────┬───────────┘
                    │ closed
                    ▼
               ORDER PLACED
                    │
                    ▼
        ┌───────────────────────┐
Layer 6 │   LIVE MONITORING     │  Tick gap > 30s? → ALERT
        │   (background)        │  Tick gap > 60s? → TELEGRAM
        │                       │  System error?   → KILL SWITCH
        └───────────────────────┘
```

---

## Diagram 10: Shutdown Sequence

```
           Ctrl+C (SIGINT)
               │
               ▼
  ┌────────────────────────┐
  │ "Shutdown signal        │
  │  received"              │
  │ Telegram: Shutdown      │
  │  Initiated              │
  └───────────┬────────────┘
              │
    ┌─────────┼─────────────┐
    │         │             │
    ▼         ▼             ▼
 Abort     Abort         Abort
 Token     Order         All WS
 Renewal   Update WS     Connections
    │         │             │
    └─────────┼─────────────┘
              │
              ▼
  ┌────────────────────────┐
  │ Wait for tick processor │
  │ (5s timeout)            │
  │  • Drain ILP batches    │
  │  • Flush candles        │
  └───────────┬────────────┘
              │
    ┌─────────┼─────────────┐
    ▼         ▼             ▼
 Abort     Abort         Drop
 Trading   API           OpenTelemetry
 Pipeline  Server        (flush traces)
    │         │             │
    └─────────┼─────────────┘
              │
              ▼
  ┌────────────────────────┐
  │ Telegram: Shutdown     │
  │  Complete              │
  └────────────────────────┘
              │
              ▼
            EXIT 0


  ┌─── FORCE EXIT ────────────┐
  │ 2nd Ctrl+C during         │
  │ shutdown → EXIT 1          │
  │ (immediate, no cleanup)    │
  └───────────────────────────┘
```

---

## Diagram 11: Docker Service Stack

```
┌──────────────────────────────────────────────────────────┐
│                    DOCKER COMPOSE                         │
│                                                          │
│  ┌──────────────┐    ┌──────────────┐                    │
│  │  tickvault     │    │  QuestDB     │                    │
│  │              │───▶│  :9000 HTTP  │                    │
│  │  :8080 API   │    │  :8812 PG    │                    │
│  │  :9090 Prom  │    │  :9009 ILP   │                    │
│  └──────────────┘    └──────────────┘                    │
│         │                                                │
│         │            ┌──────────────┐                    │
│         └───────────▶│   Valkey     │                    │
│                      │  :6379       │                    │
│                      └──────────────┘                    │
│                                                          │
│  ┌──────────────┐    ┌──────────────┐    ┌────────────┐  │
│  │  Prometheus  │───▶│   Grafana    │◀───│   Loki     │  │
│  │  :9091       │    │  :3000       │    │  :3100     │  │
│  └──────────────┘    └──────────────┘    └────────────┘  │
│                           ▲                    ▲         │
│                           │              ┌─────┘         │
│  ┌──────────────┐    ┌────┴─────────┐    │               │
│  │  Jaeger V2   │    │   Traefik    │  ┌─┴──────────┐   │
│  │  :16686      │    │  :80/:443    │  │   Alloy    │   │
│  │  :4317 OTLP  │    │  Rev. Proxy  │  │  Log ship  │   │
│  └──────────────┘    └──────────────┘  └────────────┘   │
│                                                          │
│  Infrastructure: AWS c7i.2xlarge, Mumbai (ap-south-1)    │
│                                                          │
└──────────────────────────────────────────────────────────┘
```

---

## Diagram 12: File Map (Key Files Only)

```
tickvault/
│
├── crates/
│   ├── app/src/
│   │   ├── main.rs              ← ENTRY POINT (boot + shutdown)
│   │   ├── infra.rs             ← Docker health + auto-start
│   │   ├── observability.rs     ← Prometheus + OpenTelemetry
│   │   └── trading_pipeline.rs  ← Tick → Indicator → Strategy → OMS
│   │
│   ├── common/src/
│   │   ├── config.rs            ← ApplicationConfig (TOML)
│   │   ├── constants.rs         ← All named constants
│   │   ├── tick_types.rs        ← ParsedTick struct
│   │   ├── order_types.rs       ← OrderStatus, OrderType enums
│   │   └── instrument_types.rs  ← FnoUniverse, InstrumentRecord
│   │
│   ├── core/src/
│   │   ├── auth/                ← Token manager, TOTP, SSM
│   │   ├── instrument/          ← CSV download, universe builder
│   │   ├── websocket/           ← Connection pool, subscriptions
│   │   ├── parser/              ← Binary packet parsing (zerocopy)
│   │   ├── pipeline/            ← Tick processor, candle aggregator
│   │   ├── historical/          ← Candle fetcher, cross-verify
│   │   ├── notification/        ← Telegram alerts
│   │   ├── network/             ← IP verification
│   │   └── scheduler/           ← Time-based task scheduling
│   │
│   ├── trading/src/
│   │   ├── oms/                 ← Order engine, rate limiter, circuit breaker
│   │   ├── risk/                ← Risk engine, P&L tracking, tick gaps
│   │   ├── indicator/           ← EMA, RSI, MACD, BB, ATR, ...
│   │   └── strategy/            ← Signal evaluator, hot reload
│   │
│   ├── storage/src/
│   │   ├── tick_persistence.rs  ← QuestDB ILP writer
│   │   ├── candle_persistence.rs← 1s candle writer
│   │   └── materialized_views.rs← 18 QuestDB views DDL
│   │
│   └── api/src/
│       ├── lib.rs               ← Router builder
│       └── handlers/            ← HTTP endpoint handlers
│
├── config/
│   ├── base.toml                ← Production config (committed)
│   └── local.toml               ← Dev overrides (git-ignored)
│
└── docs/
    ├── flow-functional.md       ← "What does it do?" (this series)
    ├── flow-technical.md        ← "How does it work?"
    ├── flow-diagrams.md         ← "Show me pictures" (YOU ARE HERE)
    ├── dhan-ref/                ← Dhan API ground truth (16 files)
    └── phases/                  ← Implementation roadmap
```
