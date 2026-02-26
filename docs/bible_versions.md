# Bible Versions — Active Dependencies

> Extracted from Tech Stack Bible V6. Read this INSTEAD of the PDF/MD.
> Only read the full Bible when adding a NEW dependency not listed here.

## Frontend — Trading Terminal

| Component | Version | Purpose |
|-----------|---------|---------|
| SolidJS | 1.9.11 | O(1) signal-to-DOM, no virtual DOM diffing |
| TradingView LW Charts | v5.1.0 | O(1) candle append, viewport culling, ~45 KB gzip |
| @dschz/solid-lwcharts | 0.4.0 | SolidJS reactive wrapper for TradingView charts |
| DataView + SharedArrayBuffer | ES2020 | O(1) field reads from Dhan binary WebSocket frames |

## Workspace Cargo.toml — Exact Versions

### Backend Core
| Crate | Version | Purpose |
|-------|---------|---------|
| tokio | 1.49.0 | Async runtime (full features) |
| tokio-tungstenite | 0.28.0 | WebSocket client (rustls-tls-native-roots) |
| zerocopy | 0.8.39 | Zero-copy binary parsing (derive) |
| enum_dispatch | 0.3.13 | Static dispatch via jump tables |

### Data Pipeline
| Crate | Version | Purpose |
|-------|---------|---------|
| papaya | 0.2.3 | Hot-path concurrent map |
| dashmap | 6.1.0 | Cold-path concurrent map |
| crossbeam-channel | 0.5.15 | MPMC bounded channel |
| rtrb | 0.3.1 | SPSC wait-free ring buffer |
| ringbuf | 0.4.8 | Ring buffer utilities |

### Database & Cache
| Crate | Version | Purpose |
|-------|---------|---------|
| questdb-rs | 6.1.0 | QuestDB ILP client (chrono_timestamp) |
| redis | 1.0.4 | Valkey client (tokio-comp, connection-manager) |
| deadpool-redis | 0.22.1 | Valkey connection pool |

### Observability
| Crate | Version | Purpose |
|-------|---------|---------|
| metrics | 0.24.3 | Metrics facade |
| metrics-exporter-prometheus | 0.18.1 | Prometheus exporter |
| hdrhistogram | 7.5.4 | HDR histogram for latency |
| quanta | 0.12.5 | High-precision timestamps |
| opentelemetry | 0.31.0 | OTLP tracing |
| opentelemetry_sdk | 0.31.0 | OTLP SDK |
| opentelemetry-otlp | 0.31.0 | OTLP exporter |
| tracing | 0.1.44 | Structured logging facade |
| tracing-subscriber | 0.3.22 | JSON log subscriber (json, env-filter) |
| tracing-opentelemetry | 0.31.0 | Tracing → OTLP bridge |

### Trading Domain
| Crate | Version | Purpose |
|-------|---------|---------|
| arc-swap | 1.7.1 | Atomic pointer swap (token hot-swap) |
| jsonwebtoken | 10.3.0 | JWT decode/validate |
| totp-rs | 5.7.0 | TOTP generation (gen_secret, otpauth) |
| statig | 0.3.0 | OMS state machine |
| blackscholes | 0.24.0 | Options pricing |
| governor | 0.10.2 | SEBI rate limiter (GCRA) |
| failsafe | 1.3.0 | Circuit breaker |
| backon | 1.6.0 | Exponential backoff retry |
| yata | 0.7.0 | Technical indicators |

### HTTP
| Crate | Version | Purpose |
|-------|---------|---------|
| reqwest | 0.12.15 | HTTP client (json, rustls-tls) |
| axum | 0.8.8 | HTTP server (macros) |
| tower | 0.5.2 | Middleware (full) |
| tower-http | 0.6.5 | HTTP middleware (cors, trace, compression-gzip) |

### Configuration & Serialization
| Crate | Version | Purpose |
|-------|---------|---------|
| toml | 1.0.2 | TOML parser |
| figment | 0.10.19 | Config loader (toml, env) |
| chrono | 0.4.41 | Date/time (serde) |
| chrono-tz | 0.10.3 | Timezone support |
| serde | 1.0.228 | Serialization (derive) |
| serde_json | 1.0.149 | JSON serde |
| bitcode | 0.6.9 | Binary codec (replaces banned bincode) |
| csv | 1.3.1 | CSV parser |
| thiserror | 2.0.18 | Typed errors |
| anyhow | 1.0.99 | Error context chain |
| bytes | 1.10.1 | Byte buffer |

### System & Resilience
| Crate | Version | Purpose |
|-------|---------|---------|
| core_affinity | 0.8.3 | CPU pinning |
| memmap2 | 0.9.9 | Memory-mapped crash recovery |
| notify | 8.2.0 | Filesystem watcher |
| signal-hook | 0.3.18 | Signal handlers |
| signal-hook-tokio | 0.3.1 | Async signal handling (Bible says 0.3.2 — never published) |
| sd-notify | 0.4.5 | Systemd watchdog |
| ahash | 0.8.12 | Fast hasher |
| nohash-hasher | 0.2.0 | Identity hasher for integer keys |
| arrayvec | 0.7.6 | Stack-allocated vec |

### Security
| Crate | Version | Purpose |
|-------|---------|---------|
| secrecy | 0.10.3 | Secret wrapping ([REDACTED] Debug) |
| zeroize | 1.8.2 | Memory wiping on drop (derive) |
| aws-config | 1.6.3 | AWS SDK config |
| aws-sdk-ssm | 1.68.0 | SSM Parameter Store client |

### CLI & IDs
| Crate | Version | Purpose |
|-------|---------|---------|
| clap | 4.5.32 | CLI argument parser (derive) |
| uuid | 1.16.0 | Unique IDs (v4, serde) |
| teloxide | 0.13.0 | Telegram bot alerts (macros) |

### Dev/Test
| Crate | Version | Purpose |
|-------|---------|---------|
| criterion | 0.8.2 | Benchmarks (html_reports) |
| proptest | 1.6.0 | Property-based testing |
| loom | 0.7.2 | Concurrency testing |
| dhat | 0.3.3 | Heap allocation profiling |
| tokio-test | 0.4.4 | Tokio test utilities |

## Docker Images (SHA256 pinned)

| Service | Image | Version |
|---------|-------|---------|
| dlt-questdb | questdb/questdb | 9.3.2 |
| dlt-valkey | valkey/valkey | 9.0.2-alpine |
| dlt-prometheus | prom/prometheus | v3.9.1 |
| dlt-loki | grafana/loki | 3.6.6 |
| dlt-alloy | grafana/alloy | v1.8.0 |
| dlt-jaeger | jaegertracing/jaeger | 2.15.0 |
| dlt-grafana | grafana/grafana-oss | 12.3.3 |
| dlt-traefik | traefik | v3.6.8 |
| dlt-localstack | localstack/localstack | 4.3.0 |

## Rust Toolchain
| Component | Version |
|-----------|---------|
| Rust edition | 2024 |
| Rust stable | 1.93.1 |
| Target (dev) | aarch64-apple-darwin |
| Target (prod) | x86_64-unknown-linux-gnu |
