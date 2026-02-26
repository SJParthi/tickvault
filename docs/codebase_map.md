# Codebase Map — dhan-live-trader

> Session start: read THIS file only. Skip CLAUDE.md (auto-loads), skip PDFs.
> Bible: `tech_stack_bible_v6.md` — read ONLY when adding a new dependency.
> Updated: 2026-02-26 after Blocks 04-06 + API + Frontend (full tick-to-TradingView pipeline).

## File Tree

```
dhan-live-trader/
├── Cargo.toml                          # Workspace root — ALL versions pinned
├── CLAUDE.md                           # Project rules (auto-loaded)
├── config/
│   └── base.toml                       # Runtime config (non-secret)
├── crates/
│   ├── common/src/                     # Shared types — imported by all crates
│   │   ├── lib.rs                      # Re-exports: config, constants, error, types, instrument_types, tick_types
│   │   ├── config.rs                   # ApplicationConfig + validate() — 12 subsections
│   │   ├── constants.rs                # 100+ named constants + compile-time assertions + byte offsets
│   │   ├── error.rs                    # ApplicationError enum (12 variants, thiserror)
│   │   ├── types.rs                    # Exchange, ExchangeSegment, FeedMode, InstrumentType, OptionType
│   │   ├── instrument_types.rs         # FnoUniverse, Underlying, DerivativeContract, OptionChain, etc.
│   │   └── tick_types.rs               # Timeframe(27), TickInterval(4), ChartInterval, Candle, CandleBroadcastMessage
│   ├── core/src/                       # Core engine
│   │   ├── lib.rs                      # Re-exports: auth, instrument, websocket, parser, candle, pipeline
│   │   ├── auth/
│   │   │   ├── mod.rs                  # Re-exports auth submodules
│   │   │   ├── secret_manager.rs       # SSM fetch (LocalStack/AWS) → DhanCredentials
│   │   │   ├── totp_generator.rs       # TOTP 6-digit code from base32 secret
│   │   │   ├── token_manager.rs        # JWT lifecycle: acquire (auth.dhan.co), renew (api.dhan.co), arc-swap
│   │   │   └── types.rs               # DhanCredentials, TokenState, DhanGenerateTokenResponse, parse_expiry_time()
│   │   ├── instrument/
│   │   │   ├── mod.rs                  # Re-exports instrument submodules
│   │   │   ├── csv_downloader.rs       # Download Dhan CSV (primary + fallback + cache)
│   │   │   ├── csv_parser.rs           # Parse 260K rows → 147K RawInstruments
│   │   │   ├── universe_builder.rs     # 5-pass F&O universe build → FnoUniverse
│   │   │   └── validation.rs           # Post-build validation (must-exist checks)
│   │   ├── websocket/
│   │   │   ├── mod.rs                  # Re-exports websocket submodules
│   │   │   ├── types.rs               # ConnectionState, DisconnectCode(805-809), WebSocketError, InstrumentSubscription
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
│   │   ├── candle/
│   │   │   ├── mod.rs                 # Re-exports candle submodules
│   │   │   ├── rolling_candle.rs     # O(1) per-tick OHLCV update, IST-aligned boundaries, tick-based candles
│   │   │   └── candle_manager.rs     # Manages all (security_id, timeframe) pairs, time + tick candle aggregation
│   │   └── pipeline/
│   │       ├── mod.rs                 # Re-exports pipeline submodules
│   │       └── tick_processor.rs     # Main loop: frame → parse → candle update → broadcast + persist
│   ├── storage/src/                    # Persistence layer
│   │   ├── lib.rs                      # Re-exports: instrument_persistence, tick_persistence
│   │   ├── instrument_persistence.rs   # QuestDB ILP writer (4 tables)
│   │   └── tick_persistence.rs        # QuestDB ILP writer for ticks table + ensure_tick_table_dedup_keys()
│   ├── trading/src/
│   │   └── lib.rs                      # SKELETON — OMS, risk (not started)
│   ├── api/src/
│   │   ├── lib.rs                      # build_router() — axum router with CORS, static files, API + WS routes
│   │   ├── state.rs                    # SharedAppState (candle_broadcast, questdb_config)
│   │   └── handlers/
│   │       ├── mod.rs                  # Re-exports handler submodules
│   │       ├── health.rs              # GET /health → JSON status + version
│   │       ├── candles.rs             # GET /api/candles/:security_id?timeframe=5m → QuestDB SAMPLE BY
│   │       ├── websocket.rs           # WS /ws/live → subscribe/switch_timeframe, candle_update streaming
│   │       └── static_file.rs        # GET / → serves index.html from embedded static/
│   │   └── static/
│   │       └── index.html             # TradingView Lightweight Charts v5.1.0 — full interval selector
│   └── app/
│       ├── src/main.rs                 # Full orchestration: Config → Auth → WS → Parse → Candle → Persist → HTTP
│       └── examples/
│           └── persist_snapshot.rs      # Integration example: CSV → Universe → QuestDB
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
    ├── bible_versions.md              # Quick-reference active + planned dep versions
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
[instrument]  # daily_download_time "08:45", cache_directory "/app/data/"
[pipeline]    # tick_flush_interval_ms 1000, tick_flush_batch_size 1000, candle_broadcast_capacity 4096
[api]         # host "0.0.0.0", port 3001
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
PipelineConfig { tick_flush_interval_ms, tick_flush_batch_size, candle_broadcast_capacity }
ApiConfig { host, port }

// types.rs — enums with as_str() + Display + Serialize/Deserialize
Exchange { NationalStockExchange, BombayStockExchange }
ExchangeSegment { IdxI, NseEquity, NseFnO, BseEquity, BseFnO, McxComm }
FeedMode { Ticker, Quote, Full }
InstrumentType { Equity, Future, Option }
OptionType { Call, Put }

// tick_types.rs — candle intervals and types
Timeframe { S1, S5, S10, S15, S30, S45, M1..M45, H1..H4, D1, D5, W1, Mo1..Mo6, Y1, Custom(i64) }
Timeframe::as_seconds() -> i64
Timeframe::as_str() -> &'static str        // "1s", "5m", "1h", "1D", etc.
Timeframe::from_str(s) -> Option<Self>
Timeframe::all_standard() -> &'static [Timeframe]  // 27 standard intervals

TickInterval { T1, T10, T100, T1000, Custom(u32) }
TickInterval::tick_count() -> u32
TickInterval::as_str() -> String            // "1T", "10T", etc.
TickInterval::from_str(s) -> Option<Self>
TickInterval::all_standard() -> &'static [TickInterval]  // 4 standard

ChartInterval { Time(Timeframe), Tick(TickInterval) }
Candle { time, open, high, low, close, volume, open_interest, tick_count, security_id, timeframe_str }
CandleBroadcastMessage { candle, security_id, timeframe }

// instrument_types.rs — domain structs
FnoUniverse { underlyings, derivative_contracts, option_chains, subscribed_indices, ... }
Underlying { security_id, trading_symbol, underlying_kind, lot_size, ... }
DerivativeContract { security_id, trading_symbol, instrument_type, expiry_date, ... }
OptionChain { underlying_symbol, expiry_date, sorted_strikes, future_security_id }
SubscribedIndex { security_id, trading_symbol, exchange_segment, index_category, ... }
InstrumentInfo { security_id, trading_symbol, exchange_segment, instrument_type }
UniverseBuildMetadata { csv_source, csv_row_count, build_duration, ... }
```

### core::auth
```rust
secret_manager::fetch_dhan_credentials() -> Result<DhanCredentials>
secret_manager::resolve_environment() -> Result<String>
totp_generator::generate_totp(base32_secret: &str) -> Result<String>

// Token lifecycle — auth.dhan.co for acquire, api.dhan.co for renew
token_manager::TokenManager::initialize(dhan_config, token_config, network_config) -> Result<Self>
token_manager::TokenManager::token_handle(&self) -> TokenHandle
token_manager::TokenManager::spawn_renewal_task(&self) -> JoinHandle<()>

// types.rs — SDK-verified request/response types
DhanCredentials { client_id: SecretString, client_secret: SecretString, totp_secret: SecretString }
TokenState { access_token: SecretString, expires_at, issued_at }
TokenState::from_generate_response(response: &DhanGenerateTokenResponse) -> Self
DhanGenerateTokenResponse { dhan_client_id, access_token, expiry_time: serde_json::Value }
GenerateTokenRequest { dhan_client_id, pin, totp }  // camelCase serialization
parse_expiry_time(value: &Value) -> Option<DateTime>  // handles epoch ms/s, ISO 8601, string
```

### core::parser (Block 04)
```rust
// types.rs — parsed structures
ParsedTick { security_id, exchange_segment_code, last_traded_price, last_trade_quantity,
             exchange_timestamp, received_at_nanos, average_traded_price, volume,
             total_sell_quantity, total_buy_quantity, day_open, day_close, day_high, day_low,
             open_interest, oi_day_high, oi_day_low }
MarketDepthLevel { bid_quantity, ask_quantity, bid_orders, ask_orders, bid_price, ask_price }
PacketHeader { response_code, message_length, exchange_segment_code, security_id }

ParsedFrame {
    Tick(ParsedTick),
    TickWithDepth(ParsedTick, [MarketDepthLevel; 5]),
    OiUpdate { security_id, segment, open_interest },
    PreviousClose { security_id, segment, prev_close, prev_oi },
    MarketStatus { segment, security_id },
    Disconnect(DisconnectCode),
}
ParseError { InsufficientBytes, UnknownResponseCode, InvalidExchangeSegment }

// Parsing functions
dispatch_frame(raw: &[u8], received_at_nanos: i64) -> Result<ParsedFrame, ParseError>
parse_header(raw: &[u8]) -> Result<PacketHeader, ParseError>
parse_ticker_packet(raw: &[u8], header: &PacketHeader, nanos: i64) -> Result<ParsedTick>
parse_quote_packet(raw: &[u8], header: &PacketHeader, nanos: i64) -> Result<ParsedTick>
parse_full_packet(raw: &[u8], header: &PacketHeader, nanos: i64) -> Result<(ParsedTick, [MarketDepthLevel; 5])>
parse_oi_packet(raw: &[u8], header: &PacketHeader) -> Result<(u32, u8, u32)>
parse_previous_close_packet(raw: &[u8], header: &PacketHeader) -> Result<(u32, u8, f32, u32)>
parse_market_status_packet(header: &PacketHeader) -> (u8, u32)
parse_disconnect_packet(raw: &[u8]) -> Result<DisconnectCode>
```

### core::candle (Block 06)
```rust
// rolling_candle.rs — O(1) per-tick aggregation
RollingCandleState::new() -> Self
RollingCandleState::update(ltp, volume, oi, tick_epoch, interval_secs, security_id) -> Option<Candle>
compute_ist_aligned_boundary(epoch_utc: i64, interval_secs: i64) -> i64  // IST 5h30m offset

TickCandleState::new(target_ticks: u32) -> Self
TickCandleState::update(ltp, volume, oi, tick_epoch, security_id) -> Option<Candle>

// candle_manager.rs — manages all (security_id, interval) pairs
CandleManager::new(timeframes: &[Timeframe], tick_intervals: &[TickInterval]) -> Self
CandleManager::process_tick(&mut self, tick: &ParsedTick) -> ArrayVec<Candle, 40>
CandleManager::current_candle(security_id, interval_secs) -> Option<&Candle>  // for live chart updates
```

### core::pipeline (Block 05)
```rust
// tick_processor.rs — main processing loop
run_tick_processor(
    frame_receiver: mpsc::Receiver<Vec<u8>>,
    candle_broadcast: broadcast::Sender<CandleBroadcastMessage>,
    tick_writer: Option<TickPersistenceWriter>,
    timeframes: &[Timeframe],
    tick_intervals: &[TickInterval],
) -> ()  // async — runs until receiver closes
```

### core::websocket
```rust
// types.rs — connection types, errors, protocol constants
ConnectionState { Disconnected, Connecting, Connected, Reconnecting }
DisconnectCode::from_u16(code) -> Self   // 805-809 Dhan SDK codes
DisconnectCode::is_reconnectable(&self) -> bool
DisconnectCode::requires_token_refresh(&self) -> bool
InstrumentSubscription::new(segment, security_id) -> Self
WebSocketError { ConnectionFailed, NoTokenAvailable, DhanDisconnect, ... }

// subscription_builder.rs — JSON message batching
build_subscription_messages(instruments, feed_mode, batch_size) -> Vec<String>
build_unsubscription_messages(instruments, feed_mode, batch_size) -> Vec<String>
build_disconnect_message() -> String

// connection.rs — single WebSocket lifecycle (URL query param auth, server-initiated pings)
WebSocketConnection::new(id, token, client_id, dhan_config, ws_config, instruments, feed_mode, sender) -> Self
WebSocketConnection::run(&self) -> Result<(), WebSocketError>
WebSocketConnection::health(&self) -> ConnectionHealth

// connection_pool.rs — multi-connection manager
WebSocketConnectionPool::new(token, client_id, dhan_config, ws_config, instruments, feed_mode) -> Result<Self>
WebSocketConnectionPool::spawn_all(&mut self) -> Vec<JoinHandle<Result<()>>>
WebSocketConnectionPool::take_frame_receiver(&mut self) -> mpsc::Receiver<Vec<u8>>
WebSocketConnectionPool::health(&self) -> Vec<ConnectionHealth>
```

### core::instrument
```rust
csv_downloader::download_instrument_csv(config) -> Result<Vec<u8>>
csv_parser::parse_instrument_csv(raw_bytes: &[u8]) -> Result<Vec<RawInstrument>>
universe_builder::build_fno_universe(config) -> Result<FnoUniverse>
validation::validate_fno_universe(universe: &FnoUniverse) -> Result<()>
```

### storage
```rust
// instrument_persistence.rs — F&O universe to QuestDB (4 tables)
instrument_persistence::persist_instrument_snapshot(
    universe: &FnoUniverse, questdb_config: &QuestDbConfig
) -> Result<()>

// tick_persistence.rs — live tick data to QuestDB
TickPersistenceWriter::new(config: &QuestDbConfig) -> Result<Self>
TickPersistenceWriter::write_tick(&mut self, tick: &ParsedTick) -> Result<()>
TickPersistenceWriter::flush(&mut self) -> Result<()>
ensure_tick_table_dedup_keys(config: &QuestDbConfig) -> ()  // async — creates ticks + candles_1s tables
```

### api
```rust
// lib.rs — axum router construction
build_router(state: SharedAppState) -> Router

// state.rs — shared application state
SharedAppState::new(candle_tx: broadcast::Sender<CandleBroadcastMessage>, questdb: QuestDbConfig) -> Self

// handlers
GET  /              → TradingView frontend (index.html with Lightweight Charts v5.1.0)
GET  /health        → { "status": "ok", "version": "0.1.0" }
GET  /api/intervals → { "time": ["1s","5s",...,"1Y"], "tick": ["1T","10T","100T","1000T"] }
GET  /api/candles/:security_id?timeframe=5m&from=<epoch>&to=<epoch>  → candle array
WS   /ws/live       → subscribe { action, security_id, timeframe } → candle_update streaming
```

## Key Types

```rust
// ParsedTick — zero-copy parsed from binary WebSocket frame (Block 04)
ParsedTick {
    security_id: u32, exchange_segment_code: u8,
    last_traded_price: f32, last_trade_quantity: u16,
    exchange_timestamp: u32, received_at_nanos: i64,
    average_traded_price: f32, volume: u32,
    total_sell_quantity: u32, total_buy_quantity: u32,
    day_open: f32, day_close: f32, day_high: f32, day_low: f32,
    open_interest: u32, oi_day_high: u32, oi_day_low: u32,
}

// Candle — aggregated OHLCV (Block 06)
Candle {
    time: i64, open: f64, high: f64, low: f64, close: f64,
    volume: u64, open_interest: u32, tick_count: u32,
    security_id: u32, timeframe_str: String,
}

// RawInstrument — parsed from CSV, used only during universe building
RawInstrument {
    security_id: u32, exchange_segment: ExchangeSegment,
    trading_symbol: String, lot_size: u32, tick_size: f64,
    instrument_type: InstrumentType, option_type: Option<OptionType>,
    strike_price: f64, expiry_date: Option<NaiveDate>,
    dhan_instrument_kind: Option<DhanInstrumentKind>,
    custom_symbol: String, underlying_security_id: u32, series: String,
}

// DhanCredentials — secrets from SSM, zeroized on drop
DhanCredentials {
    client_id: SecretString,
    client_secret: SecretString,    // NOTE: this is the 6-digit Dhan trading PIN
    totp_secret: SecretString,
}

// TokenState — JWT from Dhan, stored in ArcSwap, Debug redacted
TokenState {
    access_token: SecretString,
    expires_at: DateTime<FixedOffset>,  // IST timezone
    issued_at: DateTime<FixedOffset>,   // IST timezone
}

// TokenHandle — O(1) atomic read for WebSocket auth header injection
type TokenHandle = Arc<ArcSwap<Option<TokenState>>>;

// ConnectionHealth — monitoring snapshot per WebSocket connection
ConnectionHealth {
    connection_id: u8, state: ConnectionState,
    subscribed_count: usize, total_reconnections: u64,
}
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

CRITICAL: Quote vs Full diverge at offset 34. Quote has OHLC(f32). Full has OI(u32) then OHLC at 46.
Prices are f32 in rupees (NOT paise). No division needed.
```

## Test Counts (483 total)

| Crate | Module | Tests |
|-------|--------|-------|
| common | tick_types | 27 |
| common | config | 21 |
| common | types | 18 |
| common | instrument_types | 17 |
| common | error | 13 |
| core | auth/types | 23 |
| core | auth/secret_manager | 10 |
| core | auth/token_manager | 8 |
| core | auth/totp_generator | 4 |
| core | instrument/universe_builder | 54 |
| core | instrument/csv_parser | 53 |
| core | instrument/validation | 12 |
| core | instrument/csv_downloader | 5 |
| core | websocket/types | 32 |
| core | websocket/subscription_builder | 24 |
| core | websocket/connection_pool | 17 |
| core | websocket/connection | 11 |
| core | parser/dispatcher | 10 |
| core | parser/header | 8 |
| core | parser/quote | 8 |
| core | parser/full_packet | 7 |
| core | parser/disconnect | 7 |
| core | parser/ticker | 6 |
| core | parser/oi | 4 |
| core | parser/types | 3 |
| core | parser/previous_close | 3 |
| core | parser/market_status | 3 |
| core | candle/rolling_candle | 19 |
| core | candle/candle_manager | 14 |
| core | pipeline | 3 |
| storage | instrument_persistence | 25 |
| storage | tick_persistence | 11 |
| api | handlers | 3 |
| **Total** | | **483** |

## QuestDB Tables (6) — DEDUP UPSERT KEYS enabled on all

| Table | UPSERT KEYS | Row Count | Purpose |
|-------|-------------|-----------|---------|
| instrument_build_metadata | timestamp, csv_source | 1 per run | Build audit trail |
| fno_underlyings | timestamp, underlying_symbol | 214 | F&O underlyings |
| derivative_contracts | timestamp, security_id | 96,948 | All derivative contracts |
| subscribed_indices | timestamp, security_id | 31 | Index subscriptions |
| ticks | ts, security_id | live accumulating | Raw tick data (PARTITION BY HOUR) |
| candles_1s | ts, security_id | live accumulating | 1-second candles (PARTITION BY DAY) |

## Docker Services (9 running)

| Container | Image Version | Port | Status |
|-----------|--------------|------|--------|
| dlt-questdb | 9.3.2 | 9000/8812/9009 | Healthy |
| dlt-valkey | 9.0.2-alpine | 6379 | Healthy |
| dlt-prometheus | v3.9.1 | 9090 | Healthy |
| dlt-loki | 3.6.6 | 3100 | Healthy |
| dlt-alloy | v1.8.0 | 12345 | Healthy |
| dlt-jaeger | 2.15.0 | 16686/4317/4318 | Healthy |
| dlt-grafana | 12.3.3 | 3000 | Healthy |
| dlt-traefik | v3.6.8 | 80/443/8080 | Healthy |
| dlt-localstack | 4.3.0 | 4566 | Healthy |

## SSM Secrets (5 — real AWS + LocalStack)

| Path | Status | Note |
|------|--------|------|
| /dlt/dev/dhan/client-id | Seeded | Dhan client ID |
| /dlt/dev/dhan/client-secret | Seeded | 6-digit Dhan trading PIN |
| /dlt/dev/dhan/totp-secret | Seeded | TOTP base32 secret |
| /dlt/dev/telegram/bot-token | Seeded | Telegram bot API token |
| /dlt/dev/telegram/chat-id | Seeded | Telegram chat ID |

## App Boot Sequence (main.rs)

```
1. Load config/base.toml → ApplicationConfig → validate()
2. Initialize tracing (structured logging)
3. Authenticate: SSM → TOTP → POST auth.dhan.co → JWT (graceful: OFFLINE mode if fails)
4. QuestDB: ensure ticks + candles_1s tables with DEDUP UPSERT KEYS
5. WebSocket pool: 5 index instruments (NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY, INDIAVIX)
6. Candle broadcast channel + tick processor task
7. axum API server on 0.0.0.0:3001 (always starts, even in OFFLINE mode)
8. Token renewal background task
9. Await Ctrl+C → graceful shutdown (abort all tasks)
```

## API Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/` | TradingView frontend (index.html) |
| GET | `/health` | Health check + version |
| GET | `/api/intervals` | Available time + tick intervals |
| GET | `/api/candles/:id` | Historical candles from QuestDB |
| WS | `/ws/live` | Real-time candle streaming |

## Frontend — TradingView Lightweight Charts v5.1.0

- Single static HTML file (`crates/api/static/index.html`) — zero build tooling
- Candlestick chart + volume histogram
- Full interval selector (grouped like TradingView):
  - TICKS: 1T, 10T, 100T, 1000T
  - SECONDS: 1s, 5s, 10s, 15s, 30s, 45s
  - MINUTES: 1m, 2m, 3m, 4m, 5m, 10m, 15m, 25m, 30m, 45m
  - HOURS: 1h, 2h, 3h, 4h
  - DAYS+: 1D, 5D, 1W, 1M, 3M, 6M, 1Y
  - Custom interval modal (time + tick)
- Instrument selector dropdown
- Connection status indicator (green/red)
- IST timezone (UTC+5:30)
- Calendar date picker for navigation
- Dark theme matching TradingView Pro

## Completed Blocks

- **Block 01**: Instrument download, CSV parse, 5-pass universe build, validation
- **Block 01.1**: QuestDB persistence (4 tables via ILP, DEDUP UPSERT KEYS)
- **Block 02**: Auth pipeline (SSM → TOTP → JWT → arc-swap → auto-renew)
- **Hardening**: 107 audit issues fixed, 92 new tests added (119 → 211)
- **Test audit**: 37 coverage gaps closed (211 → 250)
- **BSE filter fix**: BSE stock derivatives filtered in Pass 3 + Pass 5, 7 new tests (250 → 257)
- **Bible fix**: Section 1 Frontend corrected to SolidJS (not Grafana), total 113 components
- **Block 03**: WebSocket Connection Manager (types, subscription builder, connection lifecycle, connection pool) — 44 new tests (257 → 301)
- **Block 03 hardening**: 45 coverage gaps closed — types, subscription, connection, pool, config validation (301 → 346)
- **Block 03 protocol fix**: SDK-verified against Dhan Python SDK — 10 critical discrepancies fixed (346 → 347)
- **Block 04**: Binary Protocol Parser — dispatch_frame() routes 7 packet types (ticker/quote/full/OI/prevclose/status/disconnect) — 59 tests (347 → 406)
- **Block 05**: Tick Pipeline — frame → parse → candle update → broadcast + QuestDB persist — 14 tests (406 → 420)
- **Block 06**: Candle Aggregator — O(1) rolling OHLCV, IST-aligned boundaries, 27 time + 4 tick intervals + custom — 33 tests (420 → 453)
- **API + Frontend**: axum server + WebSocket streaming + TradingView chart + full interval selector — 30 tests (453 → 483)
- **Auth fix**: Dhan endpoint corrected (auth.dhan.co, query params, flat camelCase response, 200-OK-error detection)
- **Offline mode**: App starts API server even when auth fails — graceful degradation for development
- **End-to-end verified**: App boots in ~1s, TradingView chart loads at localhost:3001, all 483 tests pass
