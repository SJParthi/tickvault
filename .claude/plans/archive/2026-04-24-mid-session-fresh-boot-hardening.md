# Implementation Plan: F&O Universe Rebuild — 3 Indices Full Chain + 216 Stocks ATM±25

**Status:** VERIFIED
**Date:** 2026-04-25
**Approved by:** Parthiban ("yes all correct go ahead with the implementation dude okay?")
**Branch:** `claude/build-fno-universe-Tlb9d`
**Commits:** e115cfa (Phase 1), e404603 (Phase 2), 44bf076 (Phase 3), 0c386ac (Phase 4)

## Goal

Rewrite the F&O subscription universe to fit comfortably in the 25,000 WebSocket capacity by:

1. **Indices** — Drop FINNIFTY + MIDCPNIFTY entirely. Subscribe full current-expiry chain (no strike cap) for **NIFTY + BANKNIFTY + SENSEX only**.
2. **Stocks** — Subscribe **ATM±25 CE + ATM±25 PE + 1 FUT** of current expiry only for all 216 F&O stocks.
3. **Bootstrap** — Detect 4 boot modes (Pre-market / Mid-pre-market / Mid-market / Post-market) and use **live cash-equity ticks as PRIMARY spot source** for Mode C, REST `/marketfeed/ltp` only as straggler fallback.

## Capacity Math (verified against live QuestDB on 2026-04-25)

| Bucket | Underlyings | CE | PE | FUT | Total |
|---|---:|---:|---:|---:|---:|
| INDEX F&O (full current expiry) | 3 | 1,017 | 1,017 | 3 | 2,037 |
| STOCK F&O (ATM±25, current expiry) | 216 | 10,913 | 10,913 | 216 | 22,042 |
| Index values (IDX_I 13/25/51) | 3 | — | — | — | 3 |
| Display indices (Ticker mode) | 26 | — | — | — | 26 |
| Cash equities NSE_EQ | 216 | — | — | — | 216 |
| **TOTAL** | | | | | **24,324** |
| **HARD LIMIT** | | | | | **25,000** |
| **HEADROOM** | | | | | **676** ✅ |

## Plan Items

### Phase 1 — Constants + Planner (foundation)

- [x] **Item 1:** Update `crates/common/src/constants.rs`
  - Files: `crates/common/src/constants.rs`
  - Changes: `FULL_CHAIN_INDEX_SYMBOLS` → 3 entries (drop FINNIFTY, MIDCPNIFTY); `FULL_CHAIN_INDEX_COUNT` = 3; `VALIDATION_MUST_EXIST_INDICES` → 3 entries; new `STOCK_OPTION_ATM_STRIKES_EACH_SIDE = 25`; new `MAX_TOTAL_SUBSCRIPTIONS_TARGET = 24_500` (warn threshold)
  - Tests: `test_full_chain_index_symbols_is_three`, `test_validation_must_exist_indices_is_three`, `test_stock_option_atm_strikes_each_side_is_25`, `test_max_total_subscriptions_target_below_hard_limit`

- [x] **Item 2:** Update `crates/core/src/instrument/subscription_planner.rs` — Index F&O current-expiry filter
  - Files: `subscription_planner.rs`
  - Changes: Index F&O branch (lines 249–269) — add `expiry_date >= today` filter; for each index, find `min(expiry_date)` then include only contracts at that nearest expiry
  - Tests: `test_index_derivatives_use_current_expiry_only`, `test_far_month_index_contracts_excluded`

- [x] **Item 3:** Update `crates/core/src/instrument/subscription_planner.rs` — Stock ATM±25 cap
  - Files: `subscription_planner.rs`
  - Changes: Stock Stage 1 (lines 289–453) — replace configurable `stock_atm_strikes_above/below` with constant `STOCK_OPTION_ATM_STRIKES_EACH_SIDE = 25`; cap at 25 above + 25 below = 51 strikes per side total
  - Tests: `test_stock_options_capped_at_atm_pm_25`, `test_stock_with_50_strikes_capped_at_25_each_side`

- [x] **Item 4:** Add capacity ratchet test to `subscription_planner.rs`
  - Files: `subscription_planner.rs`
  - Tests: `test_total_subscription_count_below_25k_hard_limit`, `test_finnifty_midcpnifty_dropped_from_index_set`

- [x] **Item 5:** Run scoped tests — `cargo test -p tickvault-common -p tickvault-core` (Common changed → workspace escalation per testing-scope.md)
  - Verify: 0 failures, all new ratchets pass

- [x] **Item 6:** Commit + push Phase 1
  - Commit msg: `refactor(planner): drop FINNIFTY+MIDCPNIFTY, current-expiry indices, stock ATM±25`

### Phase 2 — Boot Mode Detection + Live-Tick ATM Resolver

- [x] **Item 7:** Create `crates/core/src/instrument/boot_mode.rs`
  - Files: `boot_mode.rs` (NEW), `crates/core/src/instrument/mod.rs`
  - Changes: `enum BootMode { PreMarket, MidPreMarket, MidMarket, PostMarket }`; `pub fn detect_boot_mode(now_ist_secs: u32) -> BootMode`; pure function, no I/O
  - Tests: `test_boot_mode_pre_market_before_0900`, `test_boot_mode_mid_pre_market_0900_to_0913`, `test_boot_mode_mid_market_0913_to_1530`, `test_boot_mode_post_market_after_1530`, `test_boot_mode_boundary_0900_exact`, `test_boot_mode_boundary_0913_exact`, `test_boot_mode_boundary_1530_exact`

- [x] **Item 8:** Extend `crates/core/src/pipeline/tick_processor.rs` to update `SharedSpotPrices` for cash equities
  - Files: `tick_processor.rs`
  - Changes: Currently only NIFTY/BANKNIFTY/SENSEX IDX_I ticks update spot prices. Extend to also write cash-equity NSE_EQ ticks for the 216 F&O stock underlyings.
  - Tests: `test_cash_equity_tick_updates_shared_spot_prices`, `test_idx_i_tick_still_updates_shared_spot_prices`

- [x] **Item 9:** Create `crates/core/src/instrument/live_tick_atm_resolver.rs`
  - Files: `live_tick_atm_resolver.rs` (NEW), `crates/core/src/instrument/mod.rs`
  - Changes: Async resolver that polls `SharedSpotPrices` for all 216 F&O stock cash-equity SIDs at progressive intervals (5s, 10s, 15s, 20s, 25s); returns `ResolveResult { resolved: HashMap<UnderlyingSymbol, f64>, stragglers: Vec<UnderlyingSymbol> }`; falls through to existing REST `preopen_rest_fallback` for stragglers, then to QuestDB previous close
  - Tests: `test_resolver_returns_all_when_all_ticked`, `test_resolver_returns_stragglers_when_silent`, `test_resolver_progressive_timeout_5_10_15_20_25`, `test_resolver_exit_early_when_all_resolved`, `test_resolver_o1_lookup_per_stock` (DHAT zero-alloc)

- [x] **Item 10:** Run scoped tests `cargo test -p tickvault-core`
  - Verify: 0 failures

- [x] **Item 11:** Commit + push Phase 2
  - Commit msg: `feat(boot): add boot_mode detection + live_tick_atm_resolver`

### Phase 3 — Main.rs Wiring + Notification Events

- [x] **Item 12:** Drop FINNIFTY/MIDCPNIFTY from main.rs depth arrays
  - Files: `crates/app/src/main.rs` (lines 2151, 3113, 3685)
  - Changes: Replace `["NIFTY", "BANKNIFTY", "FINNIFTY", "MIDCPNIFTY"]` with `["NIFTY", "BANKNIFTY"]` at all 3 sites
  - Tests: `test_main_depth_arrays_only_nifty_banknifty` (source-scan guard)

- [x] **Item 13:** Wire BootMode into main.rs Step 8c
  - Files: `crates/app/src/main.rs`
  - Changes: Branch on `detect_boot_mode()` — Mode A/B → existing pre-open buffer + 09:13 dispatch; Mode C → spawn `live_tick_atm_resolver` task immediately after WS connect; Mode D → use QuestDB previous close
  - Tests: `test_main_branches_on_boot_mode` (source-scan guard)

- [x] **Item 14:** Add `MidMarketBootComplete` notification event
  - Files: `crates/core/src/notification/events.rs`
  - Changes: New variant `MidMarketBootComplete { mode: BootMode, stocks_live_tick: usize, stocks_rest: usize, stocks_quest_db: usize, stocks_skipped: usize, total_subscribed: usize, latency_ms: u64 }`
  - Tests: `test_midmarket_boot_complete_severity_is_info`, `test_midmarket_boot_complete_includes_breakdown`

- [x] **Item 15:** Run scoped tests `cargo test -p tickvault-core -p tickvault-app`
  - Verify: 0 failures

- [x] **Item 16:** Commit + push Phase 3
  - Commit msg: `feat(boot): wire boot_mode + drop FINNIFTY/MIDCPNIFTY arrays`

### Phase 4 — Dashboards + Rules + Runbook

- [x] **Item 17:** Update `deploy/docker/grafana/dashboards/depth-flow.json`
  - Files: `depth-flow.json`
  - Changes: Drop FINNIFTY + MIDCPNIFTY panels/labels; description says "2 indices" not "4"
  - Tests: `grafana_dashboard_snapshot_filter_guard.rs` (existing — must still pass)

- [x] **Item 18:** Update `.claude/rules/project/depth-subscription.md`
  - Files: `depth-subscription.md`
  - Changes: 2-index policy section (NIFTY + BANKNIFTY only); ATM strike selector unchanged (still ±24 each side for depth); FINNIFTY/MIDCPNIFTY explicitly noted as DROPPED

- [x] **Item 19:** Update `.claude/rules/project/live-market-feed-subscription.md`
  - Files: `live-market-feed-subscription.md`
  - Changes: 4-mode boot section; `STOCK_OPTION_ATM_STRIKES_EACH_SIDE = 25` policy; live-tick primary / REST fallback architecture; 3-index full-chain policy

- [x] **Item 20:** Create `.claude/rules/project/disaster-recovery.md`
  - Files: `disaster-recovery.md` (NEW)
  - Changes: Document all 4 boot modes + 11 disaster scenarios + state-source hierarchy + idempotency guarantees

- [x] **Item 21:** Run scoped tests + meta-guards
  - Verify: `cargo test -p tickvault-storage` (dashboard guard), all `.claude/rules/` cross-references valid

- [x] **Item 22:** Commit + push Phase 4
  - Commit msg: `docs: 3-index policy + 4-mode boot + disaster recovery runbook`

### Phase 5 — Final Verification + PR

- [x] **Item 23:** Run full scoped pre-push gate `bash .claude/hooks/pre-push-gate.sh`
  - Verify: All 12 fast gates pass

- [x] **Item 24:** Run plan-verify `bash .claude/hooks/plan-verify.sh`
  - Verify: All plan items checked, all tests exist, all files modified

- [x] **Item 25:** Mark plan VERIFIED, open draft PR
  - Branch: `claude/build-fno-universe-Tlb9d` → `main`

## Disaster Recovery Scenarios

| # | Scenario | Detection | Recovery | RTO |
|---|---|---|---|---|
| 1 | Fresh clone + Docker fresh + boot at 11:30 IST | `detect_boot_mode()` returns `MidMarket` | Mode C: live-tick resolver → REST stragglers → QuestDB fallback | ~30s |
| 2 | Docker deleted mid-market, restart | Same as #1 | Universe rebuilds from rkyv binary cache (10ms); Mode C resolver | ~30s |
| 3 | QuestDB lost (deleted volume) | DDL detects empty schema → CSV downloader runs | CSV download + parse + persist + cache write | ~60s |
| 4 | Valkey lost | Token cache miss → SSM fallback | Trading continues; cache repopulates | Zero impact |
| 5 | Single WS connection drops mid-day | Pool watchdog (5s tick) sees state ≠ Connected | Reconnect with `SubscribeRxGuard` (PR #337) | ~5–10s |
| 6 | All 5 WS connections drop | Watchdog sees `active_count = 0` | All 5 reconnect in parallel; SubscribeRxGuard restores subs | ~30s |
| 7 | Auth token expired mid-market (807) | `DisconnectCode::AccessTokenExpired` | Token refresh (SSM + TOTP) → reconnect — AUTH-GAP-02 | ~15s |
| 8 | Network blip (RST flood like 2026-04-24) | Connection read-loop `Err` + watchdog | Existing exponential backoff + SubscribeRxGuard | ~10–30s |
| 9 | Dhan REST `/marketfeed/ltp` returns 805 | Per-call status check | 60s STOP_ALL pause → retry → fall back to QuestDB previous close | ~60s |
| 10 | F&O stock has no LTP anywhere (REST empty + no QuestDB history) | Phase2Failed event with diagnostic | Skip that stock for the day, log + Telegram | Continuous |
| 11 | Capacity overflow (computed plan > 25,000) | Pre-subscribe assertion | Build fails fast, Telegram CRITICAL, operator manual | Manual |

## State Sources Hierarchy (cold-start)

| State | Primary | Fallback 1 | Fallback 2 | Fallback 3 |
|---|---|---|---|---|
| Instrument master | rkyv binary cache (disk) | QuestDB `derivative_contracts` | Dhan CSV download | S3 backup |
| Auth token | Valkey cache | AWS SSM | TOTP regeneration | HALT + Telegram |
| Stock spot price | Pre-open buffer (Mode A/B) | Live cash-equity tick (Mode C) | REST `/marketfeed/ltp` | QuestDB previous close |
| Index spot price | Live IDX_I WS tick | REST `/marketfeed/ltp` | Pre-open buffer | QuestDB previous close |
| Universe build status | Valkey cache | QuestDB `instrument_build_metadata` | Force rebuild from CSV | — |

## O(1) / Uniqueness / Dedup Guarantees

| Guarantee | Mechanism | Proof Test |
|---|---|---|
| O(1) per-tick lookup | `InstrumentRegistry.by_composite: HashMap<(SecurityId, ExchangeSegment), _>` (papaya) | `test_registry_o1_lookup` (existing) |
| Uniqueness (composite key) | `HashSet<(u32, ExchangeSegment)>` in planner dedup (I-P1-11) | `test_regression_seen_ids_key_type_is_pair` (existing) |
| QuestDB tick dedup | `DEDUP_KEY_TICKS = "security_id, exchange_segment, ts, sequence_number"` (STORAGE-GAP-01) | `test_tick_dedup_key_includes_segment` (existing) |
| QuestDB derivative dedup | `DEDUP_KEY_DERIVATIVE_CONTRACTS = "security_id, underlying_symbol, exchange_segment"` (I-P1-05) | `test_dedup_key_derivative_contracts_includes_underlying` (existing) |
| Order idempotency | Valkey UUID v4 key (OMS-GAP-05) | `test_idempotency_*` (existing) |
| Boot mode determinism | Pure function `detect_boot_mode(now_ist_secs)` | `test_boot_mode_at_each_time_window` (NEW) |
| ATM determinism | Pure function `nearest_strike(spot, sorted_strikes)` | existing planner tests |
| Subscription idempotency | Re-subscribing same security_id on same conn = Dhan no-op | architectural |
| Capacity hard cap | Compile/boot-time assertion `total ≤ MAX_TOTAL_SUBSCRIPTIONS` | `test_total_subscription_count_below_25k_hard_limit` (NEW) |

## Real-Time Checks (live, in-production)

| Check | Frequency | Action on Fail |
|---|---|---|
| Active WS connections gauge | 5s (pool watchdog) | Telegram alert if `active < 5` |
| Capacity utilization gauge | At Phase 2 dispatch | Telegram WARN if `total > 24_500` |
| `tv_instrument_registry_cross_segment_collisions` | At boot + on rebuild | Already-existing telemetry |
| Subscription audit log | Every subscribe message | QuestDB `subscription_audit_log` table |
| Live tick freshness per stock | 5s during Mode C resolver | Mark straggler if no tick in 25s |
| `/health` endpoint counters | On-demand | Verifies main_feed_active, depth_active |
| 09:13:00 Phase 2 outcome | Once per trading day | `Phase2Complete` or `Phase2Failed` Telegram |
| 09:13:00 depth anchor | Once per trading day per index | `MarketOpenDepthAnchor` Telegram |
| 09:15:30 streaming heartbeat | Once per trading day | `MarketOpenStreamingConfirmation` Telegram |

## Tests Total

| Crate | New tests | Existing ratchets touched |
|---|---:|---:|
| `tickvault-common` | 4 | constants tests |
| `tickvault-core` | 18 | subscription_planner + tick_processor + registry |
| `tickvault-app` | 2 | source-scan guards |
| `tickvault-storage` | 0 | grafana_dashboard_snapshot_filter_guard preserved |
| **TOTAL NEW** | **24** | |

## Files Summary

| Status | File | Phase |
|---|---|---|
| MODIFY | `crates/common/src/constants.rs` | 1 |
| MODIFY | `crates/core/src/instrument/subscription_planner.rs` | 1 |
| NEW | `crates/core/src/instrument/boot_mode.rs` | 2 |
| NEW | `crates/core/src/instrument/live_tick_atm_resolver.rs` | 2 |
| MODIFY | `crates/core/src/instrument/mod.rs` (export new modules) | 2 |
| MODIFY | `crates/core/src/pipeline/tick_processor.rs` | 2 |
| MODIFY | `crates/app/src/main.rs` (lines 2151, 3113, 3685, Step 8c) | 3 |
| MODIFY | `crates/core/src/notification/events.rs` | 3 |
| MODIFY | `deploy/docker/grafana/dashboards/depth-flow.json` | 4 |
| MODIFY | `.claude/rules/project/depth-subscription.md` | 4 |
| MODIFY | `.claude/rules/project/live-market-feed-subscription.md` | 4 |
| NEW | `.claude/rules/project/disaster-recovery.md` | 4 |

**Total: 8 modified + 4 new = 12 files**

## Rollback

If anything blocks: `git revert HEAD` per phase. Phase 1 is fully reversible (planner constants). Phase 2 adds new modules — safe (not yet wired). Phase 3 wires boot mode — revert this commit reverts to existing 5-index full-chain (broken, exceeds 25K) but recoverable. Phase 4 is doc-only.
