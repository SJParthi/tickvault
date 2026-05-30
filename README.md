# tickvault

O(1) latency live F&O trading system for Indian markets (NSE), built in Rust 2024 Edition.

## Three Principles

1. **Zero allocation on hot path**
2. **O(1) or fail at compile time**
3. **Every version pinned**

## Architecture

Binary WebSocket frames from Dhan are parsed in O(1) via fixed-offset `from_le_bytes` reads — no loops, no branching on input size. Parsed ticks flow through a zero-alloc pipeline (junk filter, dedup ring, candle aggregation, top movers) and are persisted to QuestDB via ILP. Instruments are loaded daily from Dhan's CSV feed, filtered to F&O underlyings and derivatives, and cached in a zero-copy rkyv archive.

## Directory Structure

```
tickvault/
├── Cargo.toml              # Workspace root — all versions pinned
├── CLAUDE.md               # Project rules (auto-loaded by Claude Code)
├── config/base.toml        # Runtime config (non-secret)
├── crates/
│   ├── common/             # Shared types, config, constants, errors
│   ├── core/               # Parser, pipeline, WebSocket, auth, notification
│   ├── storage/            # QuestDB persistence (ticks, depth, candles)
│   ├── trading/            # OMS, order types, position tracking
│   ├── api/                # Axum HTTP API + dashboard
│   └── app/                # Binary entrypoint (main.rs boot sequence)
├── docs/                   # Phase docs, codebase map, reference
├── deploy/                 # Docker Compose, Dockerfiles, AWS/CloudWatch config
├── quality/                # Benchmark budgets, quality gates
├── scripts/                # Bootstrap, CI, deployment scripts
└── fuzz/                   # Fuzz testing targets
```

## Quick Start

**Prerequisites:** Rust 1.95.0, Docker, Docker Compose

```bash
git clone https://github.com/SJParthi/tickvault.git
cd tickvault
./scripts/bootstrap.sh
docker compose -f deploy/docker-compose.yml up -d
cargo run
```

## Key Documents

- [`CLAUDE.md`](CLAUDE.md) — Project rules and session protocol
- [`docs/architecture/codebase-map.md`](docs/architecture/codebase-map.md) — Full codebase map with module descriptions
- [`docs/phases/phase-1-live-trading.md`](docs/phases/phase-1-live-trading.md) — Current phase specification
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — Contribution guidelines
- [`SECURITY.md`](SECURITY.md) — Security policy

## Development Security Notes

The `docker-compose.yml` services are configured for **local development only**.
Post the CloudWatch-only migration (#O1–#O4, 2026-05-19) the runtime is
QuestDB + the tickvault app + AWS CloudWatch only; Grafana, Prometheus,
Alertmanager, and Valkey were removed (Traefik/Jaeger retired earlier). The
remaining dev containers:

- **Loki / Alloy:** auth disabled — log pipeline; Alloy ships logs to CloudWatch in prod
- **QuestDB:** credentials loaded from AWS SSM (not hardcoded)

Production hardening is tracked in Phase 2 (AWS deployment).

## License

MIT
