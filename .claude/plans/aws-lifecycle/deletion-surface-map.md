# Deletion Surface Map — Indices-Only Scope Reduction

> **Status:** PLAN (no code shipped). Captures the Explore-agent deletion map produced 2026-05-18.
> **Authority:** Operator lock 2026-05-18 → universe = 4 SIDs only (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21).
> **CAVEAT:** The agent was briefed with an older charter §I scope (4 IDX_I + 218 NSE_EQ underlyings). The NEW operator lock 2026-05-18 drops the 218 stocks entirely. All "KEEP 218 NSE_EQ" rows below are AMENDED to **DELETE** unless they are infrastructure shared with the 4 indices.
> **Companion docs:** `docs/architecture/aws-indices-only-locked-architecture.md`, `aws-lifecycle/active-plan-aws-lifecycle.md`.

---

## Scope drift correction (operator-charter §I → 2026-05-18 lock)

| Element | Old scope (§I) | New scope (2026-05-18) |
|---|---|---|
| Main feed SIDs | 4 IDX_I + 218 NSE_EQ = 222 | **4 IDX_I only** |
| Stock F&O contracts subscribed | 0 (already not subscribed) | 0 (unchanged) |
| NSE_EQ cash equities subscribed | 218 | **0** |
| Display indices (sectoral + INDIA VIX) | 26 | **1 (INDIA VIX only)** |
| Depth-20 / depth-200 feeds | already disabled | unchanged (disabled) |
| Phase 2 stock F&O dispatch | disabled | unchanged (disabled) |
| Greeks pipeline | disabled | unchanged (disabled) |

---

## Master deletion table (14 categories, ~16K LoC across ~46 files)

### 1. crates/core/src/instrument

| File / module | Action | Reason |
|---|---|---|
| `subscription_planner.rs` stock branch (lines 242, 1596–4777, 28 tests) | **DELETE** | No stock F&O subscription needed |
| `bhavcopy_scheduler.rs` + `bhavcopy_fetcher.rs` + `bhavcopy_cross_check.rs` + `csv_parser.rs` | **DELETE** | Bhavcopy only useful for F&O delivery cross-check |
| `live_tick_atm_resolver.rs` (entire file) | **DELETE** | Stock ATM resolver; not needed for index option chain REST (REST returns full chain) |
| `depth_strike_selector.rs` | **DELETE** | No depth subscriptions |
| `depth_rebalancer.rs` | **DELETE** | No depth subscriptions |
| `depth_20_dynamic_subscriber.rs` (438 lines) | **DELETE** | Depth disabled |
| `depth_200_dynamic_subscriber.rs` | **DELETE** | Depth disabled |
| `depth_dynamic_top_volume_selector.rs` | **DELETE** | Depth disabled |
| `subscription_planner.rs` core SIDs section | **MODIFY** | Keep IDX_I branch; gut stock/display branches |

### 2. Depth subscription infrastructure

| File | Action | Reason |
|---|---|---|
| `crates/app/src/depth_dynamic_pipeline_v2.rs` | **DELETE** | All depth-dynamic infrastructure |
| `crates/app/src/depth_20_conn_spawner.rs` | **DELETE** | No depth conns spawned |
| `crates/core/src/websocket/depth_connection.rs` | **DELETE** | No depth WS |
| All `spawn_depth_dynamic_pool()` call sites in `crates/app/src/main.rs` (lines 4115, 4170, 4216, 4283, 4343) | **DELETE** | Dead code |

### 3. Greeks pipeline

| File | Action | Reason |
|---|---|---|
| `crates/trading/src/greeks/inline_computer.rs` | **DELETE** | No real-time Greeks; REST option chain provides |
| `crates/trading/src/greeks/aggregator.rs` | **DELETE** | No inline feed |
| `crates/app/src/greeks_pipeline.rs` | **DELETE** | Pipeline task |
| `crates/trading/src/greeks/black_scholes.rs` | **KEEP** | Library; might be useful for IV solver future work |
| `crates/storage/src/greeks_persistence.rs` for `dhan_option_chain_raw` table | **KEEP** | Operator wants REST option chain stored |
| `option_greeks`, `pcr_snapshots`, `greeks_verification` tables | **DELETE** | Inline pipeline gone |

### 4. Movers pipeline

| Component | Action | Reason |
|---|---|---|
| `MoversWriter`, `MoversUnifiedWriter` classes | **DELETE** | Movers not in scope |
| `crates/app/src/movers_pipeline.rs` | **DELETE** | Pipeline task |
| `option_movers`, `movers_1s` tables | **DELETE** | Already partially retired PR 5c.5 |
| 25 movers timeframe matviews (`movers_5s` ... `movers_1h`) | **DELETE** | Already retired |

### 5. Phase 2 dispatcher

| Component | Action | Reason |
|---|---|---|
| `phase2_recovery.rs::should_spawn_phase2_scheduler` | **MODIFY** (`const fn { false }`) | Compiler-pin scope lock |
| `phase2_recovery.rs::should_spawn_depth_dynamic_pipeline` | **MODIFY** (`const fn { false }`) | Same |
| `phase2_recovery.rs::should_spawn_greeks_pipeline` | **MODIFY** (`const fn { false }`) | Same |
| Phase 2 spawn chains in `main.rs` (14 call sites lines 1824, 4062, 4115, 4170, 4216, 4283, 4343, 4439, 4465, 4484, 5165, 5754, 6447, 9056) | **DELETE** entire conditional blocks | Dead code under new scope |

### 6. Config

| Element | Action | Reason |
|---|---|---|
| `SubscriptionScope::IndicesUnderlyingsOnly` enum | **MODIFY** → rename to `Indices4Only` or add new variant; restrict to 4 SIDs | New tightening lock |
| `[subscription]` TOML — `feed_mode = "Ticker"` for 4 IDX_I | **MODIFY** | All 4 use Ticker |
| `[greeks]` block in `config/base.toml` | **DELETE** | Pipeline removed |
| `[movers]` block | **DELETE** | Pipeline removed |
| `[depth_20.dynamic]` + `[depth_200.dynamic]` | **DELETE** | No depth |
| Add new `[option_chain]` block with cadence_secs, underlyings, max_cache_age_secs | **ADD** | New heart-piece component |
| `effective_main_feed_pool_size()` | **MODIFY** | Return 1 for new scope (4 SIDs easily fit on 1 conn) |

### 7. Storage tables

| Table | Action |
|---|---|
| `ticks` | **KEEP** — primary tick persistence (now only 4 SIDs writing) |
| `candles_1m` through `candles_1d` (only the locked TFs: 1m, 3m, 5m, 15m, 1h, 1d) | **KEEP** |
| Other timeframe candle tables (e.g. `candles_1s`, `candles_30s`) | **DELETE** unless explicitly in locked TF list |
| `previous_close` | **KEEP** for indicator warm-up |
| `option_chain_snapshots`, `option_chain_request_audit`, `dhan_option_chain_raw` | **KEEP** (heart-piece) |
| `phase2_audit` | **DELETE** |
| `depth_rebalance_audit` | **DELETE** |
| `depth_dynamic_diff_audit` | **DELETE** |
| `deep_market_depth`, `market_depth` | **DELETE** |
| `option_greeks`, `pcr_snapshots`, `greeks_verification` | **DELETE** |
| `option_movers`, `movers_1s` + 25 matviews | **DELETE** |
| `order_audit`, `boot_audit`, `auth_renewal_audit`, `selftest_audit` | **KEEP** (operational audit framework) |

### 8. Tests

| Test file | Action |
|---|---|
| `crates/app/tests/phase2_recovery_wiring.rs` | **DELETE** |
| 15 `phase2_*.rs` files in `crates/core/tests/` | **DELETE** |
| `crates/common/tests/depth_invariants_proptest.rs` | **DELETE** |
| `depth_rebalance_telegram_suppression_guard.rs`, `depth_retry_cap_guard.rs`, `dhat_deep_depth.rs` | **DELETE** |
| `dhat_depth_sequence_tracker.rs`, `loom_depth_sequence_tracker.rs` | **DELETE** |
| `initial_depth_dispatch_guard.rs` | **DELETE** |
| `no_boot_depth_subscribe_guard.rs` | **MODIFY** → rewrite as positive test that scope=Indices4Only blocks depth |
| `proptest_greeks_core.rs` | **KEEP** | Library tests |
| All tests touching `218 NSE_EQ` or `phase2_emit` or `depth_20_dynamic` | **DELETE** |

### 9. Notification events

| Variant | Action |
|---|---|
| `Phase2Complete`, `Phase2Failed`, `Phase2ReadinessPassed`, `Phase2ReadinessFailed` | **DELETE** |
| `DepthRebalanced`, `DepthRebalanceFailed`, `Depth20DynamicSwap20`, `Depth20DynamicTopSetEmpty`, `Depth20DynamicSwapChannelBroken`, `Depth200DynamicTopGainersEmpty` | **DELETE** |
| `GreeksComputed`, `GreeksAggregated` | **DELETE** |
| `MoversSnapshot*` | **DELETE** |
| `MidMarketBootComplete` | **MODIFY** — simplify for 4 SIDs |
| `OptionChainFetched`, `OptionChainStaleHaltStrategy` (NEW), `OptionChainBackoffExhausted` (NEW), etc | **KEEP / ADD** per option-chain-z-plus doc |
| `BootReadyConfirmation`, `MarketOpenStreamingConfirmation`, `SelfTestPassed`, `EodDigest` | **KEEP** |

### 10. ErrorCode variants

| Prefix | Action |
|---|---|
| `PHASE2-*` (PHASE2-01, PHASE2-02, PHASE2-READY-01) | **DELETE** |
| `DEPTH-*` (DEPTH-DYN-01, DEPTH-DYN-02, DEPTH-DYN-03, DEPTH-20-DYN-03, DEPTH-200-DYN-01, DEPTH200-SMOKE-01) | **DELETE** |
| `AUDIT-01` (Phase2WriteFailed), `AUDIT-02` (DepthRebalanceWriteFailed) | **DELETE** |
| All other audit / WS / auth / boot / risk codes | **KEEP** |
| Add 8 new `OPTION-CHAIN-*` codes per heart-piece doc §8 | **ADD** |

### 11. Rule files and runbooks

| File | Action | Reason |
|---|---|---|
| `.claude/rules/project/depth-subscription.md` | **DELETE** | Depth not in scope |
| `.claude/rules/project/phase-0-architecture.md` | **MODIFY** — strip Phase 2 stock-F&O sections | Phase 2 dispatch out of scope |
| `phase-0-gap-fill-error-codes.md`, `phase-0-item-20-error-codes.md`, `phase-0-items-15-28-29-error-codes.md` | **DELETE** or **MODIFY** | Stock-F&O-only items deleted |
| `live-market-feed-subscription.md` | **MODIFY** — rewrite Wave-5 update for 4-SID-only scope |
| `depth-rebalance-telegram.md` runbook | **DELETE** |
| `phase-2-empty-plan.md` runbook | **KEEP** as historical |
| `movers.md` runbook | **DELETE** |
| `websocket-depth.md` runbook | **DELETE** |
| Operator-charter, Z+ doctrine, security-id-uniqueness, etc | **KEEP** |
| All `docs/dhan-ref/*.md` | **KEEP** | External API ref |

### 12. Active plans (archive)

| Plan | Action |
|---|---|
| `active-plan-movers-cleanup-5b-5c-5d.md` | **CLOSE** (completed) |
| `active-plan-depth-dynamic-redesign.md` | **CLOSE** (deleted) |
| `active-plan-29-tf-and-movers-deletion.md` | **CLOSE** |
| `active-plan-movers-22tf-redesign.md`, `*-v2.md` | **CLOSE** |
| `active-plan-wave-4.md`, `active-plan-wave-5-indices-only.md`, `active-plan-wave-6-backlog.md` | **CLOSE or HEAVILY REVISE** — many items now N/A |
| `active-plan-aws-lifecycle.md` | **KEEP** (this session) |
| `friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md` | **MODIFY** — strip stock-F&O items |

### 13. Grafana dashboards / Prometheus

| Component | Action |
|---|---|
| ALL `deploy/docker/grafana/dashboards/*.json` | **DELETE** (entire dir) — Grafana out per LOCK-L3 |
| `deploy/docker/grafana/provisioning/` | **DELETE** entire dir |
| `deploy/docker/prometheus/` entire dir | **DELETE** — Prom out |
| `deploy/docker/prometheus/alerts.yml` | **DELETE** — replaced by CloudWatch Alarms |
| `deploy/docker/alertmanager/` | **DELETE** — Alertmanager out |
| `deploy/docker/loki/`, `deploy/docker/alloy/` | **DELETE** — CW Logs replaces |
| `deploy/docker/traefik/` | **DELETE** — port 3001 direct via SSM tunnel |
| `deploy/docker/valkey/` | **DELETE** — in-memory + SSM replaces |
| Test files pinning Grafana/Prom dashboards | **DELETE** (e.g. `operator_health_dashboard_guard.rs`, `resilience_sla_alert_guard.rs`) — replace with CloudWatch Alarm ratchets |

### 14. Docker compose

`deploy/docker/docker-compose.yml`: drop services `tv-grafana`, `tv-prometheus`, `tv-alertmanager`, `tv-loki`, `tv-alloy`, `tv-traefik`, `tv-valkey`. **KEEP** services `tv-app`, `tv-questdb`.

---

## Summary metrics

| Category | DELETE files | DELETE LoC | MODIFY files | KEEP files | Notes |
|---|---|---|---|---|---|
| Crates/instrument | ~7 | ~2,800 | 1 | ~12 | |
| Greeks | 3 | ~2,200 | 0 | 2 (lib + table) | |
| Movers | 2 | ~1,500 | 0 | 0 | already mostly retired |
| Phase 2 / main.rs | 0 (gutted) | ~1,100 | 1 | 1 | const fn stubs |
| Config | 2 sections | ~60 | 1 | 1 | adds `[option_chain]` |
| Storage tables | ~9 tables | (DDL only) | 0 | ~10 | |
| Tests | ~18 | ~4,200 | 1 | ~30+ | |
| Notification events | ~12 variants | ~250 | 1 | ~30+ | adds option-chain events |
| ErrorCode | ~10 variants | ~70 | 0 | ~50+ | adds 8 option-chain codes |
| Rules + docs | ~8 | ~3,000 | ~3 | ~25+ | |
| Plans | ~6 | (archive) | 1 | ~4 | |
| Docker services | 6 services | ~250 (yml) | 1 (docker-compose) | 2 | |
| Grafana/Prom/etc | full 4 dirs | ~8,000 | 0 | 0 | replaced by CloudWatch |
| **TOTAL** | **~75 files / 6 dirs** | **~23,500 LoC** | **10 files** | **~150+ KEEP** | |

---

## PR sequencing (mandatory per operator-charter §H — one PR at a time)

| # | PR | Why this order | Branch |
|---|---|---|---|
| 1 | Add `[option_chain]` config block + skeleton ErrorCode variants + skeleton ratchet tests (NO live code, just stubs) | Cement the new contracts before deleting old infra | `claude/aws-lifecycle-prep-1` |
| 2 | Delete movers pipeline + tables | Already retired upstream; lowest risk | `claude/del-movers` |
| 3 | Delete Greeks inline pipeline (keep Black-Scholes lib + REST snapshot table) | Decoupled from main feed | `claude/del-greeks-inline` |
| 4 | Delete depth-20 / depth-200 dynamic + supervisor + WS depth conns | No depth → no orchestration | `claude/del-depth-dyn` |
| 5 | Delete Phase 2 dispatcher + bhavcopy + prev_OI + live_tick_atm_resolver | All stock-F&O orchestration | `claude/del-phase2` |
| 6 | Tighten SubscriptionScope to `Indices4Only`; remove 218 NSE_EQ from planner | Final universe lock | `claude/scope-tighten-indices4` |
| 7 | Delete Grafana / Prometheus / Alertmanager / Loki / Alloy / Traefik / Valkey from docker-compose; add CloudWatch agent config | Observability cutover | `claude/cutover-cloudwatch` |
| 8 | Rescue ring resize 5M → 100K; option-chain Z+ heart-piece implementation | Right-size + new heart piece | `claude/right-size-and-heart` |
| 9 | Terraform: switch instance type to t4g.medium; schedule 08:00/17:00 IST; new alarms | AWS-side cutover | `claude/aws-tf-t4g-medium` |
| 10 | Strategy fail-closed gate; bar_cache wiring to indicators | RAM-first invariant fully enforced | `claude/ram-first-enforce` |
| 11 | Documentation + ratchet test cleanup | Final polish | `claude/docs-cleanup` |

**Each PR is independent + reversible.** Total estimated effort: 4-6 weeks at 1 PR/session.

---

## Open questions (operator decision needed)

1. **218 NSE_EQ cash equities — really delete?** They were not in the operator's 2026-05-18 verbatim list (operator said only NIFTY/BANKNIFTY/SENSEX/VIX). Confirming: yes, drop them.
2. **Display indices (26 sectoral) — really delete?** Operator's list had only INDIA VIX. Confirming: yes, drop the other 25.
3. **Timeframe table for ticks-storage — what TFs are kept persisted?** Locked initial: 1m/3m/5m/15m/1h/1d. Drop everything else from `candles_*` tables?
4. **Currently archived audit tables (boot_audit, order_audit, etc) — keep at slimmer scope?** Yes, keep — they're scope-agnostic.

---

## Trigger / auto-load

This plan activates when editing any file in the DELETE column. Pre-PR gate must verify the operator-charter §H "one PR open at a time" rule.
