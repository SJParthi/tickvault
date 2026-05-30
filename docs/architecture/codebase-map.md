# Codebase Map — tickvault

> Session start: read THIS file only. Skip CLAUDE.md (auto-loads).
> S6-Step8 (2026-04-14): The Tech Stack Bible was deleted. The single source of truth for all dependency versions is the workspace root `Cargo.toml` `[workspace.dependencies]` block.
> Updated: 2026-03-17 (~2,439 tests).

## File Tree

```
tickvault/
├── Cargo.toml                          # Workspace root — ALL versions pinned
├── CLAUDE.md                           # Project rules (auto-loaded)
├── config/
│   └── base.toml                       # Runtime config (non-secret)
├── crates/
│   ├── common/src/                     # Shared types — imported by all crates
│   │   ├── lib.rs                      # Re-exports: config, constants, error, types, instrument_types, tick_types, instrument_registry
│   │   ├── config.rs                   # ApplicationConfig + validate() — 12 subsections
│   │   ├── constants.rs                # 100+ named constants + compile-time assertions + byte offsets + subscription limits
│   │   ├── error.rs                    # ApplicationError enum (12 variants, thiserror)
│   │   ├── types.rs                    # Exchange, ExchangeSegment, FeedMode, InstrumentType, OptionType
│   │   ├── instrument_types.rs         # FnoUniverse, Underlying, DerivativeContract, OptionChain, etc.
│   │   ├── instrument_registry.rs      # InstrumentRegistry (O(1) security_id → SubscribedInstrument lookup)
│   │   └── tick_types.rs               # ParsedTick, MarketDepthLevel (Copy types for zero-alloc hot path)
│   ├── core/src/                       # Core engine
│   │   ├── lib.rs                      # Re-exports: auth, instrument, websocket, parser, pipeline
│   │   ├── auth/
│   │   │   ├── mod.rs                  # Re-exports auth submodules
│   │   │   ├── secret_manager.rs       # AWS SSM fetch → DhanCredentials
│   │   │   ├── totp_generator.rs       # TOTP 6-digit code from base32 secret
│   │   │   ├── token_manager.rs        # JWT lifecycle: acquire (auth.dhan.co), renew (api.dhan.co), arc-swap
│   │   │   └── types.rs               # DhanCredentials, TokenState, DhanGenerateTokenResponse, parse_expiry_time()
│   │   ├── instrument/
│   │   │   ├── mod.rs                  # Re-exports instrument submodules + build_subscription_plan()
│   │   │   ├── csv_downloader.rs       # Download Dhan CSV (primary + fallback + cache)
│   │   │   ├── csv_parser.rs           # Parse 260K rows → 147K RawInstruments
│   │   │   ├── universe_builder.rs     # 5-pass F&O universe build → FnoUniverse
│   │   │   ├── subscription_planner.rs # FnoUniverse → SubscriptionPlan (filtered instruments + registry)
│   │   │   └── validation.rs           # Post-build validation (must-exist checks)
│   │   ├── websocket/
│   │   │   ├── mod.rs                  # Re-exports websocket submodules
│   │   │   ├── types.rs               # ConnectionState, DisconnectCode(805-809), WebSocketError, InstrumentSubscription
│   │   │   ├── tls.rs                 # TLS connector with rustls (aws-lc-rs provider)
│   │   │   ├── subscription_builder.rs # JSON batch builder (subscribe/unsubscribe/disconnect)
│   │   │   ├── connection.rs          # Single WebSocket lifecycle (URL query auth, subscribe, read)
│   │   │   ├── connection_pool.rs     # Multi-connection pool (up to 5 connections, 25K instruments)
│   │   │   └── order_update_connection.rs # Order update WebSocket connection (separate feed)
│   │   ├── parser/
│   │   │   ├── mod.rs                  # Re-exports parser submodules
│   │   │   ├── types.rs               # ParsedTick, PacketHeader, MarketDepthLevel, ParsedFrame, ParseError
│   │   │   ├── read_helpers.rs        # Shared read utilities for binary parsing
│   │   │   ├── header.rs              # parse_header() — 8-byte packet header
│   │   │   ├── ticker.rs             # parse_ticker_packet() — 16 bytes
│   │   │   ├── quote.rs              # parse_quote_packet() — 50 bytes (OHLC at offset 34)
│   │   │   ├── full_packet.rs        # parse_full_packet() — 162 bytes (OI at 34, OHLC at 46, depth at 62)
│   │   │   ├── market_depth.rs       # parse_market_depth_packet() — 20 bytes per level
│   │   │   ├── deep_depth.rs         # parse_deep_depth_packet() — extended depth (20 levels)
│   │   │   ├── oi.rs                  # parse_oi_packet() — 12 bytes
│   │   │   ├── previous_close.rs     # parse_previous_close_packet() — 16 bytes
│   │   │   ├── market_status.rs      # validate_market_status_packet() — 8 bytes
│   │   │   ├── disconnect.rs         # parse_disconnect_packet() — 10 bytes
│   │   │   ├── order_update.rs       # parse_order_update_packet() — order update binary frames
│   │   │   └── dispatcher.rs         # dispatch_frame() — top-level entry, routes by response_code
│   │   ├── notification/
│   │   │   ├── mod.rs                 # Re-exports notification submodules
│   │   │   ├── service.rs            # NotificationService (Telegram alerts via AWS SSM)
│   │   │   └── events.rs             # NotificationEvent enum (auth, instrument, custom alerts)
│   │   ├── historical/
│   │   │   ├── mod.rs                 # Re-exports historical submodules
│   │   │   ├── candle_fetcher.rs     # Fetch 1-min candles from Dhan REST API
│   │   │   └── cross_verify.rs       # Cross-verify fetched candles against live data
│   │   └── pipeline/
│   │       ├── mod.rs                 # Re-exports pipeline submodules
│   │       ├── tick_processor.rs     # Main loop: frame → parse → filter junk → dedup → persist
│   │       ├── candle_aggregator.rs  # Real-time 1-min candle aggregation from ticks
│   │       └── top_movers.rs         # Top movers by change_pct (shared snapshot for API)
│   ├── storage/src/                    # Persistence layer
│   │   ├── lib.rs                      # Re-exports: instrument_persistence, tick_persistence, candle_persistence
│   │   ├── instrument_persistence.rs   # QuestDB ILP writer (4 tables)
│   │   ├── tick_persistence.rs        # QuestDB ILP writer for ticks table + ensure_tick_table_dedup_keys()
│   │   ├── candle_persistence.rs      # QuestDB ILP writer for candles_1m table
│   │   ├── materialized_views.rs      # QuestDB materialized views (candle aggregation)
│   │   └── valkey_cache.rs            # Valkey (Redis) cache for rkyv-serialized instrument data
│   ├── trading/src/
│   │   └── lib.rs                      # SKELETON — OMS, risk (not started)
│   ├── api/src/
│   │   ├── lib.rs                      # build_router() — axum router with CORS
│   │   ├── state.rs                    # SharedAppState (questdb_config, dhan_config, top_movers snapshot)
│   │   └── handlers/
│   │       ├── mod.rs                  # Re-exports handler submodules
│   │       ├── health.rs              # GET /health → JSON status + version
│   │       ├── stats.rs               # GET /api/stats → QuestDB table counts (proxied, no CORS)
│   │       ├── instruments.rs         # POST /api/instruments/rebuild → trigger instrument rebuild
│   │       └── debug.rs               # GET /api/debug/logs/* → MCP read-only observability
│   │       # (the /portal HTML frontend + static_file.rs + portal.html
│   │       #  were retired in the AWS-lifecycle PRs, 2026-05-19)
│   └── app/
│       └── src/main.rs                 # Full orchestration: CryptoProvider → Config → Auth → Universe → Subscription → WS → Parse → Persist → HTTP
├── deploy/docker/
│   └── docker-compose.yml              # QuestDB + tickvault app only, SHA256-pinned
│       # (Grafana #O1, Alertmanager #O2, Prometheus #O3, Valkey #O4 +
│       #  the Loki/Alloy/Traefik/Jaeger configs were removed in the
│       #  CloudWatch-only migration, 2026-05-19/05-24; metrics/logs/
│       #  alarms/dashboards live in AWS CloudWatch)
├── scripts/
│   ├── setup-secrets.sh               # Seeds SSM params in AWS
│   └── notify-telegram.sh             # Sends Telegram alerts via real AWS SSM
└── docs/
    ├── # tech-stack-bible.md DELETED in S6-Step8 — Cargo.toml is the source of truth
    ├── architecture/codebase-map.md     # THIS FILE
    └── phases/
        └── phase-1-live-trading.md     # Full Phase 1 spec (1,412 lines)
```

## Config Shape (config/base.toml)

```toml
[trading]     # market_open "09:00", market_close "15:30", timezone "Asia/Kolkata"
[dhan]        # websocket_url, rest_api_base_url, auth_base_url, instrument_csv_urls, max_instruments
[questdb]     # host "tv-questdb", http_port 9000, pg_port 8812, ilp_port 9009
[valkey]      # host "tv-valkey", port 6379, max_connections 16
[prometheus]  # host "tv-prometheus", port 9090
[websocket]   # ping 10s, pong_timeout 10s, reconnect backoff, batch_size 100
[network]     # request_timeout_ms 5000, ws_timeout_ms 10000, backoff, retries
[token]       # validity_hours 24, refresh_before_expiry_hours 1
[risk]        # max_daily_loss_percent 2.0, max_position_lots 50
[logging]     # format "pretty", level "info"
[instrument]     # daily_download_time "08:45", cache_directory "/app/data/"
[subscription]   # feed_mode "Full", atm_strike_range 10, major_indices, display_indices
[api]            # host "0.0.0.0", port 3001
```

## Dhan Auth Endpoints (SDK-verified)

```
Token generation:  POST https://auth.dhan.co/app/generateAccessToken
                   Query params: dhanClientId, pin (6-digit trading PIN), totp
                   Response: flat camelCase JSON { dhanClientId, accessToken, expiryTime }

Token renewal:     GET https://api.dhan.co/v2/RenewToken
                   Headers: access-token, dhanClientId
                   Response: flat camelCase JSON { accessToken, dhanClientId }

IMPORTANT: Dhan returns HTTP 200 OK even on auth errors.
           Must check body for {"status":"error","message":"..."} pattern.
```

## Error Variants (ApplicationError)

```rust
Configuration(String)
SecretRetrieval { path, source }
MarketHourViolation(String)
InstrumentDownloadFailed { url, source }
InstrumentParseFailed { reason }
UniverseValidationFailed { check_name, details }
CsvColumnMissing { column_name }
QuestDbWriteFailed { table_name, source }
TotpGenerationFailed(String)
AuthenticationFailed { reason }
TokenRenewalFailed { attempt, source }
AuthCircuitBreakerOpen(String)
```

## Public APIs by Crate

### common
```rust
// config.rs
ApplicationConfig::validate(&self) -> Result<()>
DhanConfig { websocket_url, rest_api_base_url, auth_base_url, instrument_csv_url, ... }
SubscriptionConfig { feed_mode, atm_strike_range, major_indices, display_indices }
ApiConfig { host, port }

// types.rs — enums with as_str() + Display + Serialize/Deserialize
Exchange { NationalStockExchange, BombayStockExchange }
ExchangeSegment { IdxI, NseEquity, NseFnO, BseEquity, BseFnO, McxComm }
FeedMode { Ticker, Quote, Full }
InstrumentType { Equity, Future, Option }
OptionType { Call, Put }

// tick_types.rs — hot-path types (Copy, zero-alloc)
ParsedTick { security_id, exchange_segment_code, last_traded_price, ... }
MarketDepthLevel { bid_quantity, ask_quantity, bid_orders, ask_orders, bid_price, ask_price }

// instrument_types.rs — domain structs
FnoUniverse { underlyings, derivative_contracts, option_chains, subscribed_indices, ... }

// instrument_registry.rs — O(1) lookup by security_id
InstrumentRegistry::get(security_id) -> Option<&SubscribedInstrument>
InstrumentRegistry::iter() -> impl Iterator<Item = &SubscribedInstrument>
InstrumentRegistry::total_count() -> usize
```

### core::auth
```rust
secret_manager::fetch_dhan_credentials() -> Result<DhanCredentials>
totp_generator::generate_totp(base32_secret: &str) -> Result<String>
token_manager::TokenManager::initialize(dhan_config, token_config, network_config) -> Result<Self>
token_manager::TokenManager::token_handle(&self) -> TokenHandle
token_manager::TokenManager::spawn_renewal_task(&self) -> JoinHandle<()>
```

### core::parser
```rust
dispatch_frame(raw: &[u8], received_at_nanos: i64) -> Result<ParsedFrame, ParseError>
ParsedFrame { Tick(ParsedTick), TickWithDepth(ParsedTick, [MarketDepthLevel; 5]),
              OiUpdate, PreviousClose, MarketStatus, Disconnect(DisconnectCode) }
```

### core::pipeline
```rust
// Main loop: parse → filter junk → dedup → candle aggregation → persist
run_tick_processor(
    frame_receiver: mpsc::Receiver<Vec<u8>>,
    tick_writer: Option<TickPersistenceWriter>,
    depth_writer: Option<DepthPersistenceWriter>,
    registry: Option<Arc<InstrumentRegistry>>,
    top_movers_snapshot: SharedTopMoversSnapshot,
) -> ()  // async — runs until receiver closes

// Real-time candle aggregation
CandleAggregator::new() -> Self
CandleAggregator::update(&mut self, tick: &ParsedTick)

// Top movers tracking
TopMoversTracker::new() -> Self
TopMoversTracker::update(&mut self, tick: &ParsedTick, registry: &InstrumentRegistry)
```

### core::websocket
```rust
WebSocketConnectionPool::new(token, client_id, dhan_config, ws_config, instruments, feed_mode) -> Result<Self>
WebSocketConnectionPool::spawn_all(&mut self) -> Vec<JoinHandle<Result<()>>>
WebSocketConnectionPool::take_frame_receiver(&mut self) -> mpsc::Receiver<Vec<u8>>
```

### storage
```rust
TickPersistenceWriter::new(config: &QuestDbConfig) -> Result<Self>
TickPersistenceWriter::append_tick(&mut self, tick: &ParsedTick) -> Result<()>
TickPersistenceWriter::flush_if_needed(&mut self) -> Result<()>
TickPersistenceWriter::force_flush(&mut self) -> Result<()>
ensure_tick_table_dedup_keys(config: &QuestDbConfig) -> ()
instrument_persistence::persist_instrument_snapshot(universe, questdb_config) -> Result<()>
```

### api
```rust
build_router(state: SharedAppState) -> Router
SharedAppState::new(questdb, dhan, instrument, top_movers_snapshot) -> Self

// Endpoints
// (the /portal HTML Control Panel was retired in the AWS-lifecycle PRs,
//  2026-05-19; operator visualization is CloudWatch / QuestDB console)
GET   /health                → { "status": "ok", "version": "0.1.0" }
GET   /api/stats             → { questdb_reachable, tables, underlyings, derivatives, subscribed_indices, ticks }
GET   /api/top-movers        → Top movers by change_pct (from shared snapshot)
POST  /api/instruments/rebuild → Trigger instrument rebuild outside build window
```

## Byte Layouts — Dhan WebSocket V2 Binary Protocol (SDK-verified)

```
Header (8 bytes):     [response_code:u8][msg_length:u16LE][segment:u8][security_id:u32LE]
Ticker (16 bytes):    Header + [ltp:f32LE][ltt:u32LE]
Quote (50 bytes):     Header + [ltp:f32][ltq:u16][ltt:u32][atp:f32][vol:u32][sell:u32][buy:u32][open:f32][close:f32][high:f32][low:f32]
Full (162 bytes):     Header + [ltp:f32][ltq:u16][ltt:u32][atp:f32][vol:u32][sell:u32][buy:u32][OI:u32][OI_high:u32][OI_low:u32][open:f32][close:f32][high:f32][low:f32][depth:5×20]
OI (12 bytes):        Header + [oi:u32LE]
PrevClose (16 bytes): Header + [prev_close:f32LE][prev_oi:u32LE]
MarketStatus (8):     Header only
Disconnect (10):      Header + [code:u16LE]

CRITICAL: Quote vs Full diverge at offset 34. Prices are f32 in rupees (NOT paise).
```

## Test Counts (~2,439 total)

| Crate | Tests |
|-------|-------|
| common | 191 |
| core | 1450 |
| trading | 401 |
| storage | 259 |
| api | 105 |
| app | 21 |
| **Total** | **~2,439** |

## QuestDB Tables (5) — DEDUP UPSERT KEYS enabled on all

| Table | UPSERT KEYS | Purpose |
|-------|-------------|---------|
| instrument_build_metadata | timestamp, csv_source | Build audit trail |
| fno_underlyings | timestamp, underlying_symbol | F&O underlyings |
| derivative_contracts | timestamp, security_id | All derivative contracts |
| subscribed_indices | timestamp, security_id | Index subscriptions |
| ticks | ts, security_id | Raw tick data (PARTITION BY HOUR) |

## App Boot Sequence (main.rs) — 14 steps

```
 0. Install rustls CryptoProvider (aws_lc_rs)
 1. Load config/base.toml → ApplicationConfig → validate()
 2. Initialize observability (Prometheus metrics exporter)
 3. Initialize structured logging + OpenTelemetry tracing layer
 4. Initialize notification service (Telegram via AWS SSM, best-effort)
 5. Authenticate: SSM → TOTP → POST auth.dhan.co → JWT (timeout + graceful failure)
 6. QuestDB: ensure tables (ticks, depth, instruments, candles, materialized views)
 7. Build F&O universe + subscription plan (three-layer: fresh build / rkyv cache / unavailable)
 7.5. Fetch historical 1-min candles + cross-verify (automated, non-critical)
 8. WebSocket pool: dynamic instruments from SubscriptionPlan
 9. Tick processor task (parse → filter → dedup → candle aggregation → persist)
10. Order update WebSocket connection (separate feed)
11. axum API server on 0.0.0.0:3001 (health, stats, portal, top-movers, instruments)
12. Token renewal background task
13. Market close (15:30) → WS disconnect → historical re-fetch → cross-verify → auto-shutdown at 16:00 IST
```

## Completed Blocks

- **Block 01**: Instrument download, CSV parse, 5-pass universe build, validation
- **Block 01.1**: QuestDB persistence (4 tables via ILP, DEDUP UPSERT KEYS)
- **Block 02**: Auth pipeline (SSM → TOTP → JWT → arc-swap → auto-renew)
- **Block 03**: WebSocket Connection Manager (types, subscription builder, connection lifecycle, pool)
- **Block 04**: Binary Protocol Parser — dispatch_frame() routes 9 packet types (incl. market_depth, deep_depth, order_update)
- **Block 05**: Tick Pipeline — parse → filter junk → dedup → candle aggregation → top movers → persist
- **Subscription Planner**: InstrumentRegistry (O(1) lookup) + SubscriptionPlanner (FnoUniverse → filtered instruments)
- **Notification**: Telegram alerts via AWS SSM (auth events, instrument builds, custom)
- **Historical candles**: Fetch + cross-verify 1-min candles from Dhan REST API
- **Order update WebSocket**: Separate feed for order status updates
- **Candle aggregation**: Real-time 1-min candle aggregation from tick stream
- **Top movers**: Real-time top movers by change_pct (shared snapshot for API)
- **Observability**: Prometheus metrics + OpenTelemetry tracing
- **Valkey cache**: rkyv-serialized instrument cache for zero-copy deserialization
- **Trading crate**: OMS state machine, order types, position tracking (86 tests)
