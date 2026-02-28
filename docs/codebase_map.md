# Codebase Map — dhan-live-trader

> Session start: read THIS file only. Skip CLAUDE.md (auto-loads), skip PDFs.
> Bible: `tech_stack_bible_v6.md` — read ONLY when adding a new dependency.
> Updated: 2026-02-27 after frontend/candle stripping (463 tests).

## File Tree

```
dhan-live-trader/
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
│   │   │   ├── secret_manager.rs       # SSM fetch (LocalStack/AWS) → DhanCredentials
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
│   │   │   └── connection_pool.rs     # Multi-connection pool (up to 5 connections, 25K instruments)
│   │   ├── parser/
│   │   │   ├── mod.rs                  # Re-exports parser submodules
│   │   │   ├── types.rs               # ParsedTick, PacketHeader, MarketDepthLevel, ParsedFrame, ParseError
│   │   │   ├── header.rs              # parse_header() — 8-byte packet header
│   │   │   ├── ticker.rs             # parse_ticker_packet() — 16 bytes
│   │   │   ├── quote.rs              # parse_quote_packet() — 50 bytes (OHLC at offset 34)
│   │   │   ├── full_packet.rs        # parse_full_packet() — 162 bytes (OI at 34, OHLC at 46, depth at 62)
│   │   │   ├── oi.rs                  # parse_oi_packet() — 12 bytes
│   │   │   ├── previous_close.rs     # parse_previous_close_packet() — 16 bytes
│   │   │   ├── market_status.rs      # parse_market_status_packet() — 8 bytes
│   │   │   ├── disconnect.rs         # parse_disconnect_packet() — 10 bytes
│   │   │   └── dispatcher.rs         # dispatch_frame() — top-level entry, routes by response_code
│   │   └── pipeline/
│   │       ├── mod.rs                 # Re-exports pipeline submodules
│   │       └── tick_processor.rs     # Main loop: frame → parse → filter junk → persist to QuestDB
│   ├── storage/src/                    # Persistence layer
│   │   ├── lib.rs                      # Re-exports: instrument_persistence, tick_persistence
│   │   ├── instrument_persistence.rs   # QuestDB ILP writer (4 tables)
│   │   └── tick_persistence.rs        # QuestDB ILP writer for ticks table + ensure_tick_table_dedup_keys()
│   ├── trading/src/
│   │   └── lib.rs                      # SKELETON — OMS, risk (not started)
│   ├── api/src/
│   │   ├── lib.rs                      # build_router() — axum router with CORS: /health, /api/stats, /portal
│   │   ├── state.rs                    # SharedAppState (questdb_config only)
│   │   └── handlers/
│   │       ├── mod.rs                  # Re-exports handler submodules
│   │       ├── health.rs              # GET /health → JSON status + version
│   │       ├── stats.rs               # GET /api/stats → QuestDB table counts (proxied, no CORS)
│   │       └── static_file.rs         # GET /portal → portal.html (DLT Control Panel, embedded at compile time)
│   │   └── static/
│   │       └── portal.html            # DLT Control Panel — nav dashboard with live status + stats
│   └── app/
│       └── src/main.rs                 # Full orchestration: CryptoProvider → Config → Auth → Universe → Subscription → WS → Parse → Persist → HTTP
├── deploy/docker/
│   ├── docker-compose.yml              # 9 services, all SHA256-pinned, health-checked
│   ├── prometheus/prometheus.yml       # Scrape targets: app:9091, questdb:9003
│   ├── loki/loki-config.yml            # TSDB schema, 30d retention
│   ├── alloy/alloy-config.alloy        # Docker log discovery → Loki
│   ├── traefik/traefik.yml             # Reverse proxy, blue-green ready
│   └── grafana/provisioning/
│       └── datasources/datasources.yml # Prometheus + Loki datasources
├── scripts/
│   ├── seed-localstack-secrets.sh      # Seeds 5 SSM params in LocalStack
│   └── notify-telegram.sh             # Sends Telegram alerts via real AWS SSM
└── docs/
    ├── tech_stack_bible_v6.md          # 113 components (converted from PDF)
    ├── codebase_map.md                 # THIS FILE
    └── phases/
        └── PHASE_1_LIVE_TRADING.md     # Full Phase 1 spec (1,412 lines)
```

## Config Shape (config/base.toml)

```toml
[trading]     # market_open "09:15", market_close "15:30", timezone "Asia/Kolkata"
[dhan]        # websocket_url, rest_api_base_url, auth_base_url, instrument_csv_urls, max_instruments
[questdb]     # host "localhost", http_port 9000, pg_port 8812, ilp_port 9009
[valkey]      # host "dlt-valkey", port 6379, max_connections 16
[prometheus]  # host "dlt-prometheus", port 9090
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
// Pure capture loop: parse → filter junk → persist to QuestDB
run_tick_processor(
    frame_receiver: mpsc::Receiver<Vec<u8>>,
    tick_writer: Option<TickPersistenceWriter>,
) -> ()  // async — runs until receiver closes
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
SharedAppState::new(questdb: QuestDbConfig) -> Self

// Endpoints
GET  /portal        → DLT Control Panel (nav dashboard with live status, stats, service links)
GET  /health        → { "status": "ok", "version": "0.1.0" }
GET  /api/stats     → { questdb_reachable, tables, underlyings, derivatives, subscribed_indices, ticks }
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

## Test Counts (463 total)

| Crate | Tests |
|-------|-------|
| common | 88 |
| core | 335 |
| storage | 37 |
| api | 3 |
| **Total** | **463** |

## QuestDB Tables (5) — DEDUP UPSERT KEYS enabled on all

| Table | UPSERT KEYS | Purpose |
|-------|-------------|---------|
| instrument_build_metadata | timestamp, csv_source | Build audit trail |
| fno_underlyings | timestamp, underlying_symbol | F&O underlyings |
| derivative_contracts | timestamp, security_id | All derivative contracts |
| subscribed_indices | timestamp, security_id | Index subscriptions |
| ticks | ts, security_id | Raw tick data (PARTITION BY HOUR) |

## App Boot Sequence (main.rs) — 10 steps

```
0. Install rustls CryptoProvider (aws_lc_rs)
1. Load config/base.toml → ApplicationConfig → validate()
2. Initialize tracing (structured logging)
3. Authenticate: SSM → TOTP → POST auth.dhan.co → JWT (graceful: OFFLINE mode if fails)
4. QuestDB: ensure ticks table with DEDUP UPSERT KEYS
5. Build F&O universe + subscription plan
6. WebSocket pool: dynamic instruments from SubscriptionPlan
7. Tick processor task (pure capture: parse → filter junk → persist)
8. axum API server on 0.0.0.0:3001 (health, stats, portal)
9. Token renewal background task
10. Await Ctrl+C → graceful shutdown
```

## Completed Blocks

- **Block 01**: Instrument download, CSV parse, 5-pass universe build, validation
- **Block 01.1**: QuestDB persistence (4 tables via ILP, DEDUP UPSERT KEYS)
- **Block 02**: Auth pipeline (SSM → TOTP → JWT → arc-swap → auto-renew)
- **Block 03**: WebSocket Connection Manager (types, subscription builder, connection lifecycle, pool)
- **Block 04**: Binary Protocol Parser — dispatch_frame() routes 7 packet types
- **Block 05**: Tick Pipeline — pure capture: frame → parse → filter → persist
- **Subscription Planner**: InstrumentRegistry (O(1) lookup) + SubscriptionPlanner (FnoUniverse → filtered instruments)
- **Frontend stripped**: TradingView chart, candle aggregation, candle broadcast all removed — pure tick capture backend
