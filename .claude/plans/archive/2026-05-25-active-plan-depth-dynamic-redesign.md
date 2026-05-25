# Implementation Plan: Depth-20 + Depth-200 All-Dynamic Top-Volume Redesign + Saturday Post-Market Gate

**Status:** APPROVED (PR-A SHIPPED; PR-B SHIPPED; PR-C1 SHIPPED [dead code]; PR-D SHIPPED [observability ready]; PR-C2 cutover PENDING operator approval)
**Date:** 2026-05-02
**Approved by:** Parthiban — verbatim "go ahead as per the recommendation" 2026-05-02 ~10:35 IST
**Branch:** `claude/refine-stock-selection-71PnY`
**Triggering observation:** Live Telegram on Saturday 2026-05-02 fired `[HIGH] Post-Market: Market closed — WebSockets disconnected, API stays up` at 15:30 IST despite the day not being a trading day; same session operator approved redesign of depth-20 (4 pinned + 1 dynamic) and depth-200 (5 dynamic top-5) into a unified all-dynamic top-volume×%change-DESC selector with **incremental diff-based resubscribe** (only the changed SID gets unsubscribed/subscribed, never the full set).

## Design Summary

### Selector logic — config-driven, runtime-scalable, segment-precise

**Source:** the `movers` family — `movers_1m` materialized view
(60s bucket off the `movers_1s` base table). NOT `option_movers`
(retired). The `movers_*` family is the single source of truth.

**Schema upgrade (mandatory for this redesign):** add a precise
`exchange_segment` column to `movers_1s` so the selector can filter
by `NSE_FNO` / `BSE_FNO` / `NSE_EQ` / `IDX_I` directly without a
post-query InstrumentRegistry lookup. The current `segment` SYMBOL
column (single-char `D/E/I/...`) is too coarse — `D` collapses
NSE_FNO and BSE_FNO together, blocking any future-proof per-exchange
include/exclude.

```sql
-- Idempotent at every boot (per observability-architecture.md schema-self-heal pattern)
ALTER TABLE movers_1s ADD COLUMN IF NOT EXISTS exchange_segment SYMBOL CAPACITY 16 NOCACHE;
-- DEDUP key unchanged: (ts, security_id, segment) — exchange_segment is derived from segment+source
```

The 24 materialized views off `movers_1s` propagate the new column
automatically because they project all base columns. Writer code
in `movers_unified_pipeline.rs` populates `exchange_segment` from
the `InstrumentRegistry` lookup at write time (one lookup per tick,
amortised cost on a non-hot-path 1s tick).

**Selector is a pure data-driven config consumer.** No hardcoded
segment names, no `if symbol == "SENSEX"` blocks. Config drives
everything:

```toml
# config/base.toml — all values runtime-overridable per environment

[depth_20.dynamic]
conns                 = 5         # number of WS connections
sids_per_conn         = 50        # capacity per conn
# total_sids = conns * sids_per_conn = 250 (asserted at boot)

[depth_20.dynamic.universe]
exchange_segments     = ["NSE_FNO"]            # include list — add "BSE_FNO" to expand
cohort_size           = 500                    # top-N by volume before re-rank
rerank_metric         = "change_pct_abs_desc"  # or "change_pct_desc" / "volume_desc"
window_secs           = 60

[depth_200.dynamic]
conns                 = 5
sids_per_conn         = 1

[depth_200.dynamic.universe]
exchange_segments     = ["NSE_FNO"]
cohort_size           = 100
rerank_metric         = "change_pct_abs_desc"
window_secs           = 60
```

```sql
-- SQL is parameterised — no hardcoded segment list
SELECT security_id, segment, exchange_segment, volume, change_pct
FROM movers_1m
WHERE ts > dateadd('s', -:window_secs, now())
  AND exchange_segment IN (:exchange_segments)   -- bound from config
ORDER BY volume DESC
LIMIT :cohort_size;
```

```rust
// Pure-data-driven Rust stage 2 — no segment string literals, no symbol blacklists
fn select_top_k_dynamic(
    cohort: &[MoversRow],
    cfg: &DynamicSelectorConfig,
) -> Vec<SecurityId> {
    let mut ranked: Vec<&MoversRow> = cohort.iter().collect();
    cfg.rerank_metric.sort(&mut ranked);
    ranked.iter()
        .take(cfg.conns * cfg.sids_per_conn)
        .map(|r| r.security_id)
        .collect()
}
```

**Future expansion = config flip, zero code changes:**

| Future change | Config edit |
|---|---|
| Add SENSEX (BSE_FNO derivatives) | `exchange_segments = ["NSE_FNO", "BSE_FNO"]` |
| Add cash-equity stocks to depth-20 dynamic | `exchange_segments = ["NSE_FNO", "NSE_EQ"]` |
| Add index spot tickers | `exchange_segments = [..., "IDX_I"]` |
| Switch from %-change to %-change-abs | `rerank_metric = "change_pct_abs_desc"` |
| Increase to 6 conns × 60 SIDs (360 total) | `conns = 6, sids_per_conn = 60` (after Dhan grants more conns) |
| Tighter window | `window_secs = 30` |
| Wider cohort | `cohort_size = 1000` |

**Asserted invariants at boot (ratchet tests + runtime panics):**
- `conns * sids_per_conn ≤ MAX_DEPTH_20_TOTAL_SIDS` (= 250 today, raised when Dhan limits change)
- `conns ≤ DHAN_DEPTH_20_MAX_CONNECTIONS` (= 5 per `dhan-full-market-depth` rule 3)
- `sids_per_conn ≤ DHAN_DEPTH_20_MAX_SIDS_PER_CONN` (= 50 per rule 3)
- `exchange_segments` non-empty + every entry resolves to a valid `ExchangeSegment` enum variant
- `cohort_size ≥ conns * sids_per_conn` (cohort must be at least as large as final K)
- Same set of asserts for `depth_200` block with `conns = 5, sids_per_conn = 1` ceiling.

**Precise data columns required on `movers_1s` (per operator demand):**

| Column | Type | Purpose |
|---|---|---|
| `ts` | TIMESTAMP | designated, partition key |
| `security_id` | LONG | composite-key part 1 |
| `segment` | SYMBOL | composite-key part 2, single-char `D/E/I/...` (legacy, kept for DEDUP) |
| `exchange_segment` | SYMBOL | **NEW** — precise `NSE_FNO`/`BSE_FNO`/`NSE_EQ`/`IDX_I`/... — selector filter target |
| `volume` | LONG | cumulative session volume |
| `change_pct` | DOUBLE | %-change for re-rank |
| `last_price` | DOUBLE | renderable LTP |
| `prev_close` | DOUBLE | for % computation cross-check |
| `open_interest` | LONG | for OI-based future selectors |
| `oi_delta` | LONG | first derivative |
| `received_at` | TIMESTAMP | wall-clock arrival, forensic |

### Incremental resubscribe (the key piece)

State per feed (depth-20 and depth-200 each have their own copy):

```
struct DynamicSubscriptionState {
    current_set: HashSet<SecurityId>,
    slot_assignment: HashMap<SecurityId, ConnIdx>,  // which conn holds which SID
    free_slots: [u8; 5],  // remaining capacity per conn (50 for d20, 1 for d200)
}
```

Every 60s tick:

1. Compute `next_set` via the selector
2. `to_remove = current_set − next_set`
3. `to_add    = next_set − current_set`
4. For each `sid` in `to_remove`: look up `conn_idx = slot_assignment[sid]`,
   send `DepthCommand::RemoveSubscriptions20(vec![sid])` (or `Remove200`) on
   that conn only; increment `free_slots[conn_idx]`; remove from maps
5. For each `sid` in `to_add`: pick the conn with the most free slots
   (or the conn that just freed one, for locality), send
   `DepthCommand::AddSubscriptions20(vec![sid])` (or `Add200`); decrement
   `free_slots[conn_idx]`; insert into maps
6. **No-op fast path:** if `to_remove.is_empty() && to_add.is_empty()`,
   skip all wire traffic — the previous set is still optimal

**Wire traffic for the canonical 1-SID-changed case:**
- depth-20: 1 RequestCode 25 frame + 1 RequestCode 23 frame, both on the same conn. Other 4 conns and the other 49 SIDs on the affected conn untouched.
- depth-200: 1 RequestCode 25 frame + 1 RequestCode 23 frame on the conn that lost its SID. Other 4 conns untouched.

### What changes vs current Wave 5 plan

| Aspect | Wave 5 (current) | This redesign |
|---|---|---|
| Depth-20 conn allocation | 4 pinned (NIFTY/BANKNIFTY CE/PE) + 1 dynamic top-50 | 5 dynamic top-50 = 250 SIDs total |
| Depth-200 conn allocation | 5 dynamic top-5 (Wave 5 already this shape) | 5 dynamic top-5 (unchanged shape, but **same selector module** as depth-20) |
| Resubscribe mechanism | `DepthCommand::Swap20`/`Swap200` = full unsubscribe-then-subscribe of the entire set on the conn | NEW `AddSubscriptions20` / `RemoveSubscriptions20` / `Add200` / `Remove200` = pinpoint diff |
| SENSEX coverage | Index F&O via main feed only (Dhan does not support BSE depth) | Unchanged |
| NIFTY/BANKNIFTY guarantee in depth-20 | Hard-pinned by symbol | Soft — almost always present via volume rank, not guaranteed |

### Out of scope (deferred to follow-up)

- Removing the Wave 5 pinned-index Conn 1-4 wiring code; this plan replaces it
- Migration of existing `depth_20_top_gainers_selector.rs` (single-conn) to the new shared selector module — covered in Item 3
- Wave 5 plan archive / retire (the new plan supersedes it for depth-20)

## Plan Items

### PR-A: Saturday Post-Market gate (small, ship first)

- [x] **1. Gate the 15:30 IST Post-Market Telegram on `TradingCalendar::is_trading_day(today_ist)`** (shipped 2026-05-02)
  - Files: `crates/app/src/main.rs` (`run_shutdown_fast` — added `trading_calendar` param; gate before `notifier.notify(NotificationEvent::Custom { ... Post-Market ... })`; passed from both call sites at lines 1740 + 6951)
  - Files: `crates/app/src/boot_helpers.rs` (added pure helper `should_emit_post_market_alert(&TradingCalendar, NaiveDate) -> bool`)
  - Tests: `test_post_market_alert_suppressed_on_saturday` ✅
  - Tests: `test_post_market_alert_suppressed_on_sunday` ✅
  - Tests: `test_post_market_alert_suppressed_on_nse_holiday` ✅
  - Tests: `test_post_market_alert_fires_on_normal_trading_day` ✅

### PR-B: Shared selector module + diff-based DepthCommand variants

- [x] **2. Create shared top-volume×%change selector module reading from `movers_1m`** (shipped commit 6fa66f2)
  - Files: `crates/core/src/instrument/depth_dynamic_top_volume_selector.rs` (NEW)
  - Files: `crates/storage/src/movers_unified_query.rs` (extend with `top_volume_cohort_query(window_secs, cohort_size)` builder)
  - Tests: `test_query_targets_movers_1m_with_segment_d_filter`
  - Tests: `test_selector_excludes_bse_fno_via_registry_lookup`
  - Tests: `test_selector_sorts_cohort_by_change_pct_desc_then_takes_k`
  - Tests: `test_selector_takes_exactly_k_when_cohort_larger_than_k`
  - Tests: `test_selector_returns_partial_when_cohort_smaller_than_k_emits_dyn_03_or_dyn_01`
  - Tests: `test_selector_is_deterministic_given_same_input`

- [x] **3. Add diff-based DepthCommand variants + per-conn dispatcher** (shipped commit 7a9385d)
  - Files: `crates/core/src/websocket/depth_connection.rs` (extend `DepthCommand` enum)
  - Files: `crates/core/src/websocket/subscription_builder.rs` (build single-SID add/remove JSON frames)
  - Tests: `test_add_subscriptions_20_emits_request_code_23_with_single_sid`
  - Tests: `test_remove_subscriptions_20_emits_request_code_25_with_single_sid`
  - Tests: `test_add_subscriptions_200_emits_root_path_request_code_23_flat_json`
  - Tests: `test_remove_subscriptions_200_emits_request_code_25_flat_json`
  - Tests: `test_depth_connection_select_loop_handles_add_remove_commands`

- [x] **4. Implement `DynamicSubscriptionState` with diff algorithm** (shipped commit be5378b + smoke ef8a61b)
  - Files: `crates/core/src/instrument/dynamic_subscription_state.rs` (NEW)
  - Tests: `test_diff_no_op_when_set_unchanged`
  - Tests: `test_diff_single_swap_emits_one_remove_one_add_on_same_conn`
  - Tests: `test_diff_assigns_new_sid_to_conn_with_most_free_slots`
  - Tests: `test_diff_full_replacement_distributes_across_5_conns`
  - Tests: `test_diff_preserves_locality_when_conn_just_freed_slot`
  - Tests: `test_state_capacity_bounded_to_50_per_conn_for_depth_20`
  - Tests: `test_state_capacity_bounded_to_1_per_conn_for_depth_200`

### PR-C: Wire the selector + diff state into depth-20 + depth-200 schedulers

- [x] **5a. PR-C1: New unified pipeline orchestrator (dead code)** (shipped commit f0be306)
  - Files: `crates/app/src/depth_dynamic_pipeline_v2.rs` (NEW, 657 LoC)
  - Files: `crates/app/src/lib.rs` (module registration)
  - Public API: `spawn_depth_dynamic_pool(cfg, cmd_senders, shutdown)` —
    one spawner serves both depth-20 (`PoolShape { 5, 50 }`) and depth-200
    (`PoolShape { 5, 1 }`) by parameter
  - 19 ratchet tests (Dhan protocol JSON builders, cohort parser
    defensive paths, dispatch routing, market-hours gate)
  - **Status:** dead code — compiles and links but is NOT yet wired into
    `main.rs`. PR-C2 cutover is a separate operator-approved change.

- [ ] **5b. PR-C2: main.rs cutover — replace Wave 5 boot flow with pipeline_v2** (PENDING, requires explicit operator approval — touches live boot path)
  - Files: `crates/app/src/main.rs` lines 2735, 2759, 2831, 2876 — replace
    calls to `spawn_depth_20_minimal_conn ×4`, `spawn_depth_20_dynamic_conn5_task`,
    `spawn_depth_200_dynamic_pool_task` with two `spawn_depth_dynamic_pool` calls
  - Files: `crates/app/src/main.rs` boot DDL — call
    `ensure_depth_dynamic_diff_audit_table` after the existing
    `ensure_depth_rebalance_audit_table`
  - Tests: integration test for the cutover (probably covered by existing
    `phase2_readiness_check` + `market_open_self_test` ratchets)

### PR-D: Observability + ratchets

- [x] **7a. Audit table + Prom counters wired into pipeline_v2** (shipped commit 48ca276)
  - Files: `crates/storage/src/depth_dynamic_diff_audit_persistence.rs` (NEW — DEDUP UPSERT KEYS(ts, feed))
  - Files: `crates/storage/src/lib.rs` (module registration)
  - Files: `crates/app/src/depth_dynamic_pipeline_v2.rs` — `emit_diff_metrics` + `persist_diff_audit` helpers
  - Prometheus counters: `tv_depth_dynamic_diff_adds_total{feed}`, `tv_depth_dynamic_diff_removes_total{feed}`, `tv_depth_dynamic_diff_cycles_total{feed}`, gauge `tv_depth_dynamic_set_size{feed}`
  - 10 ratchet tests for the audit module + 19 pipeline_v2 tests still pass

- [x] **7b. Grafana panel + Prometheus alert rule** (shipped commit 8bf80a8)
  - Files: `deploy/docker/grafana/dashboards/operator-health.json` (panels 47 + 48)
  - Files: `deploy/docker/grafana/provisioning/alerting/alerts.yml` (alert `tv-depth-dynamic-set-size-low`, severity warning, fires when `min by (feed) (tv_depth_dynamic_set_size) < 5` for 5+ min)
  - Both JSON + YAML validate cleanly

- [ ] **8. Update Wave 5 error code rules + plan archive marker** (defer until PR-C2 ships)
  - Files: `.claude/rules/project/wave-5-error-codes.md` (DEPTH-20-DYN-03 reworded: now applies when top-250 set has fewer than 250; DEPTH-200-DYN-01 reworded: top-5 has fewer than 5)
  - Files: `.claude/plans/archive/active-plan-wave-5-indices-only.md` (move existing Wave 5 plan after PR-C2 ships)
  - Tests: `error_code_rule_file_crossref` (existing meta-test)

- [ ] **9. Adversarial 3-agent review (mandatory per `wave-4-shared-preamble.md` §3)**
  - Spawn IN PARALLEL on the diff BEFORE opening any PR AND again AFTER green CI:
    - `hot-path-reviewer` agent — flags any `.clone()`, `Vec::new()`, `.collect()`, `format!()`, `Box`, `dyn`, unbounded channel on the 60s diff path. Report ≤ 400 words.
    - `security-reviewer` agent — flags secret exposure in logs/labels, ILP injection, missing `Secret<T>`, unsafe blocks, missing input sanitization. Report ≤ 400 words.
    - `general-purpose` (hostile bug-hunt) — race conditions on `slot_assignment` map, daily-reset edge cases, market-hours gates, edge-trigger correctness, false-OK signals, integration with Wave-2-A/B SubscribeRxGuard + pool supervisor. Report ≤ 600 words.
  - Synthesize into CRITICAL / HIGH / MEDIUM / LOW / FALSE-POSITIVE table; fix every CRITICAL+HIGH inline before merge.
  - Tests: N/A — this is a process gate, not a code gate. Verified by PR description containing the synthesized verdict table.

### Per-Item Guarantee Matrix (mandatory, applies to every item 1-9)

Every item carries the 15-row + 7-row matrix from
`.claude/rules/project/per-wave-guarantee-matrix.md`. Mechanical
enforcement: `bash .claude/hooks/per-item-guarantee-check.sh`
(exit 2 = block) and `make wave-guarantee-check`.

## Verification

```bash
# Per-PR scoped checks
cargo check -p tickvault-app -p tickvault-core
cargo test  -p tickvault-core --lib instrument::depth_dynamic_top_volume_selector
cargo test  -p tickvault-core --lib instrument::dynamic_subscription_state
cargo test  -p tickvault-core --lib websocket::depth_connection
cargo test  -p tickvault-storage --lib depth_dynamic_diff_audit_persistence
cargo test  -p tickvault-app --lib

# Pre-PR full
FULL_QA=1 make scoped-check
bash .claude/hooks/banned-pattern-scanner.sh
bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all
bash .claude/hooks/pub-fn-wiring-guard.sh "$PWD"
bash .claude/hooks/plan-verify.sh
bash .claude/hooks/per-item-guarantee-check.sh
python3 -c "import yaml; yaml.safe_load(open('deploy/docker/grafana/provisioning/alerting/alerts.yml'))"
python3 -c "import json; json.load(open('deploy/docker/grafana/dashboards/operator-health.json'))"
```

## Scenarios

| # | Scenario | Expected behaviour |
|---|----------|-------------------|
| 1 | Saturday 2026-05-02 15:30 IST | NO Post-Market Telegram fires (PR-A gate) |
| 2 | Monday 2026-05-04 15:30 IST | Post-Market Telegram fires normally |
| 3 | Monday 09:15:00 IST boot | Depth-20 subscribes top-250 across 5 conns (50/conn); depth-200 subscribes top-5 (1/conn); audit + Telegram-Info diff event fires once with the full initial set |
| 4 | 60s tick, rank-250 drops to rank-251, rank-251 enters top-250 | 1 RemoveSubscriptions20 + 1 AddSubscriptions20 frame on the conn holding the dropped SID; other 4 conns send zero frames; audit row written; counters increment by 1 each |
| 5 | 60s tick, ranks unchanged | Zero wire traffic; zero audit row; gauge `tv_depth_dynamic_set_size` unchanged |
| 6 | Bear day, top-volume universe has only 200 NSE_FNO contracts with positive change_pct | DEPTH-20-DYN-03 fires Severity::High edge-trigger; conn 1-5 keep last-good 250 set until next tick |
| 7 | Conn 3 dies mid-session (TCP RST), pool supervisor respawns | New conn 3 reuses the same 50 SIDs from `slot_assignment[*]` where conn_idx == 3; SubscribeRxGuard preserves command channel |
| 8 | depth-200 rank-5 drops, rank-6 enters | 1 conn out of 5 sends Unsubscribe25 + Subscribe23 (root path JSON); other 4 untouched; rebalancer audit row written |
| 9 | Outside market hours (16:00 IST) | 60s tick suppressed via `is_within_market_hours_ist()`; no Telegram, no audit churn |
| 10 | SENSEX (BSE_FNO derivative) appears in the top-500 `movers_1m` cohort by volume | Stage 2 registry lookup tags it as `BseFno`, filtered out; never appears in `next_set` |

## Resolved Decisions (operator approved 2026-05-02)

1. **Slot re-assignment = least-full strategy** (argmin over `conn.size`, tie-break by ascending `conn_idx`). At steady-state 250/250 capacity this reduces to locality (the conn that just freed becomes the unique least-full). On volatile days when many SIDs swap simultaneously, the rule provably balances incoming SIDs across conns from cycle 1. Pure round-robin (incrementing `next_conn % 5`) was rejected because it ignores capacity and forces 3-frame inter-conn moves when `next_conn` differs from the conn that freed.
2. **Diff Telegram cadence:** Severity::Info per-diff via the existing 60s coalescer (Wave 3 Item 11 TELEGRAM-01 mechanism). Loses no forensic detail; summary-only-every-5-min rejected because it loses per-cycle granularity when something genuinely interesting happens.
3. **Wave 5 plan archive:** retire `active-plan-wave-5-indices-only.md` to `.claude/plans/archive/` immediately after PR-C ships and the Wave 5 pinned-index allocator is removed. Wave 5 ratchet tests + audit table + ErrorCode variants are NOT retired (they continue to apply to the new design with reworded triggers).

## Per-Wave Guarantee Matrix (cross-reference)

See `.claude/rules/project/per-wave-guarantee-matrix.md` — all 15 rows of the
100% Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply to
every item in this plan. Mechanical enforcement via
`.claude/hooks/per-item-guarantee-check.sh` (CI gate).

## Honest 100% Claim (mandatory wording per per-wave-guarantee-matrix.md §8)

100% inside the tested envelope, with ratcheted regression coverage:
≤60s QuestDB outage absorbed by rescue→spill→DLQ; ≤600K rescue ring
capacity; bench-gated O(1) diff hot path (HashSet ops + per-conn slot map);
composite-key uniqueness on `depth_dynamic_diff_audit` DEDUP UPSERT
KEYS(ts, feed, conn_idx); chaos-tested 70h sleep/wake. Beyond the
envelope, DLQ NDJSON catches every payload as recoverable text.
