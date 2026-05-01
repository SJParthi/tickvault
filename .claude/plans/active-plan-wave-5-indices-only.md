# Implementation Plan: Wave 5 — Indices-Only Subscription + Depth Redesign + Resilience Hardening

**Status:** DRAFT
**Date:** 2026-05-01
**Approved by:** pending (Parthiban — operator)
**Branch:** `claude/fetch-status-info-V56GN`
**Triggering context:** Operator decision 2026-05-01: drop the 216-stock F&O subscription, focus exclusively on NIFTY/BANKNIFTY/SENSEX indices (all expiries, all strikes), redesign depth-20 + depth-200 around major-index single-side connections + dynamic top-volume gainers, wire core_affinity, fix two hot-path bugs caught by adversarial review.

## Honest Charter (per `wave-4-shared-preamble.md` §2 + §8)

> "100% inside the tested envelope, with ratcheted regression coverage: ≤60s QuestDB outage absorbed by rescue→spill→DLQ; ≤600K rescue ring capacity; bench-gated O(1) hot path; composite-key uniqueness; chaos-tested 70h sleep/wake. Beyond the envelope, DLQ NDJSON catches every payload as recoverable text."

No literal "never disconnect" / "never fail" claims. TCP, kernel, remote processes, and disks can fail; we DETECT, ABSORB, AUDIT.

## Scope Summary

| Aspect | Today (Wave 4) | Wave 5 Plan |
|---|---|---|
| F&O subscription scope | 216 stocks ATM±25 + 3 indices full chain (~24,324) | NIFTY+BANKNIFTY+SENSEX all expiries/all strikes only (~10,783 F&O + 206 cash + 29 IDX_I = **11,018**) |
| Main-feed connections | 5 conns @ 97% cap, segment-bucket | 5 conns @ 44% cap, **category-balanced round-robin** (deterministic by security_id) |
| Depth-20 connections | 4 (NIFTY+BANKNIFTY mixed CE+PE), 98 instruments, ATM±24 | **5 conns: 4 single-side index + 1 top-50 volume-gainers**, 246 instruments |
| Depth-200 connections | 4 static (NIFTY/BANKNIFTY ATM CE/PE) | **5 dynamic top-5 volume-gainers** (1 per conn), 60s rebalance |
| Tokio worker affinity | 4 floating workers across 4 vCPUs | **4 workers pinned to dedicated cores** (0=WS, 1=pipeline, 2=ILP, 3=other) |
| candle_aggregator key | `HashMap<u32, _>` keyed on security_id alone | `HashMap<(u32, ExchangeSegment), _>` (I-P1-11) |
| tick_persistence flush failure | `warn!` (no Telegram) | `error!` (Loki → Telegram) per audit Rule 5 |

## Verified Live Numbers (QuestDB query 2026-04-30)

| Slot | Segment | Source | Count |
|---|---|---|---|
| Major index values | IDX_I | NIFTY=13, BANKNIFTY=25, SENSEX=51 | 3 |
| Display indices | IDX_I | sectoral + INDIA VIX | 26 |
| Cash equities | NSE_EQ | F&O underlying stocks (kept) | ~206 |
| F&O derivatives | NSE_FNO + BSE_FNO | NIFTY+BANKNIFTY+SENSEX, all expiries through 2030 | 10,783 |
| **TOTAL** | | | **~11,018** |

Equal-split per conn: 11,018 ÷ 5 = **2,204** (44% of Dhan's 5K/conn cap → 2.3× headroom).

## Depth-20 Split (5 conns, NSE-only)

| Conn | Underlying | Side | Strikes | Count | Source |
|---:|---|:---:|---|---:|---|
| 1 | NIFTY current expiry | CE only | ATM ±24 | 49 | depth_strike_selector |
| 2 | NIFTY current expiry | PE only | ATM ±24 | 49 | depth_strike_selector |
| 3 | BANKNIFTY current expiry | CE only | ATM ±24 | 49 | depth_strike_selector |
| 4 | BANKNIFTY current expiry | PE only | ATM ±24 | 49 | depth_strike_selector |
| 5 | Top 50 volume-gainers | mixed | dynamic 60s | 50 | option_movers (volume DESC, change_pct > 0) |
| **Total** | | | | **246** | |

**SENSEX excluded from depth** — Dhan rule 13 (`full-market-depth.md`): "Only NSE segments valid. NSE_EQ and NSE_FNO only. BSE, MCX, Currency are NOT available."

±24 locked (not ±25): Dhan caps 50 instruments per depth conn; ±25 = 51 → rejected.

## Depth-200 Split (5 conns × 1 instrument)

| Conn | Instrument | Source |
|---:|---|---|
| 1 | Top volume-gainer #1 | option_movers (volume DESC, change_pct > 0) |
| 2 | Top volume-gainer #2 | dynamic 60s rebalance |
| 3 | Top volume-gainer #3 | dynamic 60s rebalance |
| 4 | Top volume-gainer #4 | dynamic 60s rebalance |
| 5 | Top volume-gainer #5 | dynamic 60s rebalance |

Replaces static NIFTY ATM CE / NIFTY ATM PE / BANKNIFTY ATM CE / BANKNIFTY ATM PE.

## CPU Pinning (c7i.xlarge, 4 vCPU / 8 GB)

| Core | Workload | Why pinned |
|:---:|---|---|
| 0 | WS read loops + parser (5 main feed conns) | Hot path, must never preempt |
| 1 | Pipeline (tick_processor SPSC + candle aggregator + indicators) | 21-timeframe candle work |
| 2 | QuestDB ILP writer + rescue ring drain | Must drain ≥10K tps |
| 3 | API server, observability, auth, OMS, depth feeds, audit writers | Best-effort, degradation here doesn't block ticks |

## Plan Items (10)

Each item carries the 9-box checklist per `stream-resilience.md` B8: ① typed event, ② ErrorCode, ③ tracing+code field, ④ Prometheus counter, ⑤ Grafana panel, ⑥ alert rule, ⑦ call site, ⑧ triage YAML rule, ⑨ ratchet test.

### - [ ] 1. `subscription.scope` config gate

- Files: `crates/common/src/config.rs`, `config/base.toml`
- Tests: `test_subscription_scope_enum_indices_only_all_expiries_default`, `test_subscription_scope_round_trips_via_figment`
- Add enum `SubscriptionScope::{IndicesOnlyAllExpiries, FullUniverse}`. Default = `IndicesOnlyAllExpiries`.
- 9-box: ① N/A (config) ② N/A ③ N/A ④ `tv_subscription_scope` info-gauge ⑤ Operator Health header ⑥ N/A ⑦ `subscription_planner::build_subscription_plan` ⑧ N/A ⑨ enum tests + figment round-trip

### - [ ] 2. Universe filter — keep 11,018 instruments

- Files: `crates/core/src/instrument/subscription_planner.rs`
- Tests: `test_indices_only_scope_filters_to_three_underlyings`, `test_universe_count_pinned_at_11018`, `test_finnifty_midcpnifty_excluded_from_indices_only`, `test_stock_fno_excluded_under_indices_only_scope`
- Predicate: `instrument.underlying_symbol IN ('NIFTY','BANKNIFTY','SENSEX')` for derivatives. Cash equities + IDX_I unchanged.
- Drops Phase 2 dispatcher (09:13 IST) + Mode C live-tick ATM resolver + pre-open REST `/marketfeed/ltp` fallback to inert (no stock F&O).
- 9-box: ① `Phase2Skipped` (new, Severity::Info, fires once at 09:13:00 explaining "no stock F&O under indices-only scope") ② N/A (no failure path) ③ tracing `info!(scope = "indices_only", count = 11018)` at boot ④ `tv_subscription_total_instruments` gauge ⑤ Operator Health "Subscription scope" panel ⑥ `tv-subscription-count-drift` (alert if `tv_subscription_total_instruments` outside 10,500..11,500) ⑦ `main.rs` boot sequence ⑧ N/A ⑨ count-pinned test

### - [ ] 3. Main feed equal-split (5 × ~2,204, category-balanced round-robin)

- Files: `crates/core/src/websocket/connection_pool.rs`, `crates/core/src/instrument/subscription_distribution.rs` (new)
- Tests: `test_distribution_is_category_balanced_round_robin`, `test_same_security_id_lands_on_same_connection_across_runs`, `test_distribution_per_conn_within_5_pct_of_target`, `test_distribution_idempotent_on_replay`
- Algorithm: group by [IDX_I, NSE_EQ, NSE_FNO+BSE_FNO]; sort each group by security_id ASC; `conn_index = i % 5`. Stable across boots.
- 9-box: ① N/A ② N/A ③ tracing `info!(conn = i, count = n)` per conn at boot ④ `tv_main_feed_per_conn_instrument_count` gauge with `{conn}` label ⑤ Operator Health "Main feed distribution" stacked-bar panel ⑥ `tv-main-feed-conn-overload` (any conn > 4,500) ⑦ `connection_pool::distribute` ⑧ N/A ⑨ deterministic-replay test + spread test

### - [ ] 4. Depth-20 5-conn split (4 single-side index + 1 top-50 gainers)

- Files: `crates/core/src/websocket/depth_connection.rs`, `crates/core/src/instrument/depth_strike_selector.rs`, `crates/core/src/instrument/depth_20_top_gainers.rs` (new), `crates/app/src/main.rs`
- Tests: `test_depth_20_conn_1_is_nifty_ce_atm_24`, `test_depth_20_conn_2_is_nifty_pe_atm_24`, `test_depth_20_conn_3_is_banknifty_ce_atm_24`, `test_depth_20_conn_4_is_banknifty_pe_atm_24`, `test_depth_20_conn_5_is_top_50_volume_gainers`, `test_depth_20_total_under_dhan_50_per_conn_cap`, `test_depth_20_excludes_sensex`
- Top-50 selector queries `option_movers` every 60s: `SELECT security_id FROM option_movers WHERE change_pct > 0 ORDER BY volume DESC LIMIT 50`. Edge-triggered swap via existing `DepthCommand::Swap20`.
- 9-box: ① `Depth20TopSetEmpty` (existing DEPTH-DYN-01 reused — fires when result < 50) + new `Depth20TopGainersSwapped` (Severity::Low, edge-triggered on rank change) ② `DEPTH-20-DYN-03` (top-50 selector empty/below capacity, severity High) ③ `error!(code = ErrorCode::Depth20Dyn03TopGainersEmpty.code_str())` ④ `tv_depth_20_top_gainers_set_size` gauge, `tv_depth_20_top_gainers_swaps_total` counter ⑤ Operator Health "Depth-20 top-50 gainers" panel ⑥ `tv-depth-20-dyn-03-empty-set` (gauge < 25 for > 5min during market hours) ⑦ `main.rs::run_depth_20_top_gainers_loop` ⑧ `.claude/triage/error-rules.yaml::depth-20-dyn-03-top-set-empty-escalate` ⑨ all 7 ratchet tests above + `test_top_50_query_filters_change_pct_positive`

### - [ ] 5. Depth-200 dynamic top-5 (replaces static ATM CE/PE)

- Files: `crates/core/src/websocket/depth_connection.rs`, `crates/core/src/instrument/depth_200_top_gainers.rs` (new), `crates/app/src/main.rs`
- Tests: `test_depth_200_picks_top_5_by_volume`, `test_depth_200_filters_change_pct_positive`, `test_depth_200_one_instrument_per_conn`, `test_depth_200_swap_on_rank_change`, `test_depth_200_excludes_sensex`
- Replaces existing static `["NIFTY", "BANKNIFTY"]` ATM CE/PE config in `main.rs:2151,3113,3685`. Uses existing `DepthCommand::Swap200` path. 60s rebalance from `option_movers`.
- 9-box: ① new `Depth200TopGainersSwapped` (Severity::Low, edge-triggered) + reuse `Depth200SwapChannelBroken` (existing DEPTH-DYN-02) ② `DEPTH-200-DYN-01` (top-5 selector returned < 5, severity High) ③ `error!(code = ErrorCode::Depth200Dyn01TopSetEmpty.code_str())` ④ `tv_depth_200_top_gainers_set_size`, `tv_depth_200_top_gainers_swaps_total` ⑤ Operator Health "Depth-200 top-5" panel ⑥ `tv-depth-200-dyn-01-empty-set` ⑦ `main.rs::run_depth_200_top_gainers_loop` ⑧ `.claude/triage/error-rules.yaml::depth-200-dyn-01-top-set-empty-escalate` ⑨ all 5 tests above

### - [ ] 6. Wire `core_affinity` — pin 4 Tokio workers to 4 vCPUs

- Files: `crates/app/src/main.rs`, `crates/app/src/runtime.rs` (new), `Cargo.toml` (already has core_affinity 0.8.3)
- Tests: `test_core_affinity_actually_pinned_via_proc_status`, `test_runtime_builder_sets_worker_count_to_vcpu_count`, `test_pinning_skipped_gracefully_on_single_vcpu_host`, `test_each_worker_has_unique_cpu_set`
- Use `tokio::runtime::Builder::new_multi_thread().worker_threads(4).on_thread_start(|| { core_affinity::set_for_current(...) })`. Read `/proc/self/task/*/status` post-boot; assert `Cpus_allowed_list` is single-cpu per worker.
- 9-box: ① `CorePinningFailed` (Severity::High) ② `CORE-PIN-01` (pinning failed at boot, severity High), `CORE-PIN-02` (worker drifted off pinned core, severity Medium) ③ `error!(code = ErrorCode::CorePin01PinningFailedAtBoot.code_str())` ④ `tv_core_pinning_workers_pinned_total` gauge (expect = 4) ⑤ Operator Health "Core affinity" panel ⑥ `tv-core-pin-01-pinning-failed` (gauge != 4 at boot) ⑦ `main.rs::build_runtime` ⑧ `.claude/triage/error-rules.yaml::core-pin-01-pinning-failed-at-boot-escalate` ⑨ all 4 tests above + boot-time assertion

### - [ ] 7. Fix CRITICAL: candle_aggregator segment-aware key (I-P1-11)

- Files: `crates/core/src/pipeline/candle_aggregator.rs:12,21` (and any other site)
- Tests: `test_candle_aggregator_keyed_on_security_id_and_segment`, `test_two_instruments_same_id_different_segment_do_not_merge_ohlcv` (regression for FINNIFTY=27 IDX_I vs NSE_EQ=27 collision)
- Migrate `HashMap<u32, OhlcvState>` → `HashMap<(u32, ExchangeSegment), OhlcvState>`. Update banned-pattern scanner glob to include `crates/core/src/pipeline/candle_aggregator.rs`.
- 9-box: ① N/A ② reuse I-P1-11 ③ N/A (lookup, no error) ④ `tv_candle_aggregator_keyspace_size` gauge ⑤ existing I-P1-11 panel ⑥ N/A ⑦ `tick_processor::on_tick` ⑧ N/A ⑨ regression test on collision pair

### - [ ] 8. Fix HIGH: `warn!` → `error!` at tick_persistence.rs:357

- Files: `crates/storage/src/tick_persistence.rs:357`
- Tests: `crates/storage/tests/error_level_meta_guard.rs` (existing meta-guard catches new violations going forward; one-time fix is the line itself)
- Add `code = ErrorCode::StorageGap03AuditWriteFailure.code_str()` field to satisfy tag-guard.
- 9-box: ① existing typed event ② STORAGE-GAP-03 (existing) ③ added in this fix ④ existing counter ⑤ existing panel ⑥ existing alert ⑦ tick_persistence flush call site ⑧ existing triage rule ⑨ meta-guard catches future regressions

### - [ ] 9. New ErrorCode variants (4)

- Files: `crates/common/src/error_code.rs`, `.claude/rules/project/wave-5-error-codes.md` (new)
- Tests: existing `error_code_rule_file_crossref.rs` + `error_code_tag_guard.rs` cover all 4 automatically
- Variants:
  - `CorePin01PinningFailedAtBoot` → `code_str() = "CORE-PIN-01"`, Severity::High, runbook `wave-5-error-codes.md`
  - `CorePin02WorkerDrifted` → `"CORE-PIN-02"`, Severity::Medium
  - `Depth20Dyn03TopGainersEmpty` → `"DEPTH-20-DYN-03"`, Severity::High
  - `Depth200Dyn01TopGainersEmpty` → `"DEPTH-200-DYN-01"`, Severity::High (reuse if existing variant of same `code_str()` already exists; check before adding)
- 9-box: ① N/A ② self ③ N/A ④ N/A ⑤ N/A ⑥ N/A ⑦ added by Items 4/5/6 ⑧ entries in `error-rules.yaml` per Item 4/5/6 ⑨ enum invariant tests + cross-ref test + tag-guard

### - [ ] 10. Adversarial 3-agent re-review on the diff

- Spawn `hot-path-reviewer`, `security-reviewer`, `general-purpose` (hostile bug-hunt) in parallel against the final diff before opening PR. Per `wave-4-shared-preamble.md` Section 3.
- Fix every CRITICAL and HIGH inline. Document every false-positive triage with grep evidence.

## Verification (before PR)

```bash
cargo check --workspace
cargo test -p tickvault-common --lib   # ErrorCode invariants
cargo test -p tickvault-core --lib     # subscription_planner + depth selectors
cargo test -p tickvault-storage --lib  # tick_persistence flush guard
cargo test -p tickvault-app --lib      # core_affinity pinning + runtime
FULL_QA=1 make scoped-check
bash .claude/hooks/banned-pattern-scanner.sh
bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all
bash .claude/hooks/pub-fn-wiring-guard.sh "$PWD"
bash .claude/hooks/plan-verify.sh
cargo bench                            # hot path touched (Item 7)
cargo test --features dhat             # zero hot-path allocations
```

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot at 09:00 IST with `subscription.scope = "indices_only_all_expiries"` | Universe loads 11,018; 5 main-feed conns at ~2,204 each; depth-20 wires 5 conns (4 NIFTY/BANKNIFTY single-side + 1 top-50); depth-200 wires 5 conns to top-5 gainers; 4 Tokio workers pinned to cores 0-3 |
| 2 | option_movers returns < 50 for top-50 query during market hours | DEPTH-20-DYN-03 fires `Severity::High` Telegram with `returned_count` + reason; surviving conn 5 keeps last good set |
| 3 | option_movers returns < 5 for top-5 query during market hours | DEPTH-200-DYN-01 fires `Severity::High`; surviving 5 conns keep last good gainers |
| 4 | core_affinity::set_for_current returns false on one worker | CORE-PIN-01 fires `Severity::High`; gauge `tv_core_pinning_workers_pinned_total < 4` |
| 5 | candle_aggregator receives tick with security_id=27 from IDX_I + tick with security_id=27 from NSE_EQ | OHLCV state stays in two distinct keys; regression test pinned |
| 6 | tick_persistence flush fails | `error!` (not `warn!`) emitted with `code = "STORAGE-GAP-03"`; Loki routes to Telegram via Alertmanager |
| 7 | One main-feed conn drops mid-day | Surviving 4 absorb 11,018 instruments at 55%/conn (still < 5K cap); SubscribeRxGuard reinstates subscriptions |
| 8 | Two main-feed conns drop simultaneously | Surviving 3 absorb at 73%/conn; 1,328 free slots remain — no Dhan rejection |
| 9 | Top-50 ranking changes (rank 47 falls out, new entry rises) | Single `Swap20` command on conn 5; `Depth20TopGainersSwapped` Severity::Low Telegram (edge-triggered) |
| 10 | Tokio worker drifts off pinned core | CORE-PIN-02 fires `Severity::Medium`; counter `tv_core_pinning_drift_total` increments |

## Open Questions for Operator

| # | Question | Default if unanswered |
|---|---|---|
| 1 | Top-50 / Top-5 query — current implementation in `option_movers` already filters `change_pct > 0`. Should we ALSO require `volume > 0` to avoid zero-volume contracts entering the set? | Yes (volume DESC implies, but explicit guard is safer) |
| 2 | If on-day-1-boot the `option_movers` table is empty (e.g., first 60s after universe build), should depth-20 conn 5 + depth-200 5 conns: (a) skip subscribe and wait, or (b) fall back to NIFTY/BANKNIFTY ATM as today? | (a) skip + retry every 60s; INFO Telegram once |
| 3 | core_affinity on dev Mac (typically 8-12 cores) vs AWS c7i.xlarge (4 vCPUs) — do we hard-fail boot on Mac if `vcpu_count != 4`? | No, pin to first 4; Mac is dev-only |

## Notes

- Phase 2 dispatcher (09:13 IST) becomes inert under indices-only scope — keep code path, gate on `config.subscription.scope`. Do NOT delete; reactivating full universe must be a 1-line config flip.
- `MidMarketBootComplete` event becomes inert (no stock F&O Mode C resolution needed). Keep variant; rename trigger condition.
- Pre-open buffer + REST fallback (`preopen_rest_fallback.rs`) becomes inert for stocks. Indices still use it.
- DEDUP keys + I-P1-11 composite-key invariants UNCHANGED. New code MUST use `(security_id, exchange_segment)`.
