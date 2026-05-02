# Implementation Plan: Depth-20 + Depth-200 All-Dynamic Top-Volume Redesign + Saturday Post-Market Gate

**Status:** DRAFT
**Date:** 2026-05-02
**Approved by:** pending (Parthiban verbally approved direction; this doc captures the contract)
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

- [ ] **1. Gate the 15:30 IST Post-Market Telegram on `TradingCalendar::is_trading_day(today_ist)`**
  - Files: `crates/app/src/main.rs` (the 15:30 `WebSocketDisconnected` / Post-Market emission site — exact line TBD via grep for `Post-Market` and `15:30`)
  - Files: possibly `crates/core/src/notification/events.rs` if a new variant is needed for "post-market on non-trading-day = no-op"
  - Tests: `test_post_market_alert_suppressed_on_saturday`
  - Tests: `test_post_market_alert_suppressed_on_holiday`
  - Tests: `test_post_market_alert_fires_on_trading_day_at_1530`

### PR-B: Shared selector module + diff-based DepthCommand variants

- [ ] **2. Create shared top-volume×%change selector module reading from `movers_1m`**
  - Files: `crates/core/src/instrument/depth_dynamic_top_volume_selector.rs` (NEW)
  - Files: `crates/storage/src/movers_unified_query.rs` (extend with `top_volume_cohort_query(window_secs, cohort_size)` builder)
  - Tests: `test_query_targets_movers_1m_with_segment_d_filter`
  - Tests: `test_selector_excludes_bse_fno_via_registry_lookup`
  - Tests: `test_selector_sorts_cohort_by_change_pct_desc_then_takes_k`
  - Tests: `test_selector_takes_exactly_k_when_cohort_larger_than_k`
  - Tests: `test_selector_returns_partial_when_cohort_smaller_than_k_emits_dyn_03_or_dyn_01`
  - Tests: `test_selector_is_deterministic_given_same_input`

- [ ] **3. Add diff-based DepthCommand variants + per-conn dispatcher**
  - Files: `crates/core/src/websocket/depth_connection.rs` (extend `DepthCommand` enum)
  - Files: `crates/core/src/websocket/subscription_builder.rs` (build single-SID add/remove JSON frames)
  - Tests: `test_add_subscriptions_20_emits_request_code_23_with_single_sid`
  - Tests: `test_remove_subscriptions_20_emits_request_code_25_with_single_sid`
  - Tests: `test_add_subscriptions_200_emits_root_path_request_code_23_flat_json`
  - Tests: `test_remove_subscriptions_200_emits_request_code_25_flat_json`
  - Tests: `test_depth_connection_select_loop_handles_add_remove_commands`

- [ ] **4. Implement `DynamicSubscriptionState` with diff algorithm**
  - Files: `crates/core/src/instrument/dynamic_subscription_state.rs` (NEW)
  - Tests: `test_diff_no_op_when_set_unchanged`
  - Tests: `test_diff_single_swap_emits_one_remove_one_add_on_same_conn`
  - Tests: `test_diff_assigns_new_sid_to_conn_with_most_free_slots`
  - Tests: `test_diff_full_replacement_distributes_across_5_conns`
  - Tests: `test_diff_preserves_locality_when_conn_just_freed_slot`
  - Tests: `test_state_capacity_bounded_to_50_per_conn_for_depth_20`
  - Tests: `test_state_capacity_bounded_to_1_per_conn_for_depth_200`

### PR-C: Wire the selector + diff state into depth-20 + depth-200 schedulers

- [ ] **5. Replace depth-20 pinned-index allocator with all-dynamic top-250 scheduler**
  - Files: `crates/core/src/instrument/depth_20_top_gainers_selector.rs` (refactor to use shared selector with k=250)
  - Files: `crates/app/src/main.rs` (boot wiring: remove the 4 pinned NIFTY/BANKNIFTY CE/PE conn allocations, replace with 5 dynamic conn slots)
  - Tests: `test_depth_20_initial_allocation_picks_top_250_at_boot`
  - Tests: `test_depth_20_60s_tick_emits_diff_only_for_rank_changes`
  - Tests: `test_depth_20_no_pinning_of_nifty_banknifty_strikes_by_symbol`

- [ ] **6. Wire depth-200 to shared selector with k=5 and diff state**
  - Files: `crates/core/src/instrument/depth_200_top_gainers_selector.rs` (refactor to use shared selector with k=5)
  - Files: `crates/app/src/main.rs` (depth-200 boot wiring uses diff state)
  - Tests: `test_depth_200_initial_allocation_picks_top_5_at_boot`
  - Tests: `test_depth_200_60s_tick_swaps_only_the_changed_contract`
  - Tests: `test_depth_200_excludes_sensex_via_segment_filter`

### PR-D: Observability + ratchets + adversarial review

- [ ] **7. Per-conn diff metrics + Telegram + audit table**
  - Files: `crates/storage/src/depth_dynamic_diff_audit_persistence.rs` (NEW — DEDUP UPSERT KEYS(ts, feed, conn_idx))
  - Files: `crates/core/src/notification/events.rs` (variant `DepthDynamicDiffApplied { feed, conn_idx, removed: Vec<SecurityId>, added: Vec<SecurityId> }`, Severity::Info, edge-trigger only on non-empty diff)
  - Files: `crates/common/src/error_code.rs` (no new variants needed; reuse existing DEPTH-20-DYN-03 / DEPTH-200-DYN-01 for empty-set cases)
  - Files: Prometheus counters: `tv_depth_dynamic_diff_adds_total{feed}`, `tv_depth_dynamic_diff_removes_total{feed}`, gauge `tv_depth_dynamic_set_size{feed}`
  - Files: `deploy/docker/grafana/dashboards/operator-health.json` (depth-dynamic diff panels)
  - Files: `deploy/docker/prometheus/alerts.yml` (alert if set_size < expected for > 5min during market hours)
  - Tests: `test_audit_row_written_on_each_diff_with_dedup`
  - Tests: `test_prom_counters_increment_per_diff`
  - Tests: `test_grafana_panel_pinned_by_dashboard_guard`
  - Tests: `test_alert_rule_pinned_by_resilience_sla_guard`

- [ ] **8. Update Wave 5 error code rules + plan archive marker**
  - Files: `.claude/rules/project/wave-5-error-codes.md` (DEPTH-20-DYN-03 reworded: now applies when top-250 set has fewer than 250; DEPTH-200-DYN-01 reworded: top-5 has fewer than 5)
  - Files: `.claude/plans/archive/active-plan-wave-5-indices-only.md` (move existing Wave 5 plan after PR-C ships)
  - Tests: `error_code_rule_file_crossref` (existing meta-test; will pass once rule files updated)

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

## Open Questions for Operator (before approval)

1. **Locality preference on slot assignment:** when a conn frees a slot AND a new SID needs assignment, prefer that same conn (reduces churn) vs round-robin (better balance over time). Current draft: prefer locality. OK?
2. **Diff Telegram cadence:** every diff = 1 Info event = up to 1/min during volatile sessions. Coalescer already buckets by topic so the operator sees a 60s summary, not per-tick spam. OK?
3. **Wave 5 retirement:** PR-C archives `active-plan-wave-5-indices-only.md` since this redesign supersedes its depth-20 conn allocation. The Wave 5 audit table + selector files keep their names but their semantics change. OK to retire the plan doc?

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
