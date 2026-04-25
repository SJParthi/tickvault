# PROJECT SCOPE — tickvault

> **Last updated:** 2026-03-15
> **Purpose:** Single-glance reference for what's built, what's pending, and how to approach new work.
> **Rule:** Update this file after every major implementation session.

---

## 1. Project Overview

| Property | Value |
|----------|-------|
| **Purpose** | O(1) latency live F&O trading system for Indian markets (NSE) |
| **Language** | Rust 2024 Edition (stable 1.95.0) |
| **Architecture** | 6-crate workspace, Docker-first, AWS c7i.2xlarge Mumbai (prod) |
| **Phase** | Phase 1 — Live Trading System |
| **Codebase** | ~84,000 lines of Rust across 147 files |
| **Tests** | 2,684 passing (0 failing), 57 ignored (loom), 5 benchmarks |
| **Boot sequence** | CryptoProvider → Config → Observability → Logging → Notification → Auth → QuestDB → Universe → HistoricalCandles → WebSocket → TickProcessor → OrderUpdateWS → API → TokenRenewal → Shutdown |

---

## 2. Crate Map

```
crates/
├── common/    → 14 source files, 1 test file      (shared types, config, enums, errors)
├── core/      → 44 source files, 12 test files     (WS, parser, pipeline, auth, instruments)
├── trading/   → 20 source files, 9 test files      (OMS, risk, strategy, indicators)
├── storage/   → 7 source files, 2 test files       (QuestDB, Valkey, persistence)
├── api/       → 9 source files, 3 test files       (axum HTTP, handlers, middleware)
└── app/       → 4 source files, 0 test files       (main binary, boot orchestration)
```

### Dependency DAG (strict, no cycles)
```
app → api → trading → core → storage → common
                  ↘              ↗
                    → common ←
```

---

## 3. What's BUILT (Blocks 01-15)

### Block 01: Master Instrument Download — COMPLETE
- **Files:** `core/src/instrument/` (9 files: csv_downloader, csv_parser, universe_builder, validation, subscription_planner, binary_cache, delta_detector, daily_scheduler, s3_backup, diagnostic, instrument_loader)
- **What it does:** Downloads Dhan detailed CSV daily, parses ~100K instruments, builds FnoUniverse with all lookup maps, generates WebSocket subscription plans, persists to QuestDB + Valkey
- **Key types:** `InstrumentRecord`, `FnoKey`, `FnoUniverse`, `InstrumentRegistry`
- **Tests:** Schema validation, proptest registry, category counts, edge cases

### Block 02: Authentication & Token Management — COMPLETE
- **Files:** `core/src/auth/` (6 files: token_manager, token_cache, totp_generator, secret_manager, types, mod)
- **What it does:** AWS SSM secrets → TOTP generation → JWT token → arc-swap O(1) reads → 23h auto-renewal → circuit breaker → Telegram alerts
- **Key types:** `TokenManager`, `TokenCache`, `TotpGenerator`, `SecretManager`, `GenerateTokenResponse`
- **Tests:** Token handle allocation (dhat), secret manager retry, token state machine

### Block 03: WebSocket Connection Manager — COMPLETE
- **Files:** `core/src/websocket/` (7 files: connection, connection_pool, subscription_builder, order_update_connection, tls, types, mod)
- **What it does:** Up to 5 market data WS connections, subscription batching (100/msg, 5000/conn), TLS via rustls, exponential backoff reconnect, disconnect code handling
- **Key types:** `WebSocketConnection`, `ConnectionPool`, `SubscriptionBuilder`
- **Tests:** WebSocket protocol e2e, subscription builder validation

### Block 04: Binary Protocol Parser — COMPLETE
- **Files:** `core/src/parser/` (13 files: header, ticker, quote, full_packet, oi, previous_close, market_status, disconnect, dispatcher, deep_depth, market_depth, order_update, read_helpers, types)
- **What it does:** Zero-copy binary parsing of all Dhan WS packet types (8-byte header, 16B ticker, 50B quote, 162B full, 12B OI, depth packets). All Little Endian. enum_dispatch for O(1) routing.
- **Key types:** `PacketHeader`, `TickerPacket`, `QuotePacket`, `FullPacket`, `DeepDepthLevel`
- **Tests:** Snapshot/golden tests, proptest fuzzing, deterministic replay, dhat allocation proofs, parser pipeline integration

### Block 05: Tick Processing Pipeline — COMPLETE
- **Files:** `core/src/pipeline/` (4 files: tick_processor, candle_aggregator, top_movers, mod)
- **What it does:** WS frame → parse → dedup → candle aggregate → QuestDB ILP write → metrics. Two-thread model: WS receiver (zero-alloc) + tick processor (core-pinned).
- **Key types:** `TickProcessor`, `CandleAggregator`, `TopMoversTracker`
- **Tests:** Backpressure, load/stress, graceful degradation, timeout enforcement

### Block 06: Candle Builder & Timeframes — COMPLETE
- **Files:** `core/src/pipeline/candle_aggregator.rs`, `core/src/historical/` (3 files), `storage/src/candle_persistence.rs`, `storage/src/materialized_views.rs`
- **What it does:** 1s candles from live ticks, 18 QuestDB materialized views (5s→1M), historical 1m candle fetch + backfill, cross-verification audit
- **Key types:** `CandleAggregator`, `CandleFetcher`, `CandlePersistence`, `MaterializedViews`

### Block 07: Market Depth (20-Level & 200-Level) — COMPLETE
- **Files:** `core/src/parser/deep_depth.rs`, `core/src/parser/market_depth.rs`
- **What it does:** Separate 12-byte header parsing, f64 prices (not f32), bid/ask as separate packets, 20-level (50 instruments/conn) + 200-level (1 instrument/conn)
- **Key types:** `DeepDepthLevel`, `MarketDepthLevel`, `DepthPacket`

### Block 08: Top Gainers/Losers — COMPLETE
- **Files:** `core/src/pipeline/top_movers.rs`, `api/src/handlers/top_movers.rs`
- **What it does:** Real-time change_pct from ticks, sorted top-20 gainers/losers/most-active, API endpoint
- **Key types:** `TopMoversTracker`, `TopMoversSnapshot`, `TopMoverEntry`

### Block 09: Historical Data & Warmup — COMPLETE
- **Files:** `core/src/historical/` (candle_fetcher, cross_verify, mod)
- **What it does:** Dhan REST API historical candle fetch, 90-day chunking, QuestDB backfill, cross-verification, idempotency guard
- **Key types:** `CandleFetcher`, `CrossVerifier`, `HistoricalDataResponse`

### Block 10: Order Management System — PARTIALLY COMPLETE
- **Files:** `trading/src/oms/` (8 files: api_client, circuit_breaker, engine, idempotency, rate_limiter, reconciliation, state_machine, types)
- **What's built:** OMS engine with state machine (statig), rate limiter (GCRA), circuit breaker (failsafe), idempotency (Valkey), reconciliation logic, order types
- **What's pending:**
  - [ ] Wire OMS engine to real Dhan REST API (place/modify/cancel)
  - [ ] OMS state transition from live Order Update WebSocket
  - [ ] Reconciliation on WS reconnect (fetch all today's orders via REST)
  - [ ] Integration test with mock Dhan API
- **Key types:** `OmsEngine`, `OrderStateMachine`, `RateLimiter`, `CircuitBreaker`, `IdempotencyGuard`
- **Tests:** State machine transitions, circuit breaker (loom), financial overflow, panic safety, safety layer, graceful degradation

### Block 11: Order Update WebSocket — MOSTLY COMPLETE
- **Files:** `core/src/websocket/order_update_connection.rs`, `core/src/parser/order_update.rs`
- **What's built:** JSON WS connection, login message builder, order update parser, broadcast channel
- **What's pending:**
  - [ ] OMS state transition from WS updates
  - [ ] Reconciliation on reconnect

### Block 12: HTTP API Server — COMPLETE
- **Files:** `api/src/` (handlers: health, instruments, quote, static_file, stats, top_movers + middleware, state, lib)
- **What it does:** axum server, health check, stats (QuestDB), portal frontend, instrument rebuild, CORS, bearer auth middleware
- **Tests:** API smoke, auth middleware, contract tests

### Block 13: Observability Stack — COMPLETE
- **Files:** `app/src/observability.rs`, `core/src/notification/` (events, service, mod)
- **What it does:** Prometheus metrics, OpenTelemetry tracing, Telegram alerts, Grafana dashboards, Loki log aggregation
- **Docker:** QuestDB, Valkey, Prometheus, Grafana, Loki, Alloy, Jaeger V2, Traefik

### Block 14: Risk Engine — COMPLETE
- **Files:** `trading/src/risk/` (engine, tick_gap_tracker, types, mod)
- **What it does:** Max daily loss enforcement, position size limits, P&L calculation (realized FIFO + unrealized), auto-halt on breach, daily reset
- **Key types:** `RiskEngine`, `RiskConfig`, `TickGapTracker`
- **Tests:** 49 risk tests (proptest, never-requirements, edge cases, reconciliation safety, regression)

### Block 15: Valkey Cache — COMPLETE
- **Files:** `storage/src/valkey_cache.rs`
- **What it does:** deadpool-redis async pool, typed helpers (get/set/del/exists/set_nx_ex), health check via PING

### Supporting: Strategy & Indicators — COMPLETE
- **Files:** `trading/src/strategy/` (config, evaluator, hot_reload, types, tests) + `trading/src/indicator/` (engine, types, tests)
- **What it does:** TOML-based strategy config, indicator engine with 9 indicator types (SMA, EMA, RSI, MACD, BB, ATR, VWAP, SuperTrend, Stochastic), signal evaluation, hot reload
- **Key types:** `StrategyConfig`, `IndicatorEngine`, `Signal`, `IndicatorType`
- **Tests:** Indicator-strategy e2e integration, config validation, hot reload

### Supporting: Network — COMPLETE
- **Files:** `core/src/network/` (ip_monitor, ip_verifier, mod)
- **What it does:** Static IP verification for order APIs, IP change detection

### Supporting: Scheduler — COMPLETE
- **Files:** `core/src/scheduler/mod.rs`
- **What it does:** Time-based scheduling for daily tasks (instrument refresh, historical fetch)

### Supporting: Deploy & Auto-Start — COMPLETE
- **Files:** Various scripts and configs
- **What it does:** Mac auto-start (launchd), AWS systemd, Docker Compose orchestration, one-click deploy, smoke tests, Mac decommission for AWS migration

---

## 4. What's PENDING (Remaining Phase 1 Work)

### P1: OMS ↔ Live Integration (Block 10 completion)
**Scope:** Wire the existing OMS engine to real Dhan REST API and Order Update WS
**Files to modify:** `trading/src/oms/engine.rs`, `trading/src/oms/api_client.rs`, `core/src/websocket/order_update_connection.rs`
**Subtasks:**
1. Implement real HTTP calls in `api_client.rs` (currently has types but needs `reqwest` calls)
2. Wire Order Update WS messages → OMS state machine transitions
3. Reconciliation on WS reconnect: fetch `/v2/orders` → compare with local state → alert on mismatch
4. Integration test with mock HTTP server (wiremock or similar)

**Dhan API reference:** `docs/dhan-ref/07-orders.md`, `docs/dhan-ref/10-live-order-update-websocket.md`
**Constraints:** Static IP required for order APIs, rate limit 10/sec, `securityId` is STRING in requests

### P2: Option Chain Analytics
**Scope:** Fetch option chain from Dhan API, build strike selection logic
**Files to create/modify:** `core/src/option_chain/` (new module)
**Subtasks:**
1. Option chain API client (`POST /v2/optionchain`)
2. Expiry list fetcher (`POST /v2/optionchain/expirylist`)
3. Strike price parser (keys are decimal strings like `"25650.000000"`)
4. ATM strike finder based on underlying LTP
5. Greeks extraction (delta, theta, gamma, vega)
6. Integration with strategy evaluator

**Dhan API reference:** `docs/dhan-ref/06-option-chain.md`
**Constraints:** Requires `client-id` header, rate limit 1 unique request per 3 seconds, CE/PE may be `None`

### P3: Kill Switch & Emergency Controls
**Scope:** Wire Dhan kill switch and P&L exit APIs into risk engine
**Files to create/modify:** `trading/src/risk/kill_switch.rs` (new), `trading/src/risk/pnl_exit.rs` (new)
**Subtasks:**
1. Kill switch activate/deactivate/status API calls
2. P&L exit configure/stop/status API calls
3. Wire kill switch to error handlers (critical error → kill switch)
4. Wire P&L exit to risk engine thresholds
5. Session-scoped P&L exit reconfiguration on daily reset

**Dhan API reference:** `docs/dhan-ref/15-traders-control.md`
**Constraints:** Kill switch requires all positions closed first, P&L values are STRINGS, P&L exit is session-scoped

### P4: Portfolio & Position Management
**Scope:** Real-time position tracking with Dhan API integration
**Files to create/modify:** `trading/src/oms/` additions
**Subtasks:**
1. Holdings API client (`GET /v2/holdings`)
2. Positions API client (`GET /v2/positions`)
3. Position conversion (`POST /v2/positions/convert`) — `convertQty` is STRING
4. Exit-all emergency (`DELETE /v2/positions`)
5. Wire position updates from Order Update WS into position tracker

**Dhan API reference:** `docs/dhan-ref/12-portfolio-positions.md`
**Constraints:** `availableQty` != `totalQty`, `netQty` can be negative (short)

### P5: Margin & Funds Pre-Check
**Scope:** Pre-trade margin validation before order placement
**Files to create/modify:** `trading/src/oms/margin_checker.rs` (new)
**Subtasks:**
1. Single margin calculator (`POST /v2/margincalculator`)
2. Fund limit check (`GET /v2/fundlimit`)
3. Pre-trade gate: if `insufficientBalance > 0` → reject order before sending to Dhan
4. Wire into OMS engine order flow

**Dhan API reference:** `docs/dhan-ref/13-funds-margin.md`
**Constraints:** `availabelBalance` has Dhan's typo — use exact field name, `leverage` is STRING

### P6: Super Order Support
**Scope:** Bracket order (entry + target + stop loss) via Dhan Super Order API
**Files to create/modify:** `trading/src/oms/super_order.rs` (new)
**Subtasks:**
1. Place super order API client
2. Modify by leg (entry: all fields, target: only targetPrice, SL: only stopLossPrice + trailingJump)
3. Cancel semantics (cancel entry = cancel all, cancel target/SL = permanent removal)
4. Leg tracking from Order Update WS

**Dhan API reference:** `docs/dhan-ref/07a-super-order.md`
**Constraints:** Static IP required, `trailingJump: 0` cancels trailing (not None)

### P7: Forever Order (GTT) Support
**Scope:** Good-till-triggered orders for persistent price alerts
**Files to create/modify:** `trading/src/oms/forever_order.rs` (new)
**Subtasks:**
1. SINGLE and OCO order types
2. OCO second-leg fields (price1, triggerPrice1, quantity1)
3. CONFIRM status handling (unique to forever orders)

**Dhan API reference:** `docs/dhan-ref/07b-forever-order.md`
**Constraints:** CNC/MTF only (no INTRADAY), static IP required

---

## 5. Quick Reference: Dhan API Endpoints

### Already Integrated
| Endpoint | Method | Location in Code |
|----------|--------|-----------------|
| `/v2/charts/historical` | POST | `core/src/historical/candle_fetcher.rs` |
| `/v2/charts/intraday` | POST | `core/src/historical/candle_fetcher.rs` |
| `wss://api-feed.dhan.co` | WS | `core/src/websocket/connection.rs` |
| `wss://api-order-update.dhan.co` | WS | `core/src/websocket/order_update_connection.rs` |
| `POST auth.dhan.co/app/generateAccessToken` | POST | `core/src/auth/token_manager.rs` |
| `GET /v2/RenewToken` | GET | `core/src/auth/token_manager.rs` |
| `GET /v2/profile` | GET | `core/src/auth/token_manager.rs` |
| `POST /v2/ip/setIP` | POST | `core/src/network/ip_verifier.rs` |
| `GET /v2/ip/getIP` | GET | `core/src/network/ip_verifier.rs` |

### Not Yet Integrated
| Endpoint | Method | Needed For |
|----------|--------|------------|
| `POST /v2/orders` | POST | P1: Place order |
| `PUT /v2/orders/{id}` | PUT | P1: Modify order |
| `DELETE /v2/orders/{id}` | DELETE | P1: Cancel order |
| `GET /v2/orders` | GET | P1: Order book |
| `GET /v2/trades` | GET | P1: Trade book |
| `POST /v2/optionchain` | POST | P2: Option chain |
| `POST /v2/optionchain/expirylist` | POST | P2: Expiry list |
| `POST /v2/killswitch` | POST | P3: Kill switch |
| `GET /v2/killswitch` | GET | P3: Kill switch status |
| `POST /v2/pnlExit` | POST | P3: P&L exit |
| `GET /v2/holdings` | GET | P4: Holdings |
| `GET /v2/positions` | GET | P4: Positions |
| `DELETE /v2/positions` | DELETE | P4: Exit all |
| `POST /v2/margincalculator` | POST | P5: Margin check |
| `GET /v2/fundlimit` | GET | P5: Fund limit |
| `POST /v2/super/orders` | POST | P6: Super order |
| `POST /v2/forever/orders` | POST | P7: Forever order |
| `POST /v2/marketfeed/ltp` | POST | Market quote REST |
| `POST /v2/marketfeed/ohlc` | POST | Market quote REST |
| `POST /v2/marketfeed/quote` | POST | Market quote REST |

---

## 6. Test Coverage by Crate

| Crate | Unit Tests | Integration Tests | Total | Required Types (22-test-types.md) |
|-------|-----------|-------------------|-------|----------------------------------|
| **common** | 265 | 25 (schema) | 290 | 12 types |
| **core** | 1,311 | ~130 (12 test files) | ~1,441 | 19 types |
| **trading** | 233 + 76 | ~200 (9 test files) | ~509 | 13 types |
| **storage** | 21 | 7 (2 test files) | 28 | 8 types |
| **api** | 64 | 28 (3 test files) | 92 | 5 types |
| **app** | 23 | 0 | 23 | 2 types |
| **TOTAL** | | | **2,684** | |

---

## 7. Key Files Quick Lookup

### Config & Constants
- `common/src/config.rs` — All app configuration (TOML-based)
- `common/src/constants.rs` — API URLs, limits, sizes
- `common/src/error.rs` — Application error types
- `common/src/types.rs` — Shared type aliases

### Hot Path (zero-alloc)
- `core/src/parser/header.rs` — 8-byte WS header
- `core/src/parser/ticker.rs` — 16-byte ticker
- `core/src/parser/quote.rs` — 50-byte quote
- `core/src/parser/full_packet.rs` — 162-byte full
- `core/src/parser/dispatcher.rs` — enum_dispatch routing
- `core/src/pipeline/tick_processor.rs` — Tick processing loop
- `core/src/pipeline/candle_aggregator.rs` — 1s candle builder

### Trading
- `trading/src/oms/engine.rs` — OMS core engine
- `trading/src/oms/state_machine.rs` — Order state machine (statig)
- `trading/src/oms/rate_limiter.rs` — GCRA rate limiter
- `trading/src/oms/api_client.rs` — Dhan REST API client (types ready, calls pending)
- `trading/src/risk/engine.rs` — Risk engine
- `trading/src/strategy/evaluator.rs` — Signal evaluation
- `trading/src/indicator/engine.rs` — Indicator computation

### Infrastructure
- `app/src/main.rs` — Boot sequence orchestration
- `app/src/infra.rs` — Docker/infra health checks
- `app/src/observability.rs` — Tracing + metrics init
- `storage/src/tick_persistence.rs` — QuestDB ILP writer
- `storage/src/valkey_cache.rs` — Valkey (Redis) cache

---

## 8. Dhan API Reference Quick Index

All ground truth docs are in `docs/dhan-ref/`:

| Doc | What | When to Read |
|-----|------|--------------|
| `01-introduction-and-rate-limits.md` | Base URL, headers, rate limits, error codes | Any REST API work |
| `02-authentication.md` | Token lifecycle, TOTP, static IP, profile | Auth changes |
| `03-live-market-feed-websocket.md` | Binary WS protocol, packet layouts | Parser changes |
| `04-full-market-depth-websocket.md` | 20/200-level depth, 12-byte header | Depth parser changes |
| `05-historical-data.md` | OHLCV fetch, columnar response | Historical data changes |
| `06-option-chain.md` | Option chain API, greeks | Option chain work |
| `07-orders.md` | Order CRUD, trade book | OMS work |
| `07a-super-order.md` | Bracket orders, 3 legs | Super order work |
| `07b-forever-order.md` | GTT, OCO orders | Forever order work |
| `07c-conditional-trigger.md` | Alert-based triggers | Conditional trigger work |
| `07d-edis.md` | EDIS/T-PIN for selling holdings | Holdings sell work |
| `07e-postback.md` | Webhooks (prefer WS instead) | Webhook work |
| `08-annexure-enums.md` | ALL enums, error codes, mappings | Any Dhan API work |
| `09-instrument-master.md` | CSV download, SecurityId lookup | Instrument changes |
| `10-live-order-update-websocket.md` | JSON WS, order status push | Order update work |
| `11-market-quote-rest.md` | LTP/OHLC/quote REST snapshots | Quote REST work |
| `12-portfolio-positions.md` | Holdings, positions, exit-all | Portfolio work |
| `13-funds-margin.md` | Margin calculator, fund limits | Margin work |
| `14-statements-trade-history.md` | Ledger, trade history | Reporting work |
| `15-traders-control.md` | Kill switch, P&L exit | Emergency controls |
| `16-release-notes.md` | API versions, breaking changes | Version checks |

---

## 9. Implementation Priority Order

Based on trading system criticality:

```
PRIORITY 1 (Must have for live trading):
  P1: OMS ↔ Live Integration     ← Can't trade without this
  P3: Kill Switch                 ← Can't trade safely without this
  P5: Margin Pre-Check            ← Can't validate orders without this

PRIORITY 2 (Needed for F&O strategy):
  P2: Option Chain Analytics      ← Strategy needs strike selection
  P4: Portfolio & Positions       ← Need position tracking

PRIORITY 3 (Enhanced order types):
  P6: Super Order                 ← Bracket orders
  P7: Forever Order (GTT)         ← Persistent triggers
```

---

## 10. Three Principles Checklist

Before any new code, verify:

| # | Principle | How to Check |
|---|-----------|-------------|
| 1 | Zero allocation on hot path | `dhat` test for any code in parser/pipeline/tick path |
| 2 | O(1) or fail at compile time | No `Vec::push`, no `HashMap::insert` on hot path; array/fixed-size only |
| 3 | Every version pinned | Exact version in root `Cargo.toml`, no `^`/`~`/`*`/`>=` |

---

## 11. Session Quick-Start

```bash
# 1. Pull latest
git pull

# 2. Read this file + CLAUDE.md + phase doc
# 3. Check recent history
git log --oneline -20

# 4. Verify build
cargo check && cargo test

# 5. Start working on next priority item
# 6. When done: /quality → commit → push → update this file
```
