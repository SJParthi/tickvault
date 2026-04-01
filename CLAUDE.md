# CLAUDE.md — dhan-live-trader

> **Authority chain:** Tech Stack Bible V6 > this file > defaults. If neither covers a topic, ASK Parthiban.

## THREE PRINCIPLES

```
1. Zero allocation on hot path
2. O(1) or fail at compile time
3. Every version pinned
```

Every file, function, config decision must pass all three. No exceptions.

## PROJECT

- **Purpose:** O(1) latency live F&O trading system for Indian markets (NSE)
- **Language:** Rust 2024 Edition (stable 1.93.1)
- **Repo:** `https://github.com/SJParthi/dhan-live-trader` (single source of truth)
- **Runtime:** Docker everywhere. Mac (dev) → AWS c7i.2xlarge Mumbai (prod). Same containers, same code, always real AWS SSM.
- **Owner:** Parthiban (architect). Claude Code (builder).

## SESSION PROTOCOL

**Start:** git pull → read CLAUDE.md → read phase doc → git log -20 → Cargo.toml → cargo check → cargo test
**End:** Run `/quality` skill → commit → push → summary.

Do NOT read Bible at startup. Read it ONLY when adding/updating a dependency.

## WORKFLOW

Parthiban = architect. Claude Code = builder. Present plan → wait for approval → execute → show proof.
NEVER execute without approval. NEVER guess versions. Silence != approval.

## CODEBASE STRUCTURE

### Workspace Layout (6 crates)

```
crates/
├── common/     # Shared types, config, constants, errors, enums
├── core/       # WebSocket, binary parser, auth, instruments, pipeline
├── trading/    # OMS, risk engine, indicators, strategy evaluator
├── storage/    # QuestDB persistence, Valkey cache, materialized views
├── api/        # Axum HTTP handlers, middleware, state
└── app/        # Binary entry point, boot sequence, observability
```

**Dependency flow:** `common` ← `core` ← `trading` ← `storage` ← `api` ← `app`

### crates/common — Shared Foundation (10 modules)

| File | Contains |
|------|----------|
| `config.rs` | `AppConfig` (figment + TOML), all config structs |
| `constants.rs` | API URLs, packet sizes, header offsets, limits |
| `error.rs` | `DhanErrorCode` (DH-901..910), `DataApiError` (800..814) |
| `types.rs` | `ExchangeSegment`, `FeedRequestCode`, `FeedResponseCode` |
| `order_types.rs` | `OrderStatus`, `ProductType`, `OrderType`, `TransactionType` |
| `instrument_types.rs` | `InstrumentType`, `ExpiryCode`, `InstrumentRecord` |
| `instrument_registry.rs` | `InstrumentRegistry` (papaya concurrent map) |
| `tick_types.rs` | `TickerData`, `QuoteData`, `FullPacketData`, `DepthLevel` |
| `trading_calendar.rs` | Market hours, holiday checks, IST handling |
| `sanitize.rs` | Input sanitization utilities |

### crates/core — Market Data & Infrastructure

| Module | Contains |
|--------|----------|
| `parser/` | Binary packet parsing: header (8-byte), ticker (16B), quote (50B), full (162B), OI (12B), prev_close (16B), disconnect (10B), market_depth (20-level), deep_depth (200-level). All O(1) fixed-offset `from_le_bytes`. |
| `websocket/` | Connection pool (max 5 WS), subscription builder (100 instruments/msg, string SecurityId), TLS (aws-lc-rs), order update WS (JSON, `wss://api-order-update.dhan.co`) |
| `auth/` | Token manager (arc-swap, 24h JWT, 23h refresh), TOTP generator (RFC 6238), secret manager (AWS SSM), token cache (Valkey) |
| `instrument/` | CSV downloader, CSV parser, universe builder (F&O filter), subscription planner, binary cache (rkyv zero-copy), daily scheduler, delta detector, S3 backup, validation |
| `pipeline/` | Tick processor (SPSC 65K buffer), candle aggregator (21 timeframes from ticks), top movers |
| `historical/` | Candle fetcher (Dhan REST, 90-day chunks), cross-verification |
| `network/` | IP monitor, IP verifier (static IP for order APIs) |
| `notification/` | Telegram alerts (teloxide), event types |
| `index_constituency/` | NSE index composition download, caching, mapping |

### crates/trading — Order Management & Strategy

| Module | Contains |
|--------|----------|
| `oms/` | Engine, API client (`access-token` header, v2 base URL), state machine (10 valid transitions, 26 target), rate limiter (GCRA: 10/sec, 7000/day), circuit breaker (3-state FSM), idempotency (UUID v4), reconciliation (f64::EPSILON) |
| `risk/` | Pre-trade checks (halt → daily loss → position limit), P&L tracker, tick gap detection |
| `indicator/` | O(1) indicator engine (SMA, EMA, RSI, MACD, BB via yata), types |
| `strategy/` | FSM evaluator, TOML config, hot reload (notify crate) |

### crates/storage — Persistence Layer

| File | Contains |
|------|----------|
| `tick_persistence.rs` | QuestDB ILP writer (zero-alloc hot path) |
| `candle_persistence.rs` | Candle storage with O(1) dedup |
| `instrument_persistence.rs` | Instrument master persistence |
| `calendar_persistence.rs` | Trading calendar storage |
| `materialized_views.rs` | QuestDB materialized view DDL |
| `valkey_cache.rs` | Redis/Valkey caching layer |

### crates/api — HTTP Server (10 routes)

| File | Contains |
|------|----------|
| `handlers/health.rs` | `GET /health` — health check |
| `handlers/quote.rs` | `GET /api/quote/{security_id}` — latest tick |
| `handlers/stats.rs` | `GET /api/stats` — QuestDB table counts |
| `handlers/top_movers.rs` | `GET /api/top-movers` — gainers/losers/most active |
| `handlers/instruments.rs` | `POST /api/instruments/rebuild`, `GET /api/instruments/diagnostic` |
| `handlers/index_constituency.rs` | `GET /api/index-constituency`, `GET /api/index-constituency/{index_name}`, `GET /api/stock-indices/{symbol}` |
| `handlers/static_file.rs` | `GET /portal` — DLT Control Panel |
| `middleware.rs` | Auth middleware, request tracing |
| `state.rs` | Shared application state |

### crates/app — Entry Point

| File | Contains |
|------|----------|
| `main.rs` | 15-step boot sequence (see below), shutdown handler |
| `observability.rs` | Prometheus + OpenTelemetry + tracing setup |
| `infra.rs` | Docker health checks, service readiness |
| `trading_pipeline.rs` | Pipeline wiring & channel setup |

## BOOT SEQUENCE (15 steps)

```
CryptoProvider → Config → Observability → Logging →
[Parallel: Notification + Docker health] →
[Parallel: IP verification] →
Auth (cache → SSM → TOTP → JWT) →
[Parallel: QuestDB DDL] →
Universe (CSV → parse → filter → cache) →
WebSocket pool → Tick processor →
Historical candles (cold path) →
Order update WS → API server →
Token renewal → Shutdown signal
```

## KEY ARCHITECTURAL PATTERNS

1. **Binary parsing:** Fixed-offset `from_le_bytes` reads — no loops, no allocation. Constants for all offsets.
2. **Token refresh:** `arc-swap` for lock-free reads during atomic swap. `Secret<String>` for zeroization.
3. **Instrument cache:** `rkyv` zero-copy deserialization. Daily refresh, binary cache on disk.
4. **Rate limiting:** `governor` GCRA algorithm. Dual limits (per-second burst + per-day cumulative).
5. **Pipeline:** SPSC 65,536-buffer async channel. No blocking I/O in hot loop.
6. **State machine:** 10 implemented OMS transitions (26 target). Terminal states block outgoing. Pure function.
7. **Circuit breaker:** 3-state FSM (Closed → Open → Half-Open). `failsafe` crate.

## GIT

```
Branch:   main (single branch until AWS deployment)
Commit:   <type>(<scope>): <description>
Types:    feat, fix, refactor, test, docs, chore, perf, security
```

Every commit compiles + passes tests. One logical change per commit.
Branch protection ON: Build & Verify, Security & Audit, Commit Lint, Secret Scan must pass before merge. Enforced for admins. No direct pushes to main.

## CARGO

- Workspace deps in root Cargo.toml, crates use `{ workspace = true }`
- Exact versions ONLY from Bible. `^`, `~`, `*`, `>=` are BANNED. `cargo update` is BANNED.
- `edition = "2024"`, `rust-version = "1.93.1"` in every crate
- Release profile: `overflow-checks = true`, `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip = "symbols"`

## KEY DEPENDENCIES (pinned versions)

| Category | Crate | Version |
|----------|-------|---------|
| Async | tokio | 1.49.0 |
| WebSocket | tokio-tungstenite | 0.29.0 |
| HTTP client | reqwest | 0.12.15 |
| HTTP server | axum | 0.8.8 |
| Database | questdb-rs | 6.1.0 |
| Cache | redis | 1.1.0 |
| Metrics | metrics + prometheus-exporter | 0.24.3 / 0.18.1 |
| Tracing | tracing + opentelemetry | 0.1.44 / 0.31.0 |
| Serialization | serde + serde_json | 1.0.228 / 1.0.149 |
| Zero-copy | rkyv | 0.8.15 |
| Auth | arc-swap + jsonwebtoken + totp-rs | 1.9.0 / 10.3.0 / 5.7.1 |
| Secrets | secrecy + zeroize | 0.10.3 / 1.8.2 |
| AWS | aws-config + aws-sdk-ssm + aws-sdk-sns | 1.8.15 / 1.108.0 / 1.98.0 |
| Config | figment + toml | 0.10.19 / 1.1.0 |
| Concurrent map | papaya | 0.2.3 |
| Rate limiting | governor | 0.10.2 |
| CLI | clap | 4.6.0 |

## BANNED

Enforcement: `.claude/hooks/` (mechanical, blocks at commit). Rules: `.claude/rules/` (auto-loaded per path).
Quick ref: .env | bincode/Promtail/Jaeger-v1 | ^/~/\*/>=/:latest | brew | localhost | hardcoded values | .clone()/DashMap/dyn on hot | unbounded channels | println!/unwrap | cargo update

## COMMANDS

```bash
# Build & Test
cargo build --workspace              # Debug build
cargo build --release --workspace    # Release build
cargo test --workspace               # All tests
cargo fmt --check                    # Format check
cargo clippy --workspace -- -D warnings -W clippy::perf   # Lint

# Quality
make check                           # fmt + clippy + test
make quality                         # Full CI-equivalent pipeline
make coverage                        # llvm-cov with threshold

# Docker
make docker-up                       # Start all 8 services
make docker-down                     # Stop all services
make docker-status                   # Show container health

# Run
make run                             # Start app (ensures Docker ready)
make stop                            # Kill running app
make health                          # Check app health endpoint

# Benchmarks
make bench                           # cargo bench --workspace
make audit                           # cargo audit + cargo deny

# Dashboards
make grafana                         # localhost:3000
make questdb                         # localhost:9000
make jaeger                          # localhost:16686
make prometheus                      # localhost:9090
```

## TESTING STRATEGY

| Type | Tool | Where | Purpose |
|------|------|-------|---------|
| Unit | `#[test]` | Inline in src | Pure functions, error cases |
| Integration | `tests/` dirs | Each crate | End-to-end flows, Docker services |
| Property | `proptest` | `crates/core/tests/` | Random input robustness |
| Concurrency | `loom` | `crates/trading/tests/` | Data race detection |
| Zero-alloc | `dhat` | `crates/*/tests/dhat_*.rs` | Hot-path allocation verification |
| Fuzz | `cargo-fuzz` | `fuzz/` | Binary protocol crash testing |
| Mutation | `cargo-mutants` | CI weekly | Test quality verification |
| Sanitizers | ASan + TSan | CI weekly | Memory safety + data races |

**Compile-time enforcement** in every crate's `lib.rs`:
```rust
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
```

## CI/CD PIPELINE

**On every push/PR:**
1. Build & Verify (25min): compile → binary size <15MB → fmt → clippy → doc → typos → pattern guards → test
2. Security & Audit (10min): `cargo deny` + `cargo audit --deny yanked`
3. Commit Lint (PR only): conventional commit format
4. Secret Scan: blocks .env, AWS keys, private keys, tokens

**Post-merge only:**
5. Coverage (100% minimum for ALL crates — see `quality/crate-coverage-thresholds.toml`)
6. Benchmarks (budgets in `quality/benchmark-budgets.toml`, 5% regression gate)
7. DHAT zero-allocation (hard fail for core + trading crates)

**Weekly (Monday):**
- Fuzz testing (tick_parser, config_parser)
- Mutation testing (survived mutants = hard fail)
- Safety net: cargo-careful + AddressSanitizer + ThreadSanitizer

## LOCAL HOOKS (18 scripts)

**Pre-commit (8 gates):** fmt → banned patterns → data integrity → O(1)/dedup scan → secrets → version pinning → commit msg → typos
**Pre-push (7 fast gates):** fmt → banned patterns → secrets → test count → data integrity → pub fn test guard → financial test guard
**Git hook pre-push (11 gates):** fmt → clippy → test → banned patterns → test count → audit → deny → loom → data integrity → pub fn test guard → financial test guard
**Commit message:** `^(feat|fix|refactor|test|docs|chore|perf|security)(\([a-z0-9_/-]+\))?: .+`
**Other hooks:** pre-tool-dispatch, auto-save, session-sanity, plan-verify, block-env-files

## DOCKER SERVICES (8 containers)

| Service | Image Version | Port | Purpose |
|---------|--------------|------|---------|
| dlt-questdb | 9.3.2 | 9000/8812/9009 | Time-series DB |
| dlt-valkey | 9.0.2-alpine | 6379 | Cache (Redis replacement) |
| dlt-prometheus | v3.9.1 | 9090 | Metrics |
| dlt-grafana | 12.3.3 | 3000 | Dashboards |
| dlt-jaeger | 2.15.0 | 16686 | Distributed tracing |
| dlt-loki | 3.6.6 | 3100 | Log aggregation |
| dlt-alloy | v1.8.0 | — | Observability collector |
| dlt-traefik | v3.6.8 | 80/443/8080 | API gateway |

All images pinned with SHA256 digest. Config in `deploy/docker/docker-compose.yml`.

## ENFORCEMENT RULES

Auto-loaded `.claude/rules/` files by directory:

### Dhan API rules (21 files in `dhan/`)
| Rule File | Enforces |
|-----------|----------|
| `api-introduction.md` | Base URL v2, `access-token` header, rate limits, DH-904 backoff |
| `authentication.md` | 24h JWT, TOTP, static IP, token never logged, DH-901 rotation |
| `live-market-feed.md` | Binary protocol byte offsets, packet sizes, f32 types, LE reads |
| `full-market-depth.md` | 12-byte header (not 8), f64 prices (not f32), separate bid/ask |
| `historical-data.md` | Columnar arrays, string intervals, 90-day max, non-inclusive toDate |
| `option-chain.md` | PascalCase fields, decimal strike keys, `client-id` header |
| `orders.md` | String securityId, quantity=total on modify, correlationId |
| `annexure-enums.md` | Exact numeric codes, gap at enum 6, no-panic on unknown |
| `instrument-master.md` | Daily refresh, detailed CSV for F&O, derivative IDs unstable |
| `live-order-update.md` | JSON (not binary), MsgCode=42, single-char product codes |
| `market-quote.md` | `client-id` header, 1/sec limit, string keys in response |
| `portfolio-positions.md` | String convertQty, exit-all cancels orders too |
| `funds-margin.md` | `availabelBalance` typo (keep it!), string leverage |
| `traders-control.md` | Kill switch prereqs, P&L exit strings, session-scoped |
| `super-order.md` | 3 leg types, cancel entry = cancel all, trailingJump=0 |
| `forever-order.md` | CNC/MTF only, OCO second leg fields, CONFIRM status |
| `conditional-trigger.md` | Equities/Indices only, indicator names, operators |
| `edis.md` | T-PIN flow, CDSL mandate for holdings sell |
| `postback.md` | Webhook format, snake_case filled_qty |
| `statements.md` | String debit/credit, page 0-indexed, date formats |
| `release-notes.md` | v2 only, breaking changes awareness |

### Project rules (10 files in `project/`)
| Rule File | Enforces |
|-----------|----------|
| `rust-code.md` | Error handling, naming, logging, no hardcoded values, secrets |
| `hot-path.md` | Zero allocation, O(1) constraints, banned hot-path patterns |
| `testing.md` | 22 test categories, coverage, property testing requirements |
| `enforcement.md` | Pre-push gates (7 fast + 11 git hook), scoped testing, gap enforcement |
| `cargo-and-docker.md` | Version pinning, Docker digest, workspace deps |
| `data-integrity.md` | Price precision, f32→f64, dedup keys |
| `market-hours.md` | IST timezone, market hour checks, holiday handling |
| `plan-enforcement.md` | Multi-file plan → verify → archive workflow |
| `gap-enforcement.md` | 31 tracked gaps with mandatory tests |
| `aws-migration.md` | Mac cleanup when deploying to AWS |

## GAP ENFORCEMENT (31 tracked gaps)

Tests in `crates/*/tests/gap_enforcement.rs` verify:
- Instrument gaps (I-P0-01..06, I-P1-01..08): dedup, expiry validation, cache, backup
- OMS gaps (OMS-GAP-01..06): state machine, reconciliation, circuit breaker, rate limit, idempotency, dry-run
- WebSocket gaps (WS-GAP-01..03): disconnect codes, subscription batching, connection state
- Risk gaps (RISK-GAP-01..03): pre-trade checks, P&L tracking, tick gap detection
- Auth gaps (AUTH-GAP-01..02): token expiry, 807 refresh trigger
- Storage gaps (STORAGE-GAP-01..02): segment in dedup key, f32→f64 precision

## OBSERVABILITY

**Metrics (Prometheus):** `dlt_tick_processing_duration_ns`, `dlt_wire_to_done_duration_ns`, `dlt_orders_placed_total`, `dlt_daily_pnl`, `dlt_websocket_connections_active`
**Traces (OpenTelemetry → Jaeger):** spans on WS reads, parsing, OMS, risk checks, persistence
**Logs (tracing → Loki via Alloy):** Structured JSON, ERROR → Telegram alert

## BENCHMARK BUDGETS

| Benchmark | Budget |
|-----------|--------|
| tick_binary_parse | 10 ns |
| tick_pipeline_routing | 100 ns |
| papaya_lookup | 50 ns |
| full_tick_processing | 10 μs |
| oms_state_transition | 100 ns |
| market_hour_validation | 50 ns |
| config_toml_load | 10 ms |

## CONFIGURATION

`config/base.toml` — 17 sections: `trading` (incl. nse_holidays), `dhan`, `questdb`, `valkey`, `prometheus`, `websocket`, `network`, `token`, `risk`, `strategy` (**`dry_run = true` by default**), `logging`, `instrument`, `api` (port 3001), `subscription`, `notification`, `observability`, `historical`

Override per environment via `config/{env}.toml` or env vars.

## KEY FILES

| Purpose | Path |
|---------|------|
| Phase 1 spec | `docs/phases/phase-1-live-trading.md` |
| Tech Stack Bible | `docs/architecture/tech-stack-bible.md` |
| Dhan API reference | `docs/dhan-ref/*.md` (21 files) |
| Benchmark budgets | `quality/benchmark-budgets.toml` |
| Coverage thresholds | `quality/crate-coverage-thresholds.toml` |
| Docker compose | `deploy/docker/docker-compose.yml` |
| Bootstrap script | `scripts/bootstrap.sh` |
| Pre-push gates | `.claude/hooks/pre-push-gate.sh` |
| Active plan | `.claude/plans/active-plan.md` |
| Codebase map | `docs/architecture/codebase-map.md` |

## PLAN ENFORCEMENT

Multi-file tasks (3+ changes) require a plan in `.claude/plans/active-plan.md`:
1. Write plan (Status: DRAFT) → present to user
2. On approval → Status: APPROVED → implement, checking items off
3. Before "done" → `bash .claude/hooks/plan-verify.sh` → Status: VERIFIED
4. After push → archive to `.claude/plans/archive/`

See `.claude/rules/project/plan-enforcement.md` for full protocol.

## TOKEN EFFICIENCY

- Never re-read files already in session. Parallelize reads. Keep responses short.
- No filler phrases. No repeating rules back. No essays.
- Bible: read ONLY when adding deps. PDFs: NEVER. Reference docs: ONLY when implementing that topic.

## COMPACTION

When compacting, always preserve: (1) list of all modified files (2) test/build results (3) current phase progress (4) unresolved errors or blockers (5) the three principles.

## CURRENT CONTEXT

**Phase:** Phase 1 — Live Trading System → `docs/phases/phase-1-live-trading.md`
**Boot sequence:** CryptoProvider → Config → Observability → Logging → Notification → Auth → QuestDB → Universe → HistoricalCandles → WebSocket → TickProcessor → OrderUpdateWS → API → TokenRenewal → Shutdown
**Codebase size:** ~74K LoC Rust (~61K production, ~14K tests), 158 files, 6 crates
**Test count:** ~7,250 passing tests (unit + integration + proptest + adversarial), 43 integration test files, 8 benchmarks, 2 fuzz targets
