# Deletion Audit — Tickvault Indices-4-Only Cleanup

**Status:** APPROVED (Phase A — docs/rules cleanup ONLY)
**Date:** 2026-05-25 (audit) / 2026-06-10 (Phase A approval)
**Author:** Claude audit pass
**Approved by:** Parthiban, 2026-06-10 (verbatim): "TASK: Execute Phase A (docs/rules cleanup ONLY) of .claude/plans/active-plan-deletion-audit.md. First flip its Status to APPROVED quoting my message authorizing it. Delete ONLY files the audit lists for Phase A; for each, grep the whole repo for inbound references first and paste the zero-hit proof. No crates/ changes in this session. After each batch: full gate run, then ONE PR per the protocol, monitored to merge."
**Phase A execution constraint (operator, 2026-06-10):** whole-file deletions + plan archiving only. Phase A actions requiring `crates/` edits (wave-error-code SECTION deletions that would orphan live `ErrorCode` variants against `error_code_rule_file_crossref.rs`; deletion of `crates/common/tests/loki_alloy_profile_guard.rs`) are DEFERRED to a later session per the "No crates/ changes" constraint.
**Scope:** Identify dead code, deps, tests, rules, docs under the LOCKED runtime scope (4 IDX_I SIDs, 2 WebSockets, QuestDB+CloudWatch only).
**Cross-references:**
- `.claude/plans/aws-lifecycle/deletion-surface-map.md` (older 2026-05-18 plan, partially executed)
- `.claude/plans/active-plan-observability-cloudwatch-only.md` (in-progress)
- `.claude/rules/project/websocket-connection-scope-lock.md`
- `.claude/rules/project/operator-charter-forever.md` §I
- `.claude/rules/project/aws-budget.md` (2026-05-20 CloudWatch-only decision)

---

## Section 1 — Executive summary

- **~35-55K LoC deletable** out of ~214K total Rust LoC across the 6 crates (~16-26%). The bulk is in `crates/trading/` (~28K of ~33K — full OMS/risk/strategy/indicator stack for 0 wired strategies in dry-run) and `crates/core/src/historical/` (gap-fill scheduler + cross-verify infra sized for the deleted F&O universe).
- **~17-25 workspace deps droppable** out of ~50 (~35-50%). Confirmed dead or trivially dead: `dashmap` (1 single-file usage), `metrics-exporter-prometheus`, `opentelemetry*` (3 crates), `tracing-opentelemetry`, `hdrhistogram` (no consumer), `governor`, `failsafe` (no consumer), `jaeckel`, `yata`, `statig`, `backon` (no consumer), `csv` (no consumer), `bitcode` (no consumer), `notify`, `signal-hook`. Multiple `tower-http` features + AWS SDK feature flags retire.
- **2 crates mergeable into others.** `tickvault-api` (5,040 LoC + 1,177 test LoC, 4 surviving handlers) and `tickvault-trading` (32,827 LoC of production, ~80% unused) can each be merged into `app` (or `core`) and slimmed. Conservative path: keep 6 crates, just delete unused modules.
- **~80-100 test files deletable** out of 132 .rs test files across crates. Includes ~30 ratchet/meta-guards already protecting deleted code (`indices4only_scope_lock_guard.rs` etc. — anti-regression value for 1-2 months, then delete).
- **~40-60 .md rule + doc files deletable.** ~10 Dhan REST rules for endpoints never called (kill switch, EDIS, conditional triggers, forever orders, super orders, statements, postback, funds/margin, portfolio-positions, traders-control). ~15 wave-N-error-code rule sections whose underlying subsystems are deleted. ~20 architecture/analysis docs superseded by the LOCKED design.

---

## Section 2 — Workspace deps (root `Cargo.toml`)

Each entry classified KEEP / DROP / INVESTIGATE with one-line reason. Usage confirmed via `grep -rln 'use <crate>\|<crate>::' /home/user/tickvault/crates/*/src/`.

### Async + WebSocket + parsing (Section 2)
| Dep | Verdict | Reason |
|---|---|---|
| `tokio` | KEEP | Core runtime, used everywhere. |
| `tokio-tungstenite` | KEEP | The 2 allowed WebSockets. |
| `futures-util` | KEEP | Stream combinators on WS reads. |
| `zerocopy` | KEEP | Binary packet parsing (Ticker/Quote/Full). |
| `enum_dispatch` | INVESTIGATE | Confirm if still used in OMS/parser dispatch; drops with OMS slim. |

### Data pipeline (Section 3)
| Dep | Verdict | Reason |
|---|---|---|
| `papaya` | KEEP | Live use in 14 hot-path files (caches, registries, aggregators). |
| `dashmap` | DROP | Single usage: `crates/trading/src/in_mem/day_ohlc_tracker.rs`. Replace with `papaya` (already pulled) or `parking_lot::Mutex<HashMap>` (already pulled). |
| `crossbeam-channel` | INVESTIGATE | Verify hot-path usage; mpsc is via `tokio::sync::mpsc`. |
| `rtrb` | INVESTIGATE | Real-time ring buffer — check if rescue ring uses it. |
| `ringbuf` | INVESTIGATE | Likely the rescue ring; KEEP if `tick_persistence.rs` uses it. |
| `parking_lot` | KEEP | Per-cell aggregator mutex. |

### Database (Section 4)
| Dep | Verdict | Reason |
|---|---|---|
| `questdb-rs` | KEEP | Sole persistence target. |

### Observability — Prometheus / OTLP (Section 5) — biggest single deletion
| Dep | Verdict | Reason |
|---|---|---|
| `metrics` | INVESTIGATE | Used by `metrics::counter!` macros widely. Keep as facade until CloudWatch migration ships. |
| `metrics-exporter-prometheus` | DROP | Only used in `crates/app/src/observability.rs` to run Prometheus scrape endpoint. CloudWatch-only operator decision 2026-05-20 retires this. |
| `hdrhistogram` | DROP | Not referenced anywhere in `crates/*/src` — dead dep. |
| `quanta` | INVESTIGATE | High-precision clock — likely only inside `metrics-exporter-prometheus`; drops transitively. |
| `opentelemetry` | DROP | Only in `observability.rs` for OTLP exporter — CloudWatch replaces this. |
| `opentelemetry_sdk` | DROP | Same. |
| `opentelemetry-otlp` | DROP | Same. The `grpc-tonic` feature pulls `tonic` + `prost` (huge). |
| `tracing-opentelemetry` | DROP | OTLP layer; replace with CloudWatch tracing-subscriber writer. |
| `tracing` | KEEP | Core logging facade. |
| `tracing-subscriber` | KEEP, slim features | Drop `json`/`env-filter` features only if not needed under CW. |
| `tracing-appender` | KEEP | Hourly errors.jsonl writer; survive transition. |

### Trading domain — Auth
| Dep | Verdict | Reason |
|---|---|---|
| `arc-swap` | KEEP | Lock-free JWT swap in token manager. |
| `jsonwebtoken` | KEEP | JWT parsing for token validity. |
| `totp-rs` | KEEP | TOTP for token generation. |

### Trading domain — OMS & Risk (Section 6) — questionable under dry-run, 0 strategies
| Dep | Verdict | Reason |
|---|---|---|
| `statig` | DROP | OMS state machine; if OMS is deleted, statig is gone. |
| `jaeckel` | DROP | Black-Scholes solver — only useful for greeks (already deleted). |
| `governor` | DROP | Single usage: `trading/src/oms/rate_limiter.rs` (10 orders/sec rate limiter). Dead when OMS is dry-run-only. |
| `failsafe` | DROP | Not referenced in `crates/*/src` — dead dep. |
| `backon` | DROP | No `use backon` under crates/src — dead dep. |
| `yata` | DROP | Technical indicators — only used by `trading/src/indicator/`. With 0 live strategies, indicator engine is dead weight. |

### HTTP client + server (Section 7-8)
| Dep | Verdict | Reason |
|---|---|---|
| `reqwest` | KEEP | Dhan REST + QuestDB ILP-over-HTTP. |
| `axum` | KEEP | 4 surviving API handlers. |
| `tower` | KEEP | Axum middleware. |
| `tower-http` | DROP `compression-gzip` + `fs` features | `fs` was for the deleted `/portal/*` static-file serving. Keep `cors` + `trace`. |

### Config + time (Section 9-10)
| Dep | Verdict | Reason |
|---|---|---|
| `toml` | KEEP | `config/base.toml` parsing. |
| `figment` | INVESTIGATE | Confirm if used vs raw `toml` — likely keep for env-var overlay. |
| `chrono` | KEEP | IST math everywhere. |
| `chrono-tz` | INVESTIGATE | Used vs raw `IST_UTC_OFFSET_SECONDS` constant? Likely drop. |

### Serialization + errors (Section 11)
| Dep | Verdict | Reason |
|---|---|---|
| `serde` / `serde_json` | KEEP | Dhan REST + WS auth payloads. |
| `bitcode` | DROP | Not referenced in `crates/*/src` — confirmed dead. |
| `rkyv` | INVESTIGATE/likely DROP | 4 file usages (`instrument_registry.rs`, `instrument_types.rs`, `types.rs`, `subscription_planner.rs`) — were for the rkyv binary universe cache that `crates/core/src/instrument/mod.rs` comments say is RETIRED (PR #6b). Drop after auditing each `Archive` derive. |
| `csv` | DROP | Zero `use csv` in `crates/*/src/` — confirmed dead (CSV universe builder deleted PR #6b). |
| `thiserror` | KEEP | Error enums everywhere. |
| `anyhow` | KEEP | App-level errors. |
| `bytes` | KEEP | WS frames + ILP payloads. |

### System + resilience (Section 12)
| Dep | Verdict | Reason |
|---|---|---|
| `core_affinity` | INVESTIGATE / likely DROP | Wave-5 Item 6 pins Tokio workers to CPUs. On t4g.medium (2 vCPUs) pinning 4 workers is a no-op. |
| `memmap2` | INVESTIGATE | Find consumers — if only for rkyv mmap, drops with rkyv. |
| `notify` | DROP | 2 consumers: `core/src/websocket/activity_watchdog.rs` (verify) and `trading/src/strategy/hot_reload.rs` (dead with 0 strategies). |
| `signal-hook` | DROP | Comment in Cargo.toml says retained for "Windows-only code path" — drop, we don't run Windows. |
| `sd-notify` | KEEP | systemd notifier — useful on AWS. |
| `ahash` | KEEP | Faster HashMap. |
| `nohash-hasher` | KEEP | u32 keys (SID hashing). |
| `arrayvec` | KEEP | Bounded telemetry samples. |

### Security + TLS + AWS
| Dep | Verdict | Reason |
|---|---|---|
| `secrecy` / `zeroize` | KEEP | JWT + TOTP secret handling. |
| `rustls` / `rustls-native-certs` | KEEP | TLS for Dhan WS + REST. |
| `aws-config` / `aws-sdk-ssm` / `aws-sdk-sns` | KEEP | SSM secret fetch + SNS alerts. Slim features: drop `sso` + `credentials-process` (instance role-only on prod). |

### CLI + IDs + Telegram + dev (Sections 13/16-18)
| Dep | Verdict | Reason |
|---|---|---|
| `clap` | KEEP | `tv_doctor` + main bin args. |
| `uuid` | INVESTIGATE | OMS idempotency keys — if OMS retired, drop. |
| `teloxide` | KEEP | Telegram routing (still in scope until full CloudWatch+SNS replacement ships). |
| `criterion` / `proptest` / `loom` / `dhat` / `tokio-test` | KEEP (dev) | Test infrastructure stays. |

**Net workspace dep DROPs (high-confidence):** `dashmap`, `metrics-exporter-prometheus`, `hdrhistogram`, `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `tracing-opentelemetry`, `bitcode`, `csv`, `notify`, `signal-hook`, `failsafe`, `backon`, `yata`, `statig`, `governor`, `jaeckel` → **17 deps** (each pulls ~5-30 transitive deps; net Cargo.lock reduction est. 50-120 unique crates from the current 468).

---

## Section 3 — Crates (workspace members)

LoC counts from `find <crate>/src -name '*.rs' | xargs wc -l`.

### 3.1 `crates/common/` — 22,062 LoC (prod) / 9,212 (tests)

Foundation crate. Mostly KEEP, but several bloat hotspots:

| File | LoC | Verdict | Reason |
|---|---|---|---|
| `constants.rs` | 3,866 | INVESTIGATE / partial DROP | Contains constants for ALL deleted subsystems (`INTRADAY_TIMEFRAMES` for 5 TFs not really needed when only 1m+5m matter; depth, movers, Phase 2 constants). Estimate 30-40% deletable. |
| `instrument_types.rs` | 3,697 | INVESTIGATE / partial DROP | Includes types for `FUTIDX`/`OPTIDX`/`FUTSTK`/`OPTSTK`/`FUTCOM`/`OPTFUT`/`FUTCUR`/`OPTCUR`. Under LOCKED scope only `INDEX` matters. Could collapse to ~500 LoC. |
| `config.rs` | 2,821 | INVESTIGATE / partial DROP | `SubscriptionScope` is single-variant; depth/movers/Phase 2 config blocks are dead. Estimate 50% deletable. |
| `instrument_registry.rs` | 1,685 | KEEP, simplify | 4-SID registry; composite-key uniqueness (I-P1-11) ratchet must stay. |
| `error_code.rs` | 1,383 | INVESTIGATE | ~120 ErrorCode variants; many reference deleted subsystems (Depth20Dyn*, MOVERS-*, Phase2-*, etc.). Drop ~30-40 variants whose runbook references deleted code. |
| `trading_calendar.rs` | 1,342 | KEEP | NSE calendar/holidays. |
| `tick_types.rs` | 1,042 | KEEP, simplify | Includes types for `DepthLevel`, `FullPacketData` — Quote-mode IDX_I doesn't need DepthLevel parsing. |
| `types.rs` | 975 | KEEP | `ExchangeSegment`, feed enums. |
| `sanitize.rs` | 814 | KEEP | Input sanitization. |
| `order_types.rs` | 741 | DROP if OMS deleted | Dead with OMS slim-down. |
| `option_chain_schedule.rs` | 509 | KEEP | Index option chain REST is the strategy's primary data input. |
| `open_price_rest_fallback.rs` | 501 | INVESTIGATE / KEEP | Pre-market REST LTP fallback for index 09:15 open price. Likely KEEP for the 4 IDX_I. |
| `phase.rs` | 408 | INVESTIGATE | If Phase 2 retired, this Phase enum may be dead. |
| `market_hours.rs` | 396 | KEEP | NSE hours. |
| `open_price_source.rs` | 378 | KEEP | Index 09:15 open price source ledger. |
| `formulas.rs` | 359 | KEEP/INVESTIGATE | Likely percentage / ATM-strike math. |
| `locked_universe.rs` | 285 | KEEP (canonical) | The 4-SID `LOCKED_UNIVERSE` const — THE truth. |
| `price_precision.rs` | 146 | KEEP | f64 vs f32 routing. |
| `segment.rs` | 126 | KEEP | ExchangeSegment helper. |

**Common DROP candidate ~LoC:** 4-7K via `instrument_types.rs` collapse + `constants.rs` cleanup + `config.rs` dead-block removal + `order_types.rs` if OMS goes.

### 3.2 `crates/core/` — 72,393 LoC (prod) / 12,943 (tests)

| Subsystem | Files | LoC | Verdict | Reason |
|---|---|---|---|---|
| `auth/` | `token_manager.rs` 3,194, `types.rs` 1,459, `secret_manager.rs` 1,161, `token_cache.rs` 943, `mid_session_watchdog.rs` 541 | ~7,300 | KEEP | All needed for Dhan JWT lifecycle. |
| `cross_verify/` | `comparator.rs` 436 | ~456 | KEEP | Active scheduler (boot log shows 15:31 IST + 08:05 IST runs). |
| `historical/` | `candle_fetcher.rs` 5,093, `cross_verify.rs` 4,652, `gap_fill_scheduler.rs` 1,149, `cross_verify_report.rs` 597, `post_open_cross_check.rs` 537, others ~900 | ~12,900 | INVESTIGATE / large slim | 90-day historical for 4 SIDs × ~5 TFs ≈ ~7,500 candles, not 117,624. Module is sized for FullUniverse F&O. `candle_fetcher.rs` likely has dead F&O / movers / depth branches. Estimate 40-60% deletable (5-7K LoC). |
| `instance_lock.rs` | 827 | KEEP | Valkey-removed dual-instance lock via SSM. |
| `instrument/` | `subscription_planner.rs` 4,764, `preopen_price_buffer.rs` 790, `slo_score.rs` 546, `market_open_self_test.rs` ~300, others ~200 | ~6,600 | INVESTIGATE / huge slim | `subscription_planner.rs` already had F&O stock branches gutted (per `mod.rs` comments), but the file is still 4,764 LoC. For 4 SIDs the planner is ~50 LoC: emit the LOCKED_UNIVERSE in 1 subscribe message. Estimate 80% deletable in this file alone (~3.8K LoC). `slo_score.rs` 6-dimension score includes `depth_20_health` / `depth_200_health` / `phase2_health` — drop those dimensions. |
| `network/` | `ip_verifier.rs` 1,587, `ip_monitor.rs` 1,274 | ~2,860 | KEEP | Dhan static IP enforcement. |
| `notification/` | `events.rs` 6,124, `service.rs` 2,267, `coalescer.rs` 722, `summary_writer.rs` 719 | ~9,830 | INVESTIGATE / massive slim | `events.rs` 6,124 LoC has 150+ `NotificationEvent` variants; many for retired subsystems (DepthRebalance*, Phase2*, Movers*, Greeks*, MidMarketBoot*, Depth20Dyn*). Estimate 30-40% deletable. |
| `option_chain/` | `snapshot_scheduler.rs` 1,365, `client.rs` 789, `types.rs` 527, others ~1,300 | ~4,000 | KEEP (core of strategy) | This IS the strategy's primary data source per `docs/architecture/option-chain-z-plus-heart-piece.md`. |
| `parser/` | `full_packet.rs` 761, `dispatcher.rs` 623, `order_update.rs` 517, `types.rs` 506, `quote.rs` 504 + others | ~3,500 | KEEP, but verify | Full-packet parser (162 B) is unused under IDX_I Quote-mode (50 B). DROP `full_packet.rs` IF no NSE_EQ/NSE_FNO subscriptions remain. Order-update parser stays. |
| `pipeline/` | `tick_processor.rs` 4,585, `tick_gap_detector.rs` 632, `tick_enricher.rs` 588, `prev_oi_cache.rs` 540, others ~1,900 | ~8,250 | INVESTIGATE / slim | `tick_processor.rs` 4,585 LoC sized for 25K-SID FullUniverse. For 4 SIDs, ~500 LoC suffices. `prev_oi_cache.rs` / `volume_monotonicity_guard.rs` were Wave-5 movers / cross-verify infra — partly dead. Estimate 50% deletable (~4K LoC). |
| `scheduler/mod.rs` | 745 | KEEP | Market-hours `SchedulePhase` + `StorageGate`. |
| `websocket/` | `connection.rs` 4,841, `connection_pool.rs` 2,031, `order_update_connection.rs` 1,711, `types.rs` 1,365, `activity_watchdog.rs` 643, `subscription_builder.rs` 617, others ~700 | ~11,900 | INVESTIGATE / big slim | `connection_pool.rs` 2,031 LoC sized for 5 main-feed conns; per `websocket-connection-scope-lock.md` pool size is constant 1. Most pool/round-robin/distribution machinery is dead. `connection.rs` 4,841 LoC has depth-20/depth-200 branches that should be gone. Estimate 50% deletable (~5-6K LoC). |

**Core DROP candidate ~LoC:** **20-25K** (huge). Bulk in `subscription_planner.rs`, `tick_processor.rs`, `connection_pool.rs`, `events.rs`, `candle_fetcher.rs`.

### 3.3 `crates/trading/` — 32,827 LoC (prod) / 8,249 (tests) — biggest single deletion target

| Module | LoC | Verdict | Reason |
|---|---|---|---|
| `oms/` (10 files: `api_client.rs` 4,965, `engine.rs` 3,297, `types.rs` 2,759, `reconciliation.rs` 750, `circuit_breaker.rs` 707, `state_machine.rs` 485, `idempotency.rs` 174, `rate_limiter.rs` 123, `dh904_backoff.rs` 168, `mod.rs` 47) | ~13,500 | INVESTIGATE / mostly DROP | OMS is unused under dry-run with 0 strategies wired. `trading_pipeline.rs` references OMS imports but the strategy → OMS edge fires zero times. KEEP the api_client only if needed for `orphan_position_watchdog.rs` (calls `GET /v2/positions`). 90% deletable until Phase 1 live trading. |
| `risk/` (4 files: `tick_gap_tracker.rs` 1,796, `engine.rs` 1,233, `types.rs` 282, `mod.rs` 20) | ~3,330 | INVESTIGATE / mostly DROP | Risk engine has no consumer in dry-run. `tick_gap_tracker.rs` 1,796 LoC overlaps with `crates/core/src/pipeline/tick_gap_detector.rs` 632 LoC — pick one. |
| `indicator/` (5 files: `engine.rs` 1,436, `types.rs` 968, `obi.rs` 576, `tests.rs` 463, `mod.rs` 13) | ~3,450 | INVESTIGATE / DROP | Pulls in `yata`. With 0 wired strategies the indicator engine produces no signal. DROP entirely; reintroduce when first strategy lands. |
| `strategy/` (7 files: `evaluator.rs` 2,089, `config.rs` 1,425, `hot_reload.rs` 1,081, `types.rs` 786, `tests.rs` 515, `config_tests.rs` 459, `mod.rs` 20) | ~6,375 | INVESTIGATE / DROP | 0 strategies wired in `config/base.toml`. Pulls in `notify` for hot-reload. DROP until first strategy lands. |
| `candles/` (8 files: `aggregator_cell.rs` 795, `multi_tf_aggregator.rs` 591, `tf_index.rs` 542, `seal_ring.rs` 459, `pct_stamping.rs` 401, `boundary_calc.rs` 360, `heartbeat.rs` 296, `mod.rs` 39) | ~3,480 | KEEP, but slim TF count | `tf_index.rs` declares `TF_COUNT = 21` — operator wants 1m + 5m only (2 TFs). Reducing eliminates ~19 candle tables, ~19 mutex slots per instrument, ~19 ILP senders. Saves ~30% of this subsystem. |
| `in_mem/` (7 files: `day_ohlc_tracker.rs` 599, `bar_cache.rs` 536, `tick_storage.rs` 474, `prev_day_cache.rs` 256, `consumer.rs` 168, `reset_scheduler.rs` 170, `mod.rs` 61) | ~2,260 | KEEP | RAM-first hot path (per `aws-budget.md` rule 9). |
| `orphan_position_watchdog.rs` | 383 | KEEP | 15:25 IST safety net; needs OMS api_client subset. |

**Trading DROP candidate ~LoC:** **18-22K** (oms + risk + indicator + strategy minus the slice of api_client `orphan_position_watchdog` needs).

### 3.4 `crates/storage/` — 19,312 LoC (prod) / 5,259 (tests)

Storage is QuestDB-only. Audit tables referenced in rule files don't exist as code writers (no `*_audit_persistence.rs` files — confirmed by `ls crates/storage/src/*audit*` returning empty).

| File | LoC | Verdict | Reason |
|---|---|---|---|
| `tick_persistence.rs` | 8,721 | KEEP, slim | Largest single file; contains rescue→spill→DLQ machinery + ILP writer. For 4 SIDs the spill capacity defaults are oversized (100K ring per `aws-budget.md`). |
| `candle_persistence.rs` | 1,667 | KEEP, slim | `historical_candles` writer + ALTER for `candles_source` SYMBOL column. |
| `seal_spill.rs` + `seal_dlq.rs` + `seal_absorption.rs` + `seal_writer_*` + `shadow_*` (9 files) | ~5,500 | INVESTIGATE | The 21-TF shadow-candle writer in `shadow_persistence.rs` creates 21 tables today. Reducing to 2 TFs collapses ~80% of this code path. |
| `option_chain_minute_snapshot_persistence.rs` | 688 | KEEP | Strategy's primary input. |
| `ws_frame_spill.rs` | 589 | KEEP | WS-frame rescue. |
| `historical_fetch_marker.rs` | 457 | KEEP | Idempotency for historical fetch. |
| `partition_manager.rs` | 383 | KEEP | Partition→S3 cold archive. |
| `questdb_health.rs` | 374 | KEEP | Boot probe + readiness. |
| `tick_spill_drain.rs` | 320 | KEEP | Spill drain. |
| `disk_health_watcher.rs` | 264 | KEEP | Disk-full pre-flight. |
| `boot_probe.rs` | 225 | INVESTIGATE | Operator notes QuestDB boot probe re-runs every 10s forever after boot already succeeded — likely a bug, not deletion candidate. |

**Storage DROP candidate ~LoC:** **3-5K** from the 21-TF→2-TF collapse in shadow_persistence and seal pipelines.

### 3.5 `crates/api/` — 5,040 LoC (prod) / 1,177 (tests)

| File | LoC | Verdict | Reason |
|---|---|---|---|
| `middleware.rs` | 1,408 | INVESTIGATE / slim | Likely contains portal CORS, frontend auth etc. Bearer-token middleware stays. |
| `state.rs` | 811 | INVESTIGATE / slim | Has `depth_20_connections` / `depth_200_connections` AtomicU64s (confirmed by grep). DROP those fields + setters/getters. |
| `handlers/quote.rs` | 766 | KEEP | `/api/quote/{security_id}`. |
| `handlers/stats.rs` | 593 | KEEP | `/api/stats` table counts. |
| `lib.rs` | 491 | KEEP | Route registration. |
| `handlers/debug.rs` | 486 | KEEP | `/api/debug/logs/*` MCP read-only observability. |
| `handlers/health.rs` | 466 | KEEP | `/health`. |
| `handlers/mod.rs` | 19 | KEEP | |

**API DROP candidate ~LoC:** **1-2K** mostly in `middleware.rs` and `state.rs`.

### 3.6 `crates/app/` — 21,325 LoC (prod) / 2,905 (tests)

| File | LoC | Verdict | Reason |
|---|---|---|---|
| `main.rs` | 7,366 | INVESTIGATE / huge slim | Boot sequence + ~45 `tokio::spawn` sites. Many spawn deleted subsystems. See Section 8. Estimate 30-40% deletable (~2-3K LoC). |
| `trading_pipeline.rs` | 3,860 | INVESTIGATE / DROP | Imports `tickvault_trading::{indicator, oms, risk, strategy}` heavily. With 0 strategies wired, this whole pipeline is no-op. DROP until Phase 1. |
| `boot_helpers.rs` | 2,665 | INVESTIGATE / slim | Cross-references `tickvault_trading::oms`. Slim alongside trading deletion. |
| `infra.rs` | 2,321 | KEEP, slim | Docker / QuestDB readiness probes. Drop Loki/Prometheus/Grafana/Alertmanager probes per CW-only migration. |
| `observability.rs` | 1,410 | INVESTIGATE / huge slim | Uses `metrics-exporter-prometheus` + 4 OpenTelemetry crates. Replace with CloudWatch metrics emitter + tracing-subscriber CW writer. Estimated 50-70% deletable. |
| `bar_cache_loader.rs` | 768 | KEEP | RAM-first today/yesterday bar rehydrate. |
| `subsystem_memory.rs` | 650 | KEEP | Memory sampling. |
| `prev_oi_loader.rs` | 427 | KEEP | Per `PREVOI-01` rule, single boot-time warn if empty. Useful for 4 SIDs. |
| `option_chain_cache_loader.rs` | 412 | KEEP | 3-tier rehydrate. |
| `day_ohlc_orchestrator.rs` | 388 | KEEP | 4 IDX_I day OHLC tracker (locked architecture §15). |
| `metrics_catalog.rs` | 314 | KEEP, prune | Static label catalog — prune deleted metrics. |
| `core_pinning.rs` | 289 | DROP | Wave-5 Item 6 core_affinity pinning. On t4g.medium (2 vCPUs) pinning 4 workers is a no-op. Operator can reintroduce if measurable. |
| `bin/tv_doctor.rs` | 244 | KEEP | Operator health CLI. |
| `bin/smoke_test.rs` | 156 | INVESTIGATE / DROP | Depth-200 smoke test per `DEPTH200-SMOKE-01` rule — depth deleted, likely dead. |

**App DROP candidate ~LoC:** **5-8K** mostly from trading_pipeline + observability + main.rs spawn-site cleanup + core_pinning + smoke_test.

---

## Section 4 — Tests

### 4.1 Ratchet / meta-guards protecting deleted code (anti-regression value)

| Test file | LoC | Crate | Verdict |
|---|---|---|---|
| `crates/core/tests/indices4only_scope_lock_guard.rs` | 215 | core | KEEP (active ratchet for the LOCKED scope) |
| `crates/storage/tests/live_feed_purity_guard.rs` | 190 | storage | KEEP (active ratchet — BackfillWorker forbidden) |
| `crates/core/tests/prev_close_routing_5525125_guard.rs` | 230 | core | KEEP (Dhan Ticket cross-ref) |
| `crates/core/tests/static_universe_cleanup_guard.rs` | 179 | core | DROP after 1 month — already protects deleted code |
| `crates/core/tests/regression_cross_segment_subscribe.rs` | 253 | core | KEEP (I-P1-11 — cross-segment uniqueness) |
| `crates/storage/tests/dedup_segment_meta_guard.rs` | 264 | storage | KEEP (I-P1-11 enforcement) |
| `crates/storage/tests/error_level_meta_guard.rs` | 157 | storage | KEEP (flush/persist `error!` rule) |
| `crates/common/tests/error_code_rule_file_crossref.rs` | 247 | common | KEEP (every ErrorCode has rule mention) |
| `crates/common/tests/error_code_tag_guard.rs` | 157 | common | KEEP (every `error!` carries `code=`) |
| `crates/common/tests/triage_rules_guard.rs` | 245 | common | INVESTIGATE — may reference deleted ErrorCodes |
| `crates/common/tests/triage_rules_full_coverage_guard.rs` | 310 | common | INVESTIGATE — same |
| `crates/common/tests/loki_alloy_profile_guard.rs` | 200 | common | DROP — Loki/Alloy retired per CW-only |
| `crates/common/tests/cloudwatch_app_alarms_wiring.rs` | 152 | common | KEEP — active CloudWatch wiring |

### 4.2 Tests for deleted subsystems

Search for tests mentioning retired symbols (not exhaustive):

| Subsystem | Sample test files | Estimated DROP |
|---|---|---|
| Phase 2 dispatch | `crates/core/tests/phase2_enricher_to_ilp.rs` 214, `phase2_7_perf_and_correctness_fixes.rs` 210, `phase2_10_phase_jitter_fix.rs` 180, `phase3_runtime_lifecycle_e2e.rs` 317 | ~900 LoC |
| Depth-20/200 dynamic | Tests mentioning `depth_20_dyn` / `depth_200_dyn` / `dynamic_subscription_state` | INVESTIGATE |
| Movers | `tv_movers_writer*` / `option_movers` references in tests | INVESTIGATE |
| Greeks | `greeks` test files (likely already gone — `pub mod greeks` deleted PR #3) | ~0 remaining |
| Bhavcopy | Tests mentioning bhavcopy | INVESTIGATE |
| Universe builder CSV | `crates/core/tests/historical_fetch_decision_integration.rs` 193 | INVESTIGATE |
| Live-tick ATM resolver | Tests mentioning `live_tick_atm_resolver` / `MidMarketBootComplete` | INVESTIGATE |
| OMS integration | `crates/trading/tests/oms_integration.rs` 635, `safety_layer.rs` 1126, `obi_e2e.rs` 468, `indicator_strategy_e2e.rs` 1728 | KEEP only if OMS stays |
| Trading stress / chaos | `crates/trading/tests/stress_chaos.rs` 898, `loom_circuit_breaker.rs` 291 | KEEP only if OMS stays |

### 4.3 Tests likely to keep regardless

| Category | Files | Rationale |
|---|---|---|
| Parser | `crates/core/tests/websocket_protocol_e2e.rs` 1504, `snapshot_parser.rs` 596, `proptest_parser.rs` 310 | Quote/Ticker/Full parsing still needed |
| WS sleep / reconnect | `ws_sleep_resilience.rs` 202, `ws_sleep_resilience_b.rs` 241 | 2-WS scope still uses these |
| Storage chaos | `chaos_questdb_lifecycle.rs` 639, `chaos_zero_tick_loss.rs` 286, `chaos_burst_indices_only.rs` 166 | QuestDB resilience |
| Security audit | `crates/core/tests/security_audit.rs` 466 | KEEP |
| Common bootstrap | `claude_session_bootstrap_guard.rs` 519, `github_workflow_guard.rs` 417 | KEEP |

**Test DROP candidate ~LoC:** **5-8K** (rough order of magnitude).

---

## Section 5 — Rule files (`.claude/rules/`)

### 5.1 Dhan API rules (20 files in `dhan/`)

| File | Verdict | Reason |
|---|---|---|
| `api-introduction.md` | KEEP | Base URL + headers + rate limits — universal. |
| `authentication.md` | KEEP | JWT + TOTP + static IP. |
| `live-market-feed.md` | KEEP | Binary protocol — Ticker/Quote/Full parsing. |
| `annexure-enums.md` | KEEP | Shared enums + error codes. |
| `option-chain.md` | KEEP | Strategy's primary input. |
| `historical-data.md` | KEEP | 90-day historical fetch + cross-verify. |
| `live-order-update.md` | KEEP | The 2nd allowed WebSocket. |
| `release-notes.md` | KEEP | Version awareness. |
| `market-quote.md` | KEEP | REST LTP fallback for index 09:15 open. |
| `instrument-master.md` | DROP | Universe is now `LOCKED_UNIVERSE` const; no daily CSV refresh. |
| `traders-control.md` | DROP | Kill switch not wired (no consumer in src). |
| `super-order.md` | DROP | Not used by current/planned strategy. |
| `forever-order.md` | DROP | Not used. |
| `conditional-trigger.md` | DROP | Not used. |
| `edis.md` | DROP | Not used (no holdings sell). |
| `postback.md` | DROP | Not used (Live Order Update WS replaces). |
| `statements.md` | DROP | Not used. |
| `funds-margin.md` | DROP | Not used in dry-run. |
| `portfolio-positions.md` | INVESTIGATE / KEEP | `orphan_position_watchdog.rs` calls `GET /v2/positions` — keep this one. |
| `orders.md` | INVESTIGATE / KEEP-or-DROP-with-OMS | OMS api_client.rs — keep iff OMS stays. |

**Dhan rule DROP candidates:** 8-10 files.

### 5.2 Project rules (41 files in `project/`)

**KEEPs (core / permanent):**
`operator-charter-forever.md`, `websocket-connection-scope-lock.md`, `pr-completion-protocol.md`, `aws-budget.md`, `z-plus-defense-doctrine.md`, `per-wave-guarantee-matrix.md`, `live-feed-purity.md`, `security-id-uniqueness.md`, `audit-findings-2026-04-17.md`, `observability-architecture.md`, `hot-path.md`, `testing.md`, `testing-scope.md`, `rust-code.md`, `cargo-and-docker.md`, `enforcement.md`, `data-integrity.md`, `market-hours.md`, `gap-enforcement.md`, `plan-enforcement.md`, `live-market-feed-subscription.md`, `historical-candles-cross-verify.md`, `disaster-recovery.md`, `phase-0-architecture.md`, `stream-resilience.md`, `index-day-ohlc-tracker-error-codes.md`, `option-chain-cross-verify-error-codes.md`.

**DROPs / partial DROPs (subsystems deleted):**

| File | Verdict / Action |
|---|---|
| `wave-1-error-codes.md` | Partial DROP — keep PREVCLOSE/PREVOI sections; drop sections for deleted Phase 2 paths. |
| `wave-2-error-codes.md` | Partial DROP — keep WS-GAP/AUTH-GAP sections; DROP AUDIT-01..06 sections (those audit table writers never shipped — confirmed by missing `*_audit_persistence.rs` files). |
| `wave-2-c-error-codes.md` | KEEP (BOOT-03 clock skew). |
| `wave-3-error-codes.md` | KEEP (Telegram coalescer). |
| `wave-3-c-error-codes.md` | KEEP (SELFTEST). |
| `wave-3-d-error-codes.md` | KEEP, but `phase2_health` dim needs dropping when Phase 2 retires. |
| `wave-4-error-codes.md` | Mostly DROP — DEPTH-DYN-*, CORE-PIN-* (if dropped), many "reserved" stub entries; keep PROC-* / NET-* / RESILIENCE-*. |
| `wave-4-shared-preamble.md` | KEEP (general charter). |
| `wave-5-error-codes.md` | DROP CORE-PIN-* + DEPTH-* + PHASE2-READY-* + DEPTH200-SMOKE-01 sections (modules deleted). Keep VOLUME-MONO-01 + PREVCLOSE-03 if underlying code stays. |
| `wave-6-error-codes.md` | KEEP (active candle engine, but reflect 2-TF scope). |
| `phase-0-items-15-28-29-error-codes.md` | KEEP (post-open cross-check still relevant for 4 SIDs). |
| `phase-0-item-20-error-codes.md` | KEEP (orphan position watchdog). |
| `phase-0-gap-fill-error-codes.md` | KEEP if gap-fill scheduler stays. |
| `websocket-enforcement.md` | INVESTIGATE — check vs `live-market-feed-subscription.md` (likely stale duplicate). |

**Project rule DROP candidates:** 8-12 file or section-level deletions, ~3-5K LoC of rule content.

---

## Section 6 — Docs (`docs/`)

### 6.1 docs/architecture (26 files) — biggest cleanup target

| File | Verdict |
|---|---|
| `aws-indices-only-locked-architecture.md` | KEEP (canonical post-2026-05-18) |
| `aws-daily-lifecycle.md` | KEEP |
| `aws-cost-floor-analysis.md` | KEEP |
| `option-chain-z-plus-heart-piece.md` | KEEP |
| `tick-to-multi-tf-aggregator.md` | KEEP, but reflect 2-TF scope |
| `dhan-api-coverage-map.md` | KEEP, slim |
| `resilience-flow.md` / `resilience-simplification-4-sids.md` | KEEP |
| `instrument-complete-reference.md` / `instrument-technical-design.md` / `instrument-guarantee.md` / `instrument-dhan-dependency-map.md` | INVESTIGATE / partial DROP — written for FullUniverse. Replace with 4-SID instrument doc. |
| `restructure-plan-indices-only.md` | KEEP (history of the cut) |
| `historical-vs-live-cross-verification.md` | KEEP, slim |
| `precision-realism-and-table-redesign.md` | KEEP, TF-count update |
| `gap-inventory-2026-05-18.md` | KEEP (decision snapshot) |
| `100-percent-compliance-audit.md` | KEEP |
| `claude-cowork.md` | KEEP |
| `codebase-map.md` | INVESTIGATE / regenerate |
| `extensibility-proof.md` | INVESTIGATE |
| `guarantees.md` | KEEP |
| `local-vs-cloud-workflow.md` | KEEP |
| `ram-sizing-proof.md` | KEEP |
| `websocket-complete-reference.md` | KEEP |
| `zero-touch-stack.md` | KEEP, slim Loki/Alloy/Prom refs |

### 6.2 docs/runbooks (31 files)

| File | Verdict |
|---|---|
| `alertmanager-telegram.md` | DROP — Alertmanager retired per CW-only |
| `dashboards.md` | DROP if Grafana retired |
| `phase-4b-movers-cleanup.md` | DROP — movers retired |
| `expiry-day.md` | KEEP (index F&O expiry rollover) |
| `kill-switch.md` | INVESTIGATE — kill switch wired? |
| `oms-risk.md` | INVESTIGATE — DROP if OMS retired |
| Others (`auth.md`, `aws-*.md`, `daily-operations.md`, `dhan-api-down.md`, `internet-down-mid-session.md`, `macbook-dies-mid-session.md`, `operator-daily-startup.md`, `operator-end-to-end-automation.md`, `troubleshooting.md`, `websocket-offhours-disconnects.md`, `zero-tick-loss.md`, `phase-1-monitoring-rubric.md`, `claude-mcp-access.md`, `backtest-runner.md`, `historical-data.md`, `postmortem-template.md`, `README.md`) | KEEP |

### 6.3 docs/phases (6 files)

| File | Verdict |
|---|---|
| `phase-0-readme.md` | KEEP |
| `phase-1-live-trading.md` | KEEP |
| `block-01-master-instrument-download.md` | DROP — superseded by `LOCKED_UNIVERSE` |
| `block-02-authentication.md` | KEEP |
| `block-03-websocket-connection-manager.md` | KEEP, slim (pool size = 1 now) |
| `block-04-tradingview-terminal.md` | DROP — portal retired |

### 6.4 docs/archive (6 files)

All KEEP archived (`authentication-technical-design.md`, `holiday-calendar-*.md`, `instrument-gaps-consolidated.md`, `static-ip-*.md`).

### 6.5 Other dirs

| Dir | Verdict |
|---|---|
| `docs/operator/` | KEEP |
| `docs/standards/` (14 files) | KEEP — guarantee statements, secret rotation, testing standards |
| `docs/dhan-ref/` (21 files) | KEEP (Dhan API authoritative reference) |
| `docs/reference/` | KEEP |
| `docs/templates/` | KEEP |
| `docs/postmortems/` | KEEP |
| `docs/prompts/` | KEEP |
| `docs/dhan-support/` | KEEP |
| `docs/analysis/` (2 files) | KEEP |

### 6.6 `.claude/plans/` (~50 active-plan-*.md files)

- KEEP active: `active-plan-observability-cloudwatch-only.md`, `active-plan-deletion-audit.md` (this file).
- ARCHIVE ~45 historical wave/phase/movers/depth/gap plans into `.claude/plans/archive/` with date prefix.

**Docs DROP candidates:** ~15-25 files, ~5-10K LoC of docs content.

---

## Section 7 — QuestDB tables (`crates/storage/src/*_persistence.rs`)

Files matching `*_persistence.rs` (4 only):

| Writer file | Tables created | Verdict |
|---|---|---|
| `tick_persistence.rs` (8,721 LoC) | `ticks` | KEEP |
| `candle_persistence.rs` (1,667 LoC) | `historical_candles` | KEEP (cross-verify mirror) |
| `shadow_persistence.rs` (605 LoC) | `candles_1m`, `candles_2m`, …, `candles_15m`, `candles_30m`, `candles_1h`, `candles_2h`, `candles_3h`, `candles_4h`, `candles_1d` = **21 tables** + drop-cascade of 28 legacy `_shadow` tables | INVESTIGATE / shrink to 2 (1m + 5m). Confirmed via `TF_COUNT = 21` in `crates/trading/src/candles/tf_index.rs`. Biggest single QuestDB-side cleanup. |
| `option_chain_minute_snapshot_persistence.rs` (688 LoC) | `option_chain_minute_snapshot` | KEEP |

### Audit tables referenced by rule files but NOT shipped as code writers (cleanup opportunity for the RULE files themselves)

`grep -rln 'static_ip_audit\|phase2_audit\|orphan_position_audit\|self_trade_audit\|sl_replacement_audit\|pnl_audit\|live_instance_lock\|auth_renewal_audit\|open_price_audit\|order_update_ws_audit\|depth_rebalance_audit\|ws_reconnect_audit\|boot_audit\|selftest_audit\|order_audit\|bar_correction_audit\|gap_fill_audit\|s3_archive_log' crates/` returns hits ONLY in rule files, error_code.rs, notification events, and test guards. **Zero `_audit_persistence.rs` writer files exist in `crates/storage/src/`**. The 11 audit tables in `phase-0-architecture.md` were planned but never shipped as code. Either ship the writers OR delete the rule sections.

**QuestDB table DROP candidates:** 19 candle tables (`candles_2m`/`3m`/`4m`/`6m`/`7m`/`8m`/`9m`/`10m`/`11m`/`12m`/`13m`/`14m`/`15m`/`30m`/`1h`/`2h`/`3h`/`4h`/`1d` — keep only `1m` + `5m`).

---

## Section 8 — Background tasks spawned at boot (`crates/app/src/main.rs`)

~45 `tokio::spawn` sites. Deduced functional list:

| Task | Verdict | Reason |
|---|---|---|
| pre-market readiness check | KEEP | Index pre-open buffer (09:00–09:12) |
| slow-boot observability consumer | KEEP, slim | Reads ws_frame_spill |
| multi-TF aggregator | KEEP, slim (21→2 TFs) | The candle engine |
| aggregator heartbeat | KEEP | Operator visibility |
| IST-midnight force-seal | KEEP | Daily reset |
| L10 tick_storage broadcast consumer | KEEP | RAM-first hot-path consumer |
| day_ohlc orchestrator | KEEP | 4 IDX_I day OHLC |
| day_ohlc tick consumer | KEEP | Same |
| day_ohlc midnight reset | KEEP | Same |
| prev_oi periodic refresh | KEEP (small for 4 SIDs) | Option chain prev-day OI |
| midnight rollover (tick_enricher) | KEEP | Day boundary |
| **Phase 2 pre-open snapshotter** | **DROP** | Phase 2 retired (per `mod.rs` comments) |
| historical fetch task | KEEP, slim | 4 SIDs × small TF set |
| pre-market yesterday's-1d fetch scheduler | KEEP | Daily 08:05 IST check |
| SLO score scheduler (10s) | KEEP, slim (drop phase2/depth dims) | Wave 3-D |
| option-chain expiry warmup (09:00:30) | KEEP | Strategy primary input |
| option-chain minute-snapshot scheduler | KEEP | Strategy primary input |
| order update WS | KEEP | The 2nd allowed WS |
| **trading_pipeline** | **DROP** | Imports OMS+risk+strategy; 0 strategies wired |
| **strategy hot-reload watcher** | **DROP** | 0 strategies |
| API server | KEEP | 4 handlers |
| token renewal | KEEP | JWT 23h refresh |
| mid-session profile watchdog (15 min) | KEEP | Auth health |
| token sweep (4h) | KEEP | Auth safety-net |
| background periodic health check | KEEP | `/health` consumers |
| pool watchdog (5s) | KEEP, slim (pool size = 1) | WS-GAP-04/05 |
| gap-fill scheduler | INVESTIGATE | Sized for 5-WS pool; with 1 main-feed conn most diagnostic logic is dead. |
| seal writer loop | KEEP, slim | 2-TF instead of 21 |
| spill disk-health watcher | KEEP | Disk-full pre-flight |
| summary writer (errors.summary.md, 60s) | KEEP | Operator visibility |
| no_tick_watchdog | KEEP | Tick gap detection |
| QuestDB tick writer connector | KEEP | ILP writer |
| cold-path tick persistence writer | KEEP | Same |
| cross-verify scheduler (15:31 IST + 08:05 IST) | KEEP, slim | 4-SID daily verify |
| Wave 1 Item 0.a prev_close writer | KEEP | Disk-persistent cache |
| OOM monitor / proc monitor (Wave 4-E) | INVESTIGATE | If never wired, drop. |

**App DROP candidates from main.rs spawn sites:** ~5-10 spawn sites + their wiring code.

---

## Section 9 — Workspace members + binary targets

### 9.1 Should `tickvault-api` be merged into `app`?

- Currently 4 small handlers + 1 middleware + 1 state file = 5,040 LoC.
- After cleanup: ~3K LoC, consumed only by `app`.
- **Recommendation: KEEP separate** for compile-time boundary, but consider moving `state.rs` to `common` (HealthStatus is observed widely).

### 9.2 Should `tickvault-trading` be merged into `core`?

- Currently 32,827 LoC.
- After dry-run deletion: maybe 5-8K LoC (candles + in_mem + orphan_position_watchdog).
- **Recommendation: MERGE** the surviving modules into `core` (`core/src/candles/`, `core/src/in_mem/`, `core/src/orphan_position_watchdog.rs`). Drop the trading crate entirely. Reintroduce a `trading` crate WHEN the first live strategy lands in Phase 1.

### 9.3 Binary targets

- `crates/app/src/main.rs` → `tickvault-app` binary — KEEP.
- `crates/app/src/bin/tv_doctor.rs` — KEEP.
- `crates/app/src/bin/smoke_test.rs` — DROP if it's the depth-200 smoke test (per `DEPTH200-SMOKE-01` rule, depth is retired).

---

## Section 10 — 3-phase deletion sequence (safe order)

### Phase A — Zero-blast-radius: docs, rules, archived plans (no compile)

**Expected LoC delta:** -5 to -10K (markdown only).
**Verification:** none — no code touched. Just `cargo test` to confirm no `include_str!` doc loads broke.

Actions:
1. Delete 8-10 Dhan rule files (kill-switch, super/forever/conditional, EDIS, postback, statements, funds-margin, instrument-master).
2. Delete dead sections of wave-error-code rule files (DEPTH-DYN-*, CORE-PIN-*, PHASE2-READY-*, AUDIT-01..06, MOVERS-*).
3. Delete `docs/runbooks/alertmanager-telegram.md`, `docs/runbooks/dashboards.md`, `docs/runbooks/phase-4b-movers-cleanup.md`, `docs/phases/block-01-*`, `docs/phases/block-04-*`.
4. Archive ~45 `.claude/plans/active-plan-*.md` files into `.claude/plans/archive/`.
5. Update `.claude/rules/project/observability-architecture.md` to reflect CloudWatch-only and remove the 5-sink Loki/Alloy refs.

**Ratchets to update:** `error_code_rule_file_crossref.rs` (drop variants whose rule sections are gone), `triage_rules_guard.rs`, `loki_alloy_profile_guard.rs` (DROP entire file).

### Phase B — Module deletes inside crates (compile required, test green required)

**Expected LoC delta:** -25 to -40K Rust LoC.
**Verification:** `cargo build --workspace`; `cargo test -p <each touched crate>`; `make doctor`; smoke-test boot.

Order (small blast radius first):
1. **Drop dead AtomicU64s** in `crates/api/src/state.rs` (`depth_20_connections`, `depth_200_connections` + setters/getters/tests). ~50 LoC.
2. **Collapse `TfIndex` from 21 to 2 timeframes** (1m + 5m). Update `tf_index.rs`, `multi_tf_aggregator.rs`, `aggregator_cell.rs`, all per-instrument `[Mutex<…>; 21]` arrays, all `[Sender; 21]` ILP arrays, `shadow_persistence.rs` table list. ~2-3K LoC across trading + storage + 19 candle tables collapsed at the DB layer.
3. **Delete `core_pinning.rs`** + unwire from main.rs. ~300 LoC.
4. **Delete `bin/smoke_test.rs`** (depth-200 smoke). ~160 LoC.
5. **Delete unused `events.rs` NotificationEvent variants** (DepthRebalance*, MoversWriter*, Phase2*, MidMarketBoot*, Depth20Dyn*, Depth200Dyn*, Greeks*) + matching service.rs dispatch arms. ~1-2K LoC.
6. **Slim `observability.rs`** — remove Prometheus exporter + OTLP exporter wiring. ~700 LoC.
7. **Drop `trading::strategy` + `trading::indicator` + `trading::risk` + most of `trading::oms`** (keep `api_client.rs` portion that `orphan_position_watchdog` consumes). ~25K LoC.
8. **Drop `crates/app/src/trading_pipeline.rs`** (no consumer after #7). ~3.9K LoC.
9. **Slim `crates/core/src/historical/candle_fetcher.rs`** for 4-SID scope. ~2K LoC.
10. **Slim `crates/core/src/instrument/subscription_planner.rs`** to ~50 LoC (emit LOCKED_UNIVERSE in 1 message). ~4.7K LoC reduction.
11. **Slim `crates/core/src/pipeline/tick_processor.rs`** for 4-SID scope. ~3K LoC reduction.
12. **Slim `crates/core/src/websocket/connection_pool.rs`** for pool size 1. ~1.5K LoC reduction.
13. **Slim `crates/core/src/notification/events.rs`** — drop ~40-60 dead variants. ~1.5-2K LoC.
14. **Delete `crates/common/src/order_types.rs`** if OMS gone. ~750 LoC.
15. **Drop dead tests** matching deleted subsystems (~5-8K LoC).

Ratchets to update: `pub-fn-wiring-guard.sh` allowlist, `banned-pattern-scanner.sh` (drop patterns matching deleted code), `pub-fn-test-guard.sh` allowlist, mutation-killer test exclusions.

### Phase C — Cargo.toml + workspace member consolidation

**Expected LoC delta:** -100 to -300 transitive deps in `Cargo.lock`; binary size shrink ~20-40%.
**Verification:** `cargo build --release --workspace`; `cargo deny check`; binary size check.

Order:
1. Drop deps from root `Cargo.toml`: `dashmap`, `metrics-exporter-prometheus`, `hdrhistogram`, `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `tracing-opentelemetry`, `bitcode`, `csv`, `notify`, `signal-hook`, `failsafe`, `backon`, `yata`, `statig`, `governor`, `jaeckel`. (17 deps)
2. Update each crate's `Cargo.toml` to drop matching `<crate> = { workspace = true }` entries.
3. After confirming trading crate has shrunk to ~5-8K LoC: **MERGE `crates/trading/` into `crates/core/`**. Update `Cargo.toml [workspace] members`.
4. Audit AWS SDK features: drop `sso` + `credentials-process` (instance-role only on prod).
5. Drop `tower-http` features `fs` + `compression-gzip`.
6. Re-run `cargo deny check`, `cargo audit`, full workspace tests.

---

## Section 11 — Risks + rollback plan

### Ratchets that will fail and need updates

| Ratchet | Failure mode | Update needed |
|---|---|---|
| `crates/storage/tests/dedup_segment_meta_guard.rs` | Drops candle tables | Update table list to 2 (1m + 5m). |
| `crates/common/tests/error_code_rule_file_crossref.rs` | Drops ErrorCode variants without rule mention | Either delete the variant or delete the runbook section; tighten the matching set. |
| `crates/common/tests/error_code_tag_guard.rs` | Drops `error!` sites mentioning a removed code | Drop matching emit sites. |
| `crates/common/tests/metrics_catalog.rs` | Drops Prometheus metric names | Replace with CloudWatch metric catalog or drop the test. |
| `crates/common/tests/loki_alloy_profile_guard.rs` | Per CW-only migration | DROP entire test file. |
| `crates/common/tests/triage_rules_guard.rs` / `triage_rules_full_coverage_guard.rs` | Drops `.claude/triage/error-rules.yaml` rules for retired codes | Update YAML in sync. |
| `crates/core/tests/indices4only_scope_lock_guard.rs` | n/a — guards the LOCK | KEEP intact. |
| `crates/storage/tests/live_feed_purity_guard.rs` | n/a — guards BackfillWorker non-resurrection | KEEP intact. |
| `crates/storage/tests/zero_tick_loss_alert_guard.rs` | Drops Prom alert YAML | Replace with CloudWatch alarm check. |
| Mutation-killer tests (3 crates) | Drops modules they targeted | Reduce expected mutation surface. |
| `crates/app/tests/feature_flag_rollback_guard.rs` | Drops config feature flags | Update to current flag set. |

### Hooks needing allowlist updates

- `.claude/hooks/banned-pattern-scanner.sh` — drop patterns matching deleted symbols (BackfillWorker stays as anti-resurrection; movers/depth_20/depth_200 patterns can be dropped or kept).
- `.claude/hooks/pub-fn-wiring-guard.sh` — re-baseline call-site set.
- `.claude/hooks/pub-fn-test-guard.sh` — re-baseline pub-fn set.
- `.claude/hooks/per-item-guarantee-check.sh` — should still pass since this audit isn't a code change.

### Rollback plan

- Each Phase B step is a separate PR, one mergeable PR at a time per `pr-completion-protocol.md` §H. If a downstream caller surfaces post-merge: `git revert <sha>` restores the deleted module verbatim.
- Phase B steps 2 (TF collapse) and 7 (trading deletion) are highest-risk because they touch many call sites. Recommend implementing each as its own approved plan with `active-plan-*.md` items + 9-box checklist + 3-agent adversarial review per the operator charter.
- Cargo.toml dep removals (Phase C) are trivially revertable.
- Workspace-member merge (Phase C step 3) is the highest-cost rollback — keep this as the final step.

### What MUST stay regardless (operator charter)

Per `operator-charter-forever.md` §A-§I:

- `LOCKED_UNIVERSE` const + `SubscriptionScope::Indices4Only` single-variant enum.
- `effective_main_feed_pool_size(_, _) → PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1` constant.
- The 2 WebSocket endpoints (main-feed + order-update) and their reconnect-parity helpers.
- I-P1-11 composite-key uniqueness + DEDUP UPSERT KEYS on every QuestDB table.
- The 7-layer observability fan-out (now Prom→CloudWatch translation but same fan-out shape).
- The Wave-2/3 audit-table family **once it is actually shipped** (currently rule-only, code never landed).
- The 3-agent adversarial review pattern.

---

## Phase A execution log (2026-06-10)

Verified against live repo state (the audit is dated 2026-05-25; two Phase A actions were already executed by intervening sessions, and one verdict went stale):

| Phase A action | Finding 2026-06-10 | Disposition |
|---|---|---|
| 1. Delete 8-10 Dhan rule files | `traders-control.md`, `super-order.md`, `forever-order.md`, `conditional-trigger.md`, `edis.md`, `postback.md`, `statements.md`, `funds-margin.md` — ALL already absent from `.claude/rules/dhan/` (12 KEEP files remain). `instrument-master.md` was RECREATED 2026-05-29 as the always-on digest for the daily-universe CSV architecture (post-dates this audit) — verdict STALE, file is now KEEP. | ALREADY DONE (8 files); `instrument-master.md` verdict superseded — KEEP |
| 2. Delete dead wave-error-code sections | Section deletions would orphan live `ErrorCode` variants against `crates/common/tests/error_code_rule_file_crossref.rs` — requires paired `crates/` edits | DEFERRED (operator "No crates/ changes" constraint, 2026-06-10) |
| 3. Delete runbooks + phase blocks | `alertmanager-telegram.md`, `dashboards.md`, `phase-4b-movers-cleanup.md`, `block-01-master-instrument-download.md`, `block-04-tradingview-terminal.md` — ALL already absent | ALREADY DONE |
| 4. Archive historical plans | 14 files archived this session with `git mv` + date prefix (13 zero-inbound-reference historical plans + the VERIFIED `active-plan.md` for merged #1069). Zero-hit proof: repo-wide grep per file, hits only in self/this audit. KEPT (inbound refs from `crates/` guards, CI, or kept docs): `100pct-audit-tracker.md`, `autonomous-operations-100pct.md`, `pr-288-completion.md`, `v2-architecture.md`, `v2-phases.md`, `v2-ratchets.md`, `v2-risks.md`, `aws-lifecycle/*`, `friday-may-15-mega/*`, `research/*` | DONE 2026-06-10 |
| 5. Update `observability-architecture.md` (CloudWatch-only, drop 5-sink Loki/Alloy refs) | Coupled to dropping `crates/common/tests/loki_alloy_profile_guard.rs` (a `crates/` change) | DEFERRED with action 2 |

## What to do next (3 bullets)

1. **Operator approval gate.** Before any code touches, get explicit approval on this audit's three biggest investigation calls: (a) collapse `TF_COUNT` from 21 → 2 (1m + 5m only); (b) drop `crates/trading/*` modules except `candles/` + `in_mem/` + `orphan_position_watchdog.rs`; (c) drop the entire OpenTelemetry/Prometheus exporter stack now that the CloudWatch-only migration is locked. Each is a >1,000 LoC change that must be a separate PR per the serial-PR protocol.
2. **Start with Phase A (zero-blast).** Markdown-only deletions can land in a single PR with no compile risk and immediately reduce session-load time + auto-load context size. Aim for ~6-10K LoC of markdown deleted in one PR.
3. **Pick one Phase B step at a time, in order.** Smallest-blast-radius first step is the `api::state` dead-AtomicU64 cleanup (~50 LoC). Biggest single quick win is the `TfIndex 21 → 2` collapse, which deletes 19 candle tables + their writers + their seal pipelines (~3-5K LoC + 19 fewer QuestDB tables + ~80% shadow_persistence reduction). Do not bundle multiple Phase B steps — each is its own approved-plan PR with 9-box checklist and 3-agent review per `operator-charter-forever.md` §C/§E/§H.
