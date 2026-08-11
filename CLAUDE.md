# CLAUDE.md — tickvault

> **Authority chain (S6-Step8 — Bible deleted):** `Cargo.toml` workspace deps + `deny.toml` + `dhan_locked_facts.rs` are the executable single source of truth for versions and Dhan facts. This file (`CLAUDE.md`) is the workflow + architecture guide. If neither covers a topic, ASK Parthiban.
>
> **FOREVER CHARTER (auto-loaded every session):** `.claude/rules/project/operator-charter-forever.md` is the operator's permanent binding contract — applies to every Claude Code session, every Cowork task, every branch, every PR, every plan item. Read it FIRST before any work. Contains the 15-row + 7-row guarantee matrix, the 10 Telegram commandments, the honest 100% claim wording, the 11 always-on rules, and the mechanical enforcement chain.

## THREE PRINCIPLES

```
1. Zero allocation on hot path
2. O(1) or fail at compile time
3. Every version pinned
```

Every file, function, config decision must pass all three. No exceptions.

**Honest scope of principle 2 (recorded 2026-08-07 after an 8-agent audit).**
O(1) is VERIFIED on the tick hot path — packet decode is fixed-offset
`from_le_bytes` with no loop and no allocation, proven by DHAT tests that gate
every PR, and instrument lookup is an O(1) composite-key hash. It is **not**
universally true, and this file must not be read as claiming it is.

**The exception list below was INCOMPLETE until 2026-08-09**, when a two-agent
workspace-wide complexity audit found three more. That matters more than the
entries themselves: a partial disclosure reads exactly like a complete one, so
this list is now stated as audited-as-of a date rather than as exhaustive.

| Non-O(1) path | What it actually is |
|---|---|
| `crates/trading/src/in_mem/spot_bar_store.rs` | reads **O(log n)** (`RwLock` + binary search); writes O(log n) typical and **O(n)** on a worst-case memmove; `latest_n()` / `stats()` / `depth_days()` are **O(n)** scans (`latest_n` also allocates). *(**CORRECTED 2026-08-10:** this row claimed an "**O(#slots ≤ 256)** linear scan on EVERY read and write". That scan no longer exists — `spot_bar_store.rs:345` is `RwLock<HashMap<SlotKey, Arc<Slot>>>` and `find_slot` is one hash lookup. The row was scarier than the code. The O(log n) read and the O(n) helpers below are still real.)* |
| `crates/core/src/pipeline/chain_day_store.rs` | reads **O(log n)** under `RwLock` (`BTreeMap`), `latest_minutes()` **O(n)** + allocates. Kept off the decision path — `chain_snapshot::load_chain_snapshot` is the O(1) lock-free read. *(Verified accurate 2026-08-10.)* |
| `crates/core/src/pipeline/tick_gap_detector.rs::scan_silence` | **O(n)** in tracked instruments. Inherent: "which instruments are silent?" is a question about all of them at once. Zero-alloc. *(**CORRECTED 2026-08-10:** this row called it a "cold-path **sweep**", which implies it runs. It has **ZERO production callers** — every `scan_silence` reference in `crates/*/src` is inside its own file's test module. It is dead code, and describing dead code as a running sweep is the false-OK class rule 11 forbids. Either wire it or delete it; until then it is listed here as DEAD, not as a sweep.)* **AMENDED 2026-08-11:** `scan_silence` itself still has zero production callers — that half stands. But the surrounding detector is no longer dormant: the live lane builds a `TickGapDetector` (`dhan_feed_stack.rs:531`), seeds it, and calls `observe` on every tick. So the detector now RECORDS silence and nothing ever ASKS it what it found — a wired sensor with no read-out, which reads greener than dead code did. Wire `scan_silence` to a periodic caller or drop the detector from the lane; carrying it half-connected is the worst of the three. |
| `crates/trading/src/candles/multi_tf_aggregator.rs` slot growth | a single growth step is **O(n)** (the `Vec` copies every existing slot), bounded above by `AGGREGATOR_MAX_SLOTS`. *(**RE-CORRECTED 2026-08-11 — the 2026-08-10 correction is now itself false in BOTH halves.** It said "there is no boot call site" and "the stated mitigation does not exist". There is one: `crates/app/src/dhan_feed_stack.rs:532` calls `MultiTfAggregator::with_capacity`, constructed inside `LiveIngest::new` and driven per tick from the frame drain, spawned from `main.rs`. And the mitigation IS implemented: the boot site pre-sizes, and `slot_index` enforces `AGGREGATOR_MAX_SLOTS` fail-closed with a counter and a coalesced `error!` rather than growing without limit. The code is LIVE, not dormant — reachable when `[feeds] dhan_enabled` AND `TICKVAULT_DHAN_LIVE_FEED=1`, both off today. Recorded twice now in opposite directions, which is the real lesson: this row describes code that moved, and a row asserting "dormant" about a live per-tick path is the exact false-OK the list exists to prevent.)* |
| `crates/trading/src/in_mem/day_ohlc_tracker.rs` | **ADDED 2026-08-10** (found by the multi-agent O(1) audit; was missing from this list). `update_tick` inserts into an uncapped `papaya` map, one entry per unseen `(security_id, exchange_segment)`, **never evicted**. Per-op is O(1); **memory growth is unbounded** — bounded only by caller convention, which is exactly the class `spot_bar_store` fixed with `MAX_SPOT_BAR_SLOTS` on 2026-08-07. Dormant today (no live tick producer), but this is the path that would grow until OOM at a 25,000-instrument scale. |

Each of those files states its own complexity honestly in its own header — this
summary was the stale part, not the code. Per the operator's standing rule, an
inherently non-O(1) step is FLAGGED as such with its constraint and its chosen
alternative; it is never relabelled O(1).
Non-hot-path cold code (boot, daily builds, REST pulls) is not held to
principle 2 at all.

## PROJECT

- **Purpose:** O(1) latency live F&O trading system for Indian markets (NSE)
- **Language:** Rust 2024 Edition (stable 1.95.0)
- **Repo:** `https://github.com/SJParthi/tickvault` (single source of truth)
- **Runtime:** Docker everywhere. Mac (dev) → AWS t4g.medium Mumbai (prod, operator-lock 2026-05-18 in `aws-budget.md`). Same containers, same code, always real AWS SSM.
- **Owner:** Parthiban (architect). Claude Code (builder).

## SESSION PROTOCOL

**Start:** git pull → read CLAUDE.md → read phase doc → git log -20 → Cargo.toml → cargo check → cargo test
**End:** Run `/quality` skill → commit → push → summary.

Do NOT read reference docs (Dhan refs, standards/) at startup. Read them ONLY when implementing that specific topic.

## AUTOMATION-FIRST RULE (MANDATORY, every session)

Before grepping logs, tailing files, or asking "is X broken?" — use the
zero-touch automation. It answers faster, cites the exact proof file, and
never hallucinates.

1. **Health question** ("is anything broken?", "why is depth empty?", "is auth OK?")
   → run `make doctor` (7-section explicit pass/fail) BEFORE reading files.
2. **"Are the guards intact?"** → run `make validate-automation` (30 checks).
3. **Error triage** → `make triage-dry-run` (inspect) → `make triage-execute` (act).
4. **"What's happening right now?"** → the **tickvault-logs MCP** tools are auto-loaded
   from `.mcp.json`. Prefer `mcp__tickvault-logs__summary_snapshot`,
   `tail_errors`, `list_novel_signatures`, `questdb_sql`,
   `run_doctor` over hand-rolled Bash.
5. **"How do I fix error code X?"** → `mcp__tickvault-logs__find_runbook_for_code`
   returns the runbook path in `docs/runbooks/`. Never guess.
6. **Any 100% claim** ("guaranteed", "always", "never") → cite `docs/architecture/guarantees.md`
   and name the proof test. No test cited = claim is not allowed.

If Claude Code / Claude co-work does NOT invoke these tools on a health question
it is breaking this rule — the operator should escalate by pointing at this section.

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
| `instrument_registry.rs` | `InstrumentRegistry` — plain `HashMap` keyed on the composite `(SecurityId, ExchangeSegment)` per I-P1-11 (**corrected 2026-08-07**: this row claimed "papaya concurrent map"; `papaya` appears nowhere in `crates/common/src/instrument_registry.rs` — see `by_composite: HashMap<…>`. `papaya` IS a real workspace dep, used in `core` + `trading`, just not here. Lookup is still O(1); the claim was wrong about the type, not the complexity) |
| `tick_types.rs` | `TickerData`, `QuoteData`, `FullPacketData`, `DepthLevel` |
| `trading_calendar.rs` | Market hours, holiday checks, IST handling |
| `sanitize.rs` | Input sanitization utilities |

### crates/core — Market Data & Infrastructure

| Module | Contains |
|--------|----------|
| `parser/` | Binary packet parsing: header (8-byte), ticker (16B), quote (50B), full (162B), OI (12B), prev_close (16B), disconnect (10B), market_depth (20-level), deep_depth (200-level). All O(1) fixed-offset `from_le_bytes`. |
| `websocket/` | Connection pool (max 5 WS), subscription builder (100 instruments/msg, string SecurityId), TLS (aws-lc-rs), order update WS (JSON, `wss://api-order-update.dhan.co`), depth connection (20-level 4×50 instruments, 200-level 4×1 instrument) |
| `auth/` | Token manager (arc-swap, 24h JWT, 23h refresh), TOTP generator (RFC 6238), secret manager (AWS SSM), token cache (Valkey) |
| `instrument/` | CSV downloader, CSV parser, universe builder (F&O filter), subscription planner, binary cache (rkyv zero-copy), daily scheduler, delta detector, S3 backup, validation, depth strike selector (ATM ± 10), depth rebalancer (60s spot drift check) |
| `pipeline/` | Tick processor (SPSC 65K buffer), candle aggregator (21 timeframes from ticks) |
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

> **Corrected 2026-08-08:** all 7 rows this table previously carried
> (`tick_persistence.rs`, `candle_persistence.rs`, `instrument_persistence.rs`,
> `calendar_persistence.rs`, `materialized_views.rs`, `deep_depth_persistence.rs`,
> `indicator_snapshot_persistence.rs`) named files that **do not exist** — the tick-chain
> modules were deleted in the 2026-07-17 stage-2 dead-WS sweep (PR #1631, recorded in
> `quality/crate-coverage-thresholds.toml`), and the depth/indicator/materialized-view
> writers went with the earlier live-feed retirements. The old table also advertised a
> "zero-alloc hot path" tick ILP writer, which no longer exists in any form (live
> market-data feeds retired 2026-07-13/15) — so it misdescribed the architecture, not just
> the paths. Rows below are the real modules; descriptions are taken from each module's own
> `//!` header. Ratcheted by
> `crates/common/tests/claude_md_codebase_map_guard.rs`.

| File | Contains |
|------|----------|
| `seal_absorption.rs` | Sealed-candle 3-tier absorption pipeline (ring → spill → DLQ) |
| `seal_spill.rs` | Sealed-candle disk-spill primitive (NDJSON) |
| `seal_dlq.rs` | Sealed-candle NDJSON dead-letter queue |
| `seal_writer_loop.rs` / `seal_writer_task.rs` / `seal_writer_runner.rs` | Sealed-candle writer tokio loop + task wiring |
| `ws_frame_spill.rs` | Capture-at-receipt WAL — raw frame appended BEFORE parse/broadcast (feed-parameterized) |
| `spot_1m_rest_persistence.rs` | `spot_1m_rest` table — per-minute spot 1m REST pipeline |
| `option_chain_1m_persistence.rs` | `option_chain_1m` table — per-minute option-chain REST pipeline |
| `option_contract_1m_rest_persistence.rs` | `option_contract_1m_rest` table — per-contract 1m candle leg |
| `rest_fetch_audit_persistence.rs` | `rest_fetch_audit` table — per-fetch forensics for the REST legs |
| `instrument_lifecycle_persistence.rs` | `instrument_lifecycle` + `instrument_lifecycle_audit` (SEBI never-delete) |
| `order_audit_persistence.rs` | `order_audit` table — SEBI 5-year order-lifecycle forensics |
| `order_update_events_persistence.rs` / `position_update_events_persistence.rs` | Broker order/position push events (paper mode) |
| `order_leg_pnl_persistence.rs` / `pnl_audit_persistence.rs` / `cross_fill_audit_persistence.rs` | Order-leg P&L + P&L audit + cross-fill forensics |
| `ws_event_audit_persistence.rs` | `ws_event_audit` table — WebSocket lifecycle audit (AUDIT-WS-01) |
| `partition_manager.rs` / `partition_archive.rs` | QuestDB partition lifecycle + archive→verify→drop retention (S3 cold) |
| `questdb_health.rs` | QuestDB health poller |
| `console_views.rs` | Analyst console views — `ticks_named` + `candles_named` |
| `feed_scoreboard_persistence.rs` / `feed_episode_audit_persistence.rs` | Daily feed scoreboard + feed-episode audit tables |
| `shadow_candle_writer.rs` / `shadow_persistence.rs` / `shadow_seal_columns.rs` | Shadow candle-engine ILP append path |
| `brutex_crossverify_persistence.rs` / `spot_crossverify_persistence.rs` | Cross-verification audit tables |
| `tf_consistency_audit_persistence.rs` | Timeframe-consistency audit table |
| `index_constituency_persistence.rs` | `index_constituency` table (SEBI point-in-time) |
| `lifecycle_reconciler.rs` | Pure `classify_transition` — lifecycle state-transition classification |
| `disk_health_watcher.rs` / `oom_monitor.rs` / `resource_monitor.rs` / `wal_suspension_watcher.rs` | Host + QuestDB resource watchdogs |
| `boot_probe.rs` / `http_client.rs` | Boot-time QuestDB probe + shared HTTP client |

### crates/api — HTTP Server (12 routes)

Post-AWS-lifecycle (PRs #2-#7d, 2026-05-19) the API surface narrowed
to operator/observability endpoints.

> **Corrected 2026-08-10 (audit):** the sentence below is true of `/portal/*`
> SPECIFICALLY, but is STALE read as a global "there is no frontend" claim.
> **Four browser-facing surfaces are live today** (~1,053 lines of JavaScript):
> `crates/api/src/handlers/{dashboard_page,feeds_page,board_page}.rs` and
> `crates/aws-lambdas/src/operator_control_console.html`. They postdate the
> 2026-05-19 retirement and are the legitimate frontend set under the operator's
> "Rust only, except frontend" carve-out. Nothing else qualifies — server-side
> shell and CI JavaScript cannot claim that exemption. The entire `/portal/*` HTML
frontend + `/api/option-chain` + `/api/pcr` + `/api/market/indices`
+ `/api/movers*` + `/api/instruments/*` + `/api/index-constituency*`
routes were retired (replacement: CloudWatch Dashboards / Telegram /
MCP / QuestDB Console). (Grafana was retired in the CloudWatch-only
migration #O1, 2026-05-19.)

| File | Contains |
|------|----------|
| `handlers/health.rs` | `GET /health` — health check |
| `handlers/quote.rs` | `GET /api/quote/{security_id}` — latest tick |
| `handlers/stats.rs` | `GET /api/stats` — QuestDB table counts |
| `handlers/debug.rs` | `GET /api/debug/logs/summary`, `GET /api/debug/logs/jsonl/latest`, `GET /api/debug/spill/status` — MCP read-only observability |
| `middleware.rs` | Auth middleware, request tracing |
| `state.rs` | Shared application state |

### crates/app — Entry Point

| File | Contains |
|------|----------|
| `main.rs` | 15-step boot sequence (see below), shutdown handler |
| `observability.rs` | Prometheus + OpenTelemetry + tracing setup |
| `infra.rs` | Docker health checks, service readiness |
| `trading_pipeline.rs` | Pipeline wiring & channel setup |

## BOOT SEQUENCE

```
CryptoProvider → Config → Observability → Logging →
[Parallel: Notification + Docker health] →
[Parallel: IP verification] →
Auth (cache → SSM → TOTP → JWT) →
[Parallel: QuestDB DDL] →
Universe = LOCKED_UNIVERSE const (4 IDX_I SIDs, no CSV parse) →
WebSocket pool (1 main-feed conn) → Tick processor →
Historical candles (cold path) →
Order update WS (1 conn) → API server →
Token renewal → Shutdown signal
```

Post-AWS-lifecycle (PRs #2-#7b, 2026-05-19): the universe is a static
`LOCKED_UNIVERSE` const in `crates/common/src/locked_universe.rs`
(4 IDX_I SIDs: NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21).
CSV download/parse, Phase 2 stock-F&O dispatcher, depth-20/200 pools,
greeks pipeline, movers pipeline are all DELETED — boot is now a
linear flow with one main-feed + one order-update WebSocket. See
`.claude/rules/project/websocket-connection-scope-lock.md` for the
2-WS lock.

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
Branch protection ON: **All Green** (the ci.yml fan-in over the ENTIRE PR suite) + Build & Verify, Security & Audit, Commit Lint, Secret Scan must pass before merge. Enforced for admins. No direct pushes to main. Auto-merge arms ONLY after All Green succeeds (never at PR open). See `.claude/rules/project/merge-gate-lock-2026-07-04.md`.

## CARGO

- Workspace deps in root Cargo.toml, crates use `{ workspace = true }`
- Exact versions ONLY in workspace `Cargo.toml`. `^`, `~`, `*`, `>=` are BANNED. `cargo update` is BANNED. New dep additions need Parthiban approval.
- `edition = "2024"`, `rust-version = "1.95.0"` in every crate
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
| Tracing | tracing + opentelemetry | 0.1.44 / 0.32.0 |
| Serialization | serde + serde_json | 1.0.228 / 1.0.149 |
| Zero-copy | rkyv | 0.8.15 |
| Auth | arc-swap + totp-rs | 1.9.0 / 5.7.1 |
| Secrets | secrecy + zeroize | 0.10.3 / 1.8.2 |
| AWS | aws-config + aws-sdk-ssm + aws-sdk-sns | 1.8.15 / 1.108.0 / 1.98.0 |
| Config | figment + toml | 0.10.19 / 1.1.0 |
| Concurrent map | papaya | 0.2.4 |
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
make questdb                         # localhost:9000 (QuestDB web console)
# Operator dashboards in prod = AWS CloudWatch Dashboards.
# Grafana / Prometheus / Jaeger were retired in the CloudWatch-only
# migration (#O1/#O3, 2026-05-19); their make targets no longer exist.
```

## TESTING STRATEGY

**Block-scoped by default (S6-Step6).** When you edit code in crate X, you run tests for crate X. Workspace-wide testing (`/full-qa` or `FULL_QA=1`) is reserved for: (a) `crates/common/` changes, (b) explicit operator request, (c) post-merge CI. This is the canonical rule — see `.claude/rules/project/testing-scope.md` for the full algorithm.

**Why scoped is the default:** the 22 test categories below apply to the changed crate. Re-running them on the entire workspace for every diff wastes 10-15 minutes per session and produces no additional signal. The CI pipeline runs the full battery on every PR, so nothing slips through.

| Type | Tool | Where | Purpose |
|------|------|-------|---------|
| Unit | `#[test]` | Inline in src | Pure functions, error cases |
| Integration | `tests/` dirs | Each crate | End-to-end flows, Docker services |
| Property | `proptest` | `crates/core/tests/` | Random input robustness |
| Concurrency | `loom` | `crates/trading/tests/` | Data race detection |
| Zero-alloc | `dhat` | `crates/*/tests/dhat_*.rs` | Hot-path allocation verification |
| Chaos | integration | `crates/storage/tests/chaos_*.rs` | Worst-case failure-mode tick survival |
| Fuzz | `cargo-fuzz` | `fuzz/` | Binary protocol crash testing |
| Mutation | `cargo-mutants` | CI weekly | Test quality verification |
| Sanitizers | ASan + TSan | CI weekly | Memory safety + data races |

**Compile-time enforcement** in every crate's `lib.rs`:
```rust
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
```

**Pre-push gates (12, push-scoped, ~35s typical; full-tree scans run in CI Repo Guards):**
1. `cargo fmt --check`
2. Banned pattern scan
3. Secret scan
4. Test count guard (ratchet — count can only increase)
5. Data integrity guard (price precision, IST timestamp rules)
6. Pub fn test guard (every new pub fn has matching #[test] or // TEST-EXEMPT:)
7. Financial test guard (price/order fns have boundary tests)
8. 22-test type check (scoped to changed crates)
9. Dhan locked facts (8 invariants from support tickets)
10. cargo audit + cargo deny (best-effort, blocks on CVE)
11. **S6-G1 pub-fn wiring guard** — new pub fn must have a call site
12. **S6-G3+G4 boot symmetry guard** — state machines must have a poller; both boot paths must be wired

**Default scope rule (mechanical):**
- Edit in `crates/<X>/` → run `cargo test -p tickvault-<X>`
- Edit in `crates/common/` → escalate to `cargo test --workspace`
- Edit in `.claude/hooks/` → run the hook's own self-test if it has one
- Workspace-wide → only on `/full-qa`, `FULL_QA=1`, or post-merge CI

## CI/CD PIPELINE

(Corrected 2026-07-04 — merge-gate hardening; the previous text was stale.)

**On every PR (all feed the `All Green` fan-in gate; nothing merges without it):**
1. Build & Verify: `cargo fmt --check` + `cargo clippy --workspace --no-deps`
2. Test (common/storage/core/trading/api/app): full per-crate lib + integration suites via nextest — includes DHAT zero-alloc + proptest (DHAT was never post-merge-only; the old claim was wrong)
3. Security & Audit: `cargo deny` + `cargo audit --deny yanked`
4. Commit Lint (PR only): conventional commit format
5. Secret Scan: changed-files scope — blocks .env, AWS keys, private keys, tokens
6. Design-First Wall (PR only): `plan-gate.sh` server-side
7. Deploy Lint: SSM quoting guard
8. Coverage & Perf: `cargo llvm-cov` + ratcheted per-crate floors (`quality/crate-coverage-thresholds.toml`) — **pre-merge since 2026-07-04** (was post-merge-only; skips `dhat_*` tests under coverage instrumentation only)
9. Repo Guards: banned-pattern + data-integrity + O(1)/dedup + boot-symmetry source scans server-side (closes the `--no-verify` bypass class)
10. **All Green**: fan-in job over ALL of the above — the single required merge choke point. Auto-merge arms only after it succeeds.
11. Groww QuestDB E2E: path-filtered PR lane (feed/storage paths only; not in All Green because path-filtered)

**On push to main (post-merge; never cancel-in-progress):**
- The same suite re-runs on the merge commit (incl. Coverage ratchet artifact)
- Benchmarks (budgets in `quality/benchmark-budgets.toml`, 5% regression gate)
- Mutation testing (scoped to changed critical crates; the per-PR mutation lane was removed 2026-07-04 — every PR run died at the 60-min timeout and gated nothing; full sweep is ~18h-class)
- Deploy (path-filtered), Groww E2E (path-filtered)

**Weekly (Monday):**
- Fuzz testing (tick_parser, config_parser)
- Mutation testing full sweep of core/trading/common (MISSED mutants = hard fail)
- Safety net: cargo-careful + AddressSanitizer + ThreadSanitizer
- Secret Scan full-tree sweep (every tracked file, not just diffs)

## LOCAL HOOKS (18 scripts)

**Pre-commit (8 gates):** fmt → banned patterns → data integrity → O(1)/dedup scan → secrets → version pinning → commit msg → typos
**Pre-push (7 fast gates):** fmt → banned patterns → secrets → test count → data integrity → pub fn test guard → financial test guard
**Git hook pre-push (11 gates):** fmt → clippy → test → banned patterns → test count → audit → deny → loom → data integrity → pub fn test guard → financial test guard
**Commit message:** `^(feat|fix|refactor|test|docs|chore|perf|security|ci|build|style|bench|revert)(\([a-z0-9_/-]+\))?: .+`
**Other hooks:** pre-tool-dispatch, auto-save, session-sanity, plan-verify, block-env-files

## DOCKER SERVICES

Post CloudWatch-only migration (#O1/#O2/#O3/#O4, 2026-05-19+) the runtime
is **QuestDB + the tickvault app + AWS CloudWatch ONLY**. The metrics /
dashboards / alerting containers were removed: Grafana (#O1), Alertmanager
(#O2), Prometheus (#O3), and Valkey (#O4). Jaeger and Traefik were retired
earlier. CloudWatch (metrics + logs + alarms + dashboards) is the entire
observability layer in prod.

| Service | Image Version | Port | Purpose |
|---------|--------------|------|---------|
| tv-questdb | 9.3.5 | 9000/8812/9009 | Time-series DB |
| tv-loki | 3.7.1 | 3100 | Log aggregation (Alloy ships logs to CloudWatch in prod) |
| tv-alloy | v1.16.0 | — | Observability collector |

All images pinned with SHA256 digest. Config in `deploy/docker/docker-compose.yml`.

## ENFORCEMENT RULES

Auto-loaded `.claude/rules/` files by directory:
*(2026-07-18: per-path loading is real only for files carrying `paths:` frontmatter; this PR added it to all dhan/ + runbook-class project files — see rules-diet-2026-07-18.md.)*

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

**Metrics (Prometheus):** `tv_tick_processing_duration_ns`, `tv_wire_to_done_duration_ns`, `tv_orders_placed_total`, `tv_daily_pnl`, `tv_websocket_connections_active`
**Traces (OpenTelemetry → Jaeger):** spans on WS reads, parsing, OMS, risk checks, persistence
**Logs (tracing → CloudWatch Logs via CW agent in prod; Loki/Alloy local only):** Structured JSON. The 9 filtered codes (see `deploy/aws/terraform/error-code-alarms.tf`; AGGREGATOR-DROP-01 added 2026-07-09) page via CloudWatch metric-filter alarms → SNS → Telegram; all other coded ERRORs are log-sink-only unless they have their own metric alarm or typed NotificationEvent.

## BENCHMARK BUDGETS

`quality/benchmark-budgets.toml` is the executable source of truth (24+ keys, enforced by `scripts/bench-gate.sh` via the post-merge + nightly `bench.yml` workflow). Headline subset — budget keys match real Criterion IDs mechanically (lowercased, `/`→`_`, substring match):

| Budget key (benchmark-budgets.toml) | Real Criterion bench | Budget |
|-------------------------------------|----------------------|--------|
| dispatch_frame | `dispatch_frame/ticker`, `dispatch_frame/quote` | 10 ns |
| pipeline | `pipeline/batch_100_mixed`, `pipeline/burst_100_ticker` | 100 ns/tick |
| registry_get | `registry/get_hit`, `registry/get_miss` | 50 ns |
| oms_state_transition | `oms/state_transition` | 100 ns |
| is_trading_day | `calendar/is_trading_day` | 50 ns |
| config_toml_load | `config/toml_load` | 10 ms |
| composite_quote_tick_compute_only | `composite/quote_tick_compute_only` | 10 μs |
| composite_quote_tick_full_chain | `composite/quote_tick_full_chain` | 10 μs |

## CONFIGURATION

`config/base.toml` sections: `trading` (incl. nse_holidays), `dhan`, `questdb`, `prometheus`, `websocket`, `network`, `token`, `risk`, `strategy` (**`dry_run = true` by default**), `logging`, `instrument`, `api` (port 3001), `subscription`, `notification`, `observability`, `historical` (the `valkey` section was removed in #O4, 2026-05-24)

`[subscription]` post-AWS-lifecycle (PR #7b): `scope = "indices_4_only"` is the only legal value. The 3 dead `subscribe_*_derivatives` / `subscribe_display_indices` flags have been deleted from `SubscriptionConfig`. `SubscriptionScope` is a single-variant enum — any future scope expansion requires a rule-file edit + new enum variant per `.claude/rules/project/websocket-connection-scope-lock.md`.

Override per environment via `config/{env}.toml` or env vars.

## KEY FILES

| Purpose | Path |
|---------|------|
| Phase 1 spec | `docs/phases/phase-1-live-trading.md` |
| Workspace deps (executable truth) | `Cargo.toml` |
| Dhan API reference | `docs/dhan-ref/*.md` (21 files) |
| Dhan support comms archive | `docs/dhan-support/` (README + TEMPLATE + incidents) |
| Benchmark budgets | `quality/benchmark-budgets.toml` |
| Coverage thresholds | `quality/crate-coverage-thresholds.toml` |
| Docker compose | `deploy/docker/docker-compose.yml` |
| Bootstrap script | `scripts/bootstrap.sh` |
| Pre-push gates | `.claude/hooks/pre-push-gate.sh` |
| Active plan | `.claude/plans/active-plan.md` |
| Codebase map | `docs/architecture/codebase-map.md` |
| Dhan API reference (sole) | `docs/dhan-ref/*.md` (21 files) |

> **DhanHQ agent skill — REMOVED 2026-07-31 (zero-interpreted-language purge).**
> The `.claude/skills/dhanhq/` tree (upstream `github.com/dhan-oss/dhanhq-skills`)
> was a vendor SDK reference written entirely in an interpreted language — 15
> example/helper scripts plus 95 fenced code blocks — and was deleted under the
> operator's 2026-07-31 directive (§0 of
> `.claude/rules/project/rust-only-forever-lock-2026-07-19.md`). It was always
> reference-only: never executed, never permitted into the production Rust order
> path. **`docs/dhan-ref/*.md` (21 files) is now the SOLE Dhan API reference** and
> was already the authority the old note pointed at ("alongside
> `docs/dhan-ref/*.md`"); no API fact is lost with the skill.

## DHAN SUPPORT COMMUNICATIONS

Every technical email to Dhan API support (`apihelp@dhan.co`) MUST be
drafted as a markdown file in `docs/dhan-support/`, committed to git,
and shared with Dhan as a **GitHub rendered link** — never as pasted
plain text in Gmail (proportional font destroys ASCII tables).

**Mandatory workflow** (enforced — see `docs/dhan-support/README.md`):

1. `cp docs/dhan-support/TEMPLATE.md docs/dhan-support/YYYY-MM-DD-<ticket>-<topic>.md`
2. Fill in every `<PLACEHOLDER>` (use `grep -n '<[A-Z_]' <file>`)
3. Commit + push
4. Share the `https://github.com/.../blob/<branch>/docs/dhan-support/<file>.md` URL in the Gmail reply with ONE short line
5. Never paste the markdown body into Gmail directly

**Every support email MUST include:**
- Client ID `1106656882`, Name, UCC `NWXF17021Q`
- Precise contract labels (e.g. `NIFTY-Jun2026-28500-CE`) — NEVER generic (`NIFTY-ATM-CE`)
- SecurityId for every contract cited
- Microsecond IST timestamps
- Verbatim JSON logs in fenced code blocks
- "What works" vs "what fails" table (rules out account/token/IP issues)
- Numbered specific questions (not "please help")
- Diagnostic offer (tcpdump, different SIDs, secondary IP, etc.)

Precise contract labels are already produced by the app logs + Telegram
alerts as of commit `3903193` — so future emails can be drafted straight
from the Telegram alert text with zero manual lookup.

## PLAN ENFORCEMENT

Multi-file tasks (3+ changes) require a plan in `.claude/plans/active-plan.md`:
1. Write plan (Status: DRAFT) → present to user
2. On approval → Status: APPROVED → implement, checking items off
3. Before "done" → `bash .claude/hooks/plan-verify.sh` → Status: VERIFIED
4. After push → archive to `.claude/plans/archive/`

See `.claude/rules/project/plan-enforcement.md` for full protocol.

**Per-wave / per-item guarantee matrix (mandatory):** every wave plan / item /
block in `.claude/plans/active-plan*.md` MUST carry the 15-row + 7-row guarantee
matrix from `.claude/rules/project/per-wave-guarantee-matrix.md` (or
cross-reference it). Mechanically enforced by
`bash .claude/hooks/per-item-guarantee-check.sh` (exit 2 = block) and
`make wave-guarantee-check`. Wave 5 Item 22 wired this gate.

## TOKEN EFFICIENCY

- Never re-read files already in session. Parallelize reads. Keep responses short.
- No filler phrases. No repeating rules back. No essays.
- Cargo.toml is the version source of truth (Bible deleted in S6-Step8). PDFs: NEVER. Reference docs: ONLY when implementing that topic.

## COMPACTION

When compacting, always preserve: (1) list of all modified files (2) test/build results (3) current phase progress (4) unresolved errors or blockers (5) the three principles.

## CURRENT CONTEXT

**Phase:** Phase 1 — Live Trading System → `docs/phases/phase-1-live-trading.md`
**Boot sequence:** CryptoProvider → Config → Observability → Logging → Notification → Auth → QuestDB → Universe → HistoricalCandles → WebSocket → TickProcessor → OrderUpdateWS → API → TokenRenewal → Shutdown
**Codebase size:** ~74K LoC Rust (~61K production, ~14K tests), 158 files, 6 crates
**Test count:** ~7,250 passing tests (unit + integration + proptest + adversarial), 43 integration test files, 8 benchmarks, 2 fuzz targets

**2026-04-24 PR #337 — recent-session pointer:** reconnect hardening
(Fix #3), 09:13 triple-dispatch ratchets (#4), pre-open buffer widened
to 09:00–09:12 (#1/#2), REST `/marketfeed/ltp` fallback module (#5),
stock F&O expiry rollover ≤ 1 trading day (#6), main-feed 0/5 counter
wiring (#7), stale 09:12 comment cleanup (#8), depth-rebalance severity
LOW + title includes swap level(s) (#9/#10). Rule updates live in
`.claude/rules/project/depth-subscription.md` (2026-04-24 Updates +
"Stock F&O Expiry Rollover" section),
`.claude/rules/project/live-market-feed-subscription.md` (2026-04-24
Updates), and `.claude/rules/project/observability-architecture.md`
(clauses 7–9 in "What future sessions MUST NOT do"). Runbook update:
`docs/runbooks/expiry-day.md` → "Stock F&O Expiry Rollover".
