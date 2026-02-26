# Codebase Map — dhan-live-trader

> Session start: read THIS + `bible_versions.md`. Skip CLAUDE.md (auto-loads), skip PDFs.
> Updated: 2026-02-26 after Block 02 + 107-issue hardening.

## File Tree

```
dhan-live-trader/
├── Cargo.toml                          # Workspace root — ALL versions pinned
├── CLAUDE.md                           # Project rules (auto-loaded)
├── config/
│   └── base.toml                       # Runtime config (non-secret)
├── crates/
│   ├── common/src/                     # Shared types — imported by all crates
│   │   ├── lib.rs                      # Re-exports: config, constants, error, types, instrument_types
│   │   ├── config.rs                   # ApplicationConfig + validate() — 10 subsections
│   │   ├── constants.rs                # 60+ named constants + compile-time assertions
│   │   ├── error.rs                    # ApplicationError enum (12 variants, thiserror)
│   │   ├── types.rs                    # Exchange, ExchangeSegment, FeedMode, InstrumentType, OptionType
│   │   └── instrument_types.rs         # FnoUniverse, Underlying, DerivativeContract, OptionChain, etc.
│   ├── core/src/                       # Core engine
│   │   ├── lib.rs                      # Re-exports: auth, instrument
│   │   ├── auth/
│   │   │   ├── mod.rs                  # Re-exports auth submodules
│   │   │   ├── secret_manager.rs       # SSM fetch (LocalStack/AWS) → DhanCredentials
│   │   │   ├── totp_generator.rs       # TOTP 6-digit code from base32 secret
│   │   │   ├── token_manager.rs        # JWT lifecycle: acquire, arc-swap store, auto-renew
│   │   │   └── types.rs               # DhanCredentials, TokenState, request/response structs
│   │   └── instrument/
│   │       ├── mod.rs                  # Re-exports instrument submodules
│   │       ├── csv_downloader.rs       # Download Dhan CSV (primary + fallback + cache)
│   │       ├── csv_parser.rs           # Parse 260K rows → 147K RawInstruments
│   │       ├── universe_builder.rs     # 5-pass F&O universe build → FnoUniverse
│   │       └── validation.rs           # Post-build validation (must-exist checks)
│   ├── storage/src/                    # Persistence layer
│   │   ├── lib.rs                      # Re-exports: instrument_persistence
│   │   └── instrument_persistence.rs   # QuestDB ILP writer (4 tables)
│   ├── trading/src/
│   │   └── lib.rs                      # SKELETON — OMS, risk (not started)
│   ├── api/src/
│   │   └── lib.rs                      # SKELETON — axum HTTP server (not started)
│   └── app/
│       ├── src/main.rs                 # SKELETON — prints version, exits
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
│   └── seed-localstack-secrets.sh      # Seeds 5 SSM params in LocalStack
└── docs/
    ├── tech_stack_bible_v6.md          # 109 components (converted from PDF)
    ├── bible_versions.md               # Quick-ref version table (read this first)
    ├── codebase_map.md                 # THIS FILE
    └── phases/
        └── PHASE_1_LIVE_TRADING.md     # Full Phase 1 spec (1,412 lines)
```

## Config Shape (config/base.toml)

```toml
[trading]     # market_open "09:15", market_close "15:30", timezone "Asia/Kolkata"
[dhan]        # websocket_url, rest_api_base_url, instrument_csv_urls, max_instruments
[questdb]     # host "dlt-questdb", http_port 9000, pg_port 8812, ilp_port 9009
[valkey]      # host "dlt-valkey", port 6379, max_connections 16
[prometheus]  # host "dlt-prometheus", port 9090
[network]     # request_timeout_ms 5000, ws_timeout_ms 10000, backoff, retries
[token]       # validity_hours 24, refresh_before_expiry_hours 1
[risk]        # max_daily_loss_percent 2.0, max_position_lots 50
[logging]     # format "json", level "info"
[instrument]  # daily_download_time "08:45", cache_directory "/app/data/"
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
AuthenticationFailed { endpoint, source }
TokenRenewalFailed { attempt, source }
AuthCircuitBreakerOpen(String)
```

## Public APIs by Crate

### common
```rust
// config.rs
ApplicationConfig::load(path) -> Result<Self>
ApplicationConfig::validate(&self) -> Result<()>

// types.rs — enums with as_str() + Display + Serialize/Deserialize
Exchange { NationalStockExchange, BombayStockExchange }
ExchangeSegment { IdxI, NseEquity, NseFnO, BseEquity, BseFnO, McxComm }
FeedMode { Ticker, Quote, Full }
InstrumentType { Equity, Future, Option }
OptionType { Call, Put }

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
token_manager::TokenManager::new(config) -> Self
token_manager::TokenManager::acquire_initial_token(&self) -> Result<()>
token_manager::TokenManager::current_token(&self) -> Option<Arc<TokenState>>
token_manager::TokenManager::spawn_renewal_task(&self) -> JoinHandle<()>
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
instrument_persistence::persist_instrument_snapshot(
    questdb_host, questdb_port, universe: &FnoUniverse, metadata: &UniverseBuildMetadata
) -> Result<()>
```

## Key Types

```rust
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
    client_secret: SecretString,
    totp_secret: SecretString,
}

// TokenState — JWT from Dhan, stored in ArcSwap, Debug redacted
TokenState {
    access_token: SecretString,
    expires_at: DateTime<Utc>,
    issued_at: DateTime<Utc>,
}
```

## Test Counts (211 total)

| Crate | Module | Tests |
|-------|--------|-------|
| common | types | 18 |
| common | config | 16 |
| common | error | 13 |
| common | instrument_types | 17 |
| core | auth/secret_manager | 4 |
| core | auth/totp_generator | 4 |
| core | auth/token_manager | 4 |
| core | auth/types | 11 |
| core | instrument/csv_downloader | 7 |
| core | instrument/csv_parser | 48 |
| core | instrument/universe_builder | 47 |
| core | instrument/validation | 8 |
| storage | instrument_persistence | 17 |
| **Total** | | **211** |

## QuestDB Tables (4)

| Table | Key Columns | Purpose |
|-------|-------------|---------|
| instrument_build_metadata | csv_source, csv_row_count, build_duration_ms | Build audit trail |
| fno_underlyings | security_id, trading_symbol, underlying_kind, lot_size | 215 underlyings |
| derivative_contracts | security_id, trading_symbol, expiry_date, strike_price | ~138K contracts |
| subscribed_indices | security_id, trading_symbol, index_category | 31 indices |

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

## SSM Secrets (5 seeded in LocalStack)

| Path | Status |
|------|--------|
| /dlt/dev/dhan/client-id | Seeded |
| /dlt/dev/dhan/client-secret | Seeded |
| /dlt/dev/dhan/totp-secret | Seeded |
| /dlt/dev/telegram/bot-token | Seeded |
| /dlt/dev/telegram/chat-id | Seeded |

## Completed Blocks

- **Block 01**: Instrument download, CSV parse, 5-pass universe build, validation
- **Block 01.1**: QuestDB persistence (4 tables via ILP)
- **Block 02**: Auth pipeline (SSM → TOTP → JWT → arc-swap → auto-renew)
- **Hardening**: 107 audit issues fixed, 92 new tests added (119 → 211)
