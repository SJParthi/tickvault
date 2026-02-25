# Tech Stack Bible V6

> **O(1) ALGOTRADING**
>
> 109 Components · 22 Sections
>
> Single source of truth for all versions, components, and architecture.
> If CLAUDE.md and the Bible conflict, the Bible wins.

---

## Section 1 — FRONTEND (4 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 1 | Grafana | 12.0.1 | Dashboards: ticks, P&L, risk, OMS state |
| 2 | Grafana Image Renderer | 3.12.1 | PNG snapshots for Telegram alerts |
| 3 | WireGuard | Latest kernel | VPN tunnel: laptop → EC2 Grafana |
| 4 | Traefik | 3.4.0 | Reverse proxy, TLS termination, blue-green deploy |

---

## Section 2 — BACKEND CORE — RUST (5 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 5 | Rust Edition | 2024 | Language edition (stable 1.85+) |
| 6 | Rust Toolchain | 1.93.1 | Compiler version — pinned, no nightly |
| 7 | tokio | 1.47.0 | Async runtime — full features |
| 8 | tracing | 0.1.41 | Structured logging spans |
| 9 | tracing-subscriber | 0.3.19 | Log formatting: JSON, filtering, layers |

---

## Section 3 — DATA PIPELINE (6 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 10 | tokio-tungstenite | 0.28.0 | WebSocket client for Dhan V2 binary feed |
| 11 | zerocopy | 0.8.25 | Zero-allocation binary parsing of tick packets |
| 12 | rtrb | 0.3.1 | Lock-free SPSC ring buffer — tick pipeline |
| 13 | crossbeam-channel | 0.5.15 | MPMC channel for fan-out routing |
| 14 | arc-swap | 1.7.1 | O(1) atomic pointer swap for token/config |
| 15 | papaya | 0.2.1 | Lock-free concurrent HashMap for hot-path lookups |

---

## Section 4 — DATABASE & CACHE (5 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 16 | QuestDB | 8.3.3 | Time-series DB: tick storage, OHLCV, audit |
| 17 | questdb-rs | 6.1.0 | ILP client: high-throughput row ingestion |
| 18 | Valkey | 8.1.1 | In-memory cache (Redis fork): OMS state, live greeks |
| 19 | redis (crate) | 1.0.4 | Rust client for Valkey — async, connection pool |
| 20 | deadpool-redis | 0.22.1 | Async connection pool for Valkey |

---

## Section 5 — OBSERVABILITY & MONITORING (12 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 21 | Prometheus | 3.4.1 | Metrics store: tick latency, order counts, error rates |
| 22 | metrics | 0.24.2 | Rust metrics facade (counters, gauges, histograms) |
| 23 | metrics-exporter-prometheus | 0.16.2 | Prometheus /metrics HTTP endpoint |
| 24 | Grafana Loki | 3.6.1 | Log aggregation — queryable via Grafana |
| 25 | Grafana Alloy | v1.8.0 | Log collector/shipper — replaces Promtail |
| 26 | Jaeger v2 | 2.6.0 | Distributed tracing — trace every tick, order |
| 27 | opentelemetry | 0.28.0 | OTel API — vendor-neutral tracing interface |
| 28 | opentelemetry_sdk | 0.28.0 | OTel SDK — span processing, export |
| 29 | opentelemetry-otlp | 0.28.0 | OTLP exporter — sends traces to Jaeger v2 |
| 30 | tracing-opentelemetry | 0.29.0 | Bridge: tracing spans → OTel spans |
| 31 | opentelemetry-semantic-conventions | 0.28.0 | Standard attribute names for spans |
| 32 | Grafana dashboards | Custom JSON | Pre-built panels for every metric |

---

## Section 6 — TRADING DOMAIN (9 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 33 | yata | 0.7.0 | Technical indicators (EMA, RSI, MACD, etc.) |
| 34 | blackscholes | 0.8.3 | Options pricing: Black-Scholes IV & greeks |
| 35 | statrs | 0.18.0 | Stats/probability: normal CDF for options |
| 36 | governor | 0.8.0 | Rate limiter (GCRA): 10 orders/sec SEBI limit |
| 37 | statig | 0.3.0 | Type-safe state machine for OMS transitions |
| 38 | arrayvec | 0.7.6 | Stack-allocated Vec: zero heap on hot path |
| 39 | enum_dispatch | 0.3.13 | Zero-cost enum dispatch → vtable-free |
| 40 | rust_decimal | 1.37.1 | Fixed-point decimal: prices, quantities, P&L |
| 41 | DashMap | 6.1.0 | Concurrent HashMap: cold-path only (OMS lookup) |

---

## Section 7 — HTTP CLIENT (1 component)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 42 | reqwest | 0.12.28 | Dhan REST API: auth, orders, positions |

---

## Section 8 — HTTP SERVER (V6 NEW) (3 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 43 | axum | 0.8.1 | HTTP framework: REST API, health, admin |
| 44 | tower | 0.5.2 | Middleware: rate limit, timeout, compression |
| 45 | tower-http | 0.6.6 | HTTP-specific middleware: CORS, tracing, auth |

---

## Section 9 — CONFIGURATION (1 component)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 46 | toml (crate) | 0.8.23 | Parse base.toml + local-overrides.toml |

---

## Section 10 — DATE & TIME (2 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 47 | chrono | 0.4.41 | DateTime, IST timezone, expiry math |
| 48 | chrono-tz | 0.10.3 | IANA timezone database: Asia/Kolkata |

---

## Section 11 — SERIALIZATION & ERRORS (4 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 49 | serde | 1.0.228 | Serialize/deserialize: JSON, TOML, binary |
| 50 | serde_json | 1.0.147 | JSON parsing: Dhan API responses |
| 51 | thiserror | 2.0.17 | Enum-based error types with Display |
| 52 | anyhow | 1.0.98 | Error context chains for propagation |

---

## Section 12 — SYSTEM & RESILIENCE (9 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 53 | signal-hook | 0.3.18 | Unix signal handlers: SIGTERM, SIGINT |
| 54 | signal-hook-tokio | 0.3.2 | Async signal streams for tokio runtime |
| 55 | sd-notify | 0.4.3 | systemd watchdog: heartbeat, ready notification |
| 56 | backon | 1.6.0 | Retry with exponential backoff |
| 57 | failsafe | 1.3.0 | Circuit breaker: trip on N consecutive failures |
| 58 | memmap2 | 0.9.5 | Memory-mapped files: crash recovery state |
| 59 | parking_lot | 0.12.3 | Faster Mutex/RwLock than std (cold path only) |
| 60 | once_cell | 1.21.3 | Lazy static initialization — thread-safe |
| 61 | bytes | 1.10.1 | Zero-copy byte buffer for network I/O |

> **Known discrepancy:** signal-hook-tokio 0.3.2 was never published to crates.io. We use 0.3.1.

---

## Section 13 — TESTING & QUALITY (5 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 62 | criterion | 0.5.1 | Microbenchmarks: tick parse, pipeline latency |
| 63 | proptest | 1.6.0 | Property-based testing: random input generation |
| 64 | loom | 0.7.2 | Concurrency testing: thread interleaving |
| 65 | dhat | 0.3.3 | Heap profiler: verify zero allocation |
| 66 | mockall | 0.13.1 | Mock generation for trait-based testing |

---

## Section 14 — SECURITY & CODE QUALITY (+2 V6) (7 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 67 | secrecy | 0.10.3 | Secret wrapper — Debug prints [REDACTED] |
| 68 | zeroize | 1.8.2 | Wipe secret memory on drop |
| 69 | cargo-audit | 0.21.1 | CVE scanner for dependencies |
| 70 | clippy | Built-in | Lint: -D warnings, perf group enforced |
| 71 | rustfmt | Built-in | Code formatting: cargo fmt --check in CI |
| 72 | totp-rs | 5.7.0 | TOTP 2FA: Dhan mandatory login |
| 73 | bitcode | 0.6.6 | Binary serialization: compact state snapshots |

> **API note:** secrecy 0.10.3 uses `SecretString` (= `SecretBox<str>`), NOT `Secret<T>`.
> Create via `SecretString::from(string)`. `expose_secret()` returns `&str`.

---

## Section 15 — IDE (2 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 74 | IntelliJ IDEA Ultimate | 2025.3 | Primary IDE — Rust plugin, debugging, profiling |
| 75 | Rust Plugin | ID 22407 | IntelliJ Rust support (replaces archived intellij-rust) |

---

## Section 16 — CI/CD PIPELINE (+1 V6) (5 components)

| # | Component | Version | Purpose |
|---|-----------|---------|---------|
| 76 | GitHub Actions | Hosted runners | CI/CD: build, test, lint, deploy |
| 77 | cargo-tarpaulin | 0.32.7 | Code coverage: enforce 90%+ threshold |
| 78 | cargo-fuzz | 0.12.0 | Fuzz testing: crash discovery |
| 79 | git-secrets | 1.3.0 | Pre-commit hook: prevent secret leaks |
| 80 | Docker BuildKit | Enabled | Multi-stage builds: scratch base, <15MB images |

---

## Section 17 — DOCKER IMAGES (+1 V6) (11 components)

All images pinned by SHA256 digest. See `deploy/docker/docker-compose.yml` for
the exact digests in use (deployment versions may differ from Bible originals).

| # | Component | Bible Version | Docker Image |
|---|-----------|--------------|--------------|
| 81 | QuestDB | 8.3.3 | `questdb/questdb:8.3.3@sha256:...` |
| 82 | Valkey | 8.1.1-alpine | `valkey/valkey:8.1.1-alpine@sha256:...` |
| 83 | Grafana | 12.0.1 | `grafana/grafana-oss:12.0.1@sha256:...` |
| 84 | Grafana Image Renderer | 3.12.1 | `grafana/grafana-image-renderer:3.12.1@sha256:...` |
| 85 | Prometheus | v3.4.1 | `prom/prometheus:v3.4.1@sha256:...` |
| 86 | Grafana Loki | 3.6.1 | `grafana/loki:3.6.1@sha256:...` |
| 87 | Grafana Alloy | v1.8.0 | `grafana/alloy:v1.8.0@sha256:...` |
| 88 | Jaeger v2 | 2.6.0 | `jaegertracing/jaeger:2.6.0@sha256:...` |
| 89 | Traefik | 3.4.0 | `traefik:v3.4.0@sha256:...` |
| 90 | LocalStack | 4.4.0 | `localstack/localstack:4.4.0@sha256:...` |
| 91 | Rust (builder) | 1.93.1-slim | `rust:1.93.1-slim@sha256:...` |

> **Deployment reality:** docker-compose.yml has been updated beyond Bible V6
> during development. Actual running versions (from docker-compose.yml):
>
> | Component | Bible V6 | Deployed |
> |-----------|----------|----------|
> | QuestDB | 8.3.3 | 9.3.2 |
> | Valkey | 8.1.1 | 9.0.2 |
> | Grafana | 12.0.1 | 12.3.3 |
> | Prometheus | v3.4.1 | v3.9.1 |
> | Loki | 3.6.1 | 3.6.6 |
> | Alloy | v1.8.0 | v1.8.0 (match) |
> | Jaeger v2 | 2.6.0 | 2.15.0 |
> | Traefik | 3.4.0 | v3.6.8 |
> | LocalStack | 4.4.0 | 4.3.0 |
>
> These should be reconciled in a future Bible update (V7).

---

## Section 18 — ALERTING & NOTIFICATIONS (3 components)

| # | Component | Purpose |
|---|-----------|---------|
| 92 | Telegram Bot API | Instant alerts: WebSocket disconnect, strategy errors, failures |
| 93 | Grafana Contact Points | Metric-based rules: tick latency, queue depth, error rate |
| 94 | AWS SNS | Last-resort SMS fallback: when internet + Telegram are both down |

---

## Section 19 — AWS PRODUCTION (+1 V6) (10 components)

| # | Component | Value | Purpose |
|---|-----------|-------|---------|
| 95 | Instance Type | c7i.2xlarge | x86-64 Sapphire Rapids — 8 vCPU, 16 GB RAM |
| 96 | Region | ap-south-1 | Mumbai. SEBI mandate: servers must be in India |
| 97 | Elastic IP | Static | SEBI mandatory. Free when attached to running instance |
| 98 | Amazon Linux 2023 | AL2023 AMI | Base OS. Native AWS integration, IMDSv2 default |
| 99 | EBS gp3 | 100 GB | OS, Docker images, QuestDB data, application logs |
| 100 | S3 Bucket | Cold storage | Historical tick data archive moved off EBS |
| 101 | SSM Parameter Store | SecureString | Secrets vault: Dhan keys, tokens — zero cost |
| 102 | Amazon Time Sync | NTP | Clock accuracy: must match exchange time |
| 103 | AWS Budgets | Cost alerts | Alert if AWS bill exceeds monthly threshold |
| 104 | CloudWatch Alarms | Auto-recovery | EC2 hardware failure + auto instance recovery |

### AWS Cost Estimates

| Plan | Hourly | Monthly |
|------|--------|---------|
| On-Demand | ~₹30/hr | ~₹21,800/mo |
| Reserved 1yr | ~₹18.5/hr | ~₹14,000/mo |

---

## Section 20 — BACKUP & RECOVERY (1 component)

| # | Component | Purpose |
|---|-----------|---------|
| 105 | EBS Snapshots | Automated daily disk backups — restore if EBS volume fails |

---

## Section 21 — SECURITY HARDENING (3 components)

| # | Component | Purpose |
|---|-----------|---------|
| 106 | AWS CloudTrail | Logs every AWS API call — detect unauthorized access |
| 107 | IMDSv2 enforcement | Blocks EC2 metadata credential theft via SSRF |
| 108 | YubiKey / hardware MFA | Physical security key for AWS root — unhackable remotely |

---

## Section 22 — MARKET DATA (1 component)

| # | Component | Purpose |
|---|-----------|---------|
| 109 | NSE holidays JSON | Hardcoded market calendar — updated annually, zero API dependency |

---

## Component Count Summary

| # | Section | Count |
|---|---------|-------|
| 1 | Frontend | 4 |
| 2 | Backend Core — Rust | 5 |
| 3 | Data Pipeline | 6 |
| 4 | Database & Cache | 5 |
| 5 | Observability & Monitoring | 12 |
| 6 | Trading Domain | 9 |
| 7 | HTTP Client | 1 |
| 8 | HTTP Server (V6 NEW) | 3 |
| 9 | Configuration | 1 |
| 10 | Date & Time | 2 |
| 11 | Serialization & Errors | 4 |
| 12 | System & Resilience | 9 |
| 13 | Testing & Quality | 5 |
| 14 | Security & Code Quality (+2 V6) | 7 |
| 15 | IDE | 2 |
| 16 | CI/CD Pipeline (+1 V6) | 5 |
| 17 | Docker Images (+1 V6) | 11 |
| 18 | Alerting & Notifications | 3 |
| 19 | AWS Production (+1 V6) | 10 |
| 20 | Backup & Recovery | 1 |
| 21 | Security Hardening | 3 |
| 22 | Market Data | 1 |
| | **TOTAL** | **109** |
