# Implementation Plan: Unified 09:12 Close Subscription for Main Feed + Depth-20 + Depth-200

**Status:** DRAFT
**Date:** 2026-04-22
**Approved by:** pending (Parthiban)
**Branch:** `claude/market-feed-depth-explanation-RynUx`

## Background

Today the three subscription paths are split:

- **Main feed stock F&O** subscribes at 09:12:30 IST using the 09:12 close (Phase 2 scheduler — correct).
- **Depth-20** picks initial ATM at boot using whatever index LTP is available (pre-open ~09:00 or stale), then 60s rebalancer corrects it.
- **Depth-200** same as Depth-20 (boot-time guess + rebalancer correction).

Parthiban directive (2026-04-22): **all three must key off the same 09:12 close.** Depth-20 and Depth-200 should not subscribe until 09:12:30 IST, using the 09:12 NIFTY/BANKNIFTY close. Dynamic 60s rebalancer continues only from 09:15 IST onwards.

Tradeoff accepted: pre-open depth blackout 09:00–09:12:30 (12 min).

## Plan Items

- [ ] A. Extend `preopen_price_buffer` to also capture index closes (NIFTY, BANKNIFTY)
  - Files: `crates/core/src/instrument/preopen_price_buffer.rs`
  - Current lookup table is F&O-stocks-only (line 226 comment). Add parallel index path so ticks for IDX_I security_ids (13, 25) also record into the buffer under the symbol key ("NIFTY", "BANKNIFTY").
  - Tests: `test_preopen_buffer_records_nifty_idx_i_close`, `test_preopen_buffer_records_banknifty_idx_i_close`

- [ ] B. Defer depth-20 + depth-200 subscribe until 09:12:30 IST
  - Files: `crates/app/src/main.rs` (Step 8c block ~line 2044-2925)
  - Remove boot-time "wait 30s for LTP → select_depth_instruments → spawn depth" flow.
  - Instead: spawn depth WS tasks in a holding state (connected + authenticated but NOT subscribed). They sit idle waiting for a "depth-subscribe" command.
  - Tests: `test_depth_tasks_idle_before_0912_ist` (integration)

- [ ] C. Phase 2 scheduler extension — dispatch depth subscriptions at 09:12:30
  - Files: `crates/core/src/instrument/phase2_scheduler.rs`
  - After the existing stock-F&O dispatch, call a new `dispatch_depth_subscriptions()` that:
    1. Reads 09:12 close from preopen buffer for NIFTY + BANKNIFTY
    2. Picks ATM ± 24 strikes per index (depth-20) + ATM CE + ATM PE (depth-200)
    3. Sends `DepthCommand::InitialSubscribe` to each depth connection's mpsc channel
  - Tests: `test_phase2_dispatches_depth_at_0912`, `test_phase2_depth_uses_0912_close_not_earlier`

- [ ] D. New `DepthCommand::InitialSubscribe` variant + handling
  - Files: `crates/core/src/websocket/depth_connection.rs`
  - Add variant alongside existing `Swap20` / `Swap200`. First subscribe on the socket. After this fires, socket accepts subsequent `Swap*` commands.
  - Tests: `test_depth_initial_subscribe_sends_request_code_23`, `test_depth_initial_subscribe_only_fires_once`

- [ ] E. Depth rebalancer 09:15 IST gate for first check
  - Files: `crates/core/src/instrument/depth_rebalancer.rs`
  - Current `is_within_market_hours_ist` gate uses 09:00 start (tick_persist window). Add second gate: first rebalance check only fires at ≥ 09:15 IST so we don't rebalance immediately on top of the 09:12:30 initial subscribe (3-minute settle).
  - Tests: `test_rebalancer_first_check_skipped_before_0915_ist`, `test_rebalancer_runs_normally_after_0915_ist`

- [ ] F. Crash-recovery path — 09:12:30 snapshot extended to depth
  - Files: `crates/core/src/instrument/phase2_scheduler.rs` + the on-disk snapshot writer (PROMPT C path)
  - Snapshot now contains: {stock F&O subscriptions, depth-20 ATM contracts, depth-200 ATM contracts}
  - On crash-restart after 09:12:30 → replay all three from snapshot, not just stock F&O.
  - Tests: `test_snapshot_roundtrip_includes_depth_selections`, `test_crash_recovery_dispatches_depth_from_snapshot`

- [ ] G. Telegram notification for unified Phase 2 complete
  - Files: `crates/core/src/notification/events.rs`
  - Extend `Phase2Complete` event to include depth counts: `stock_fno_added`, `depth_20_underlyings`, `depth_200_underlyings`.
  - Message format: "Phase 2 complete at 09:12:30 — stock F&O: +6,123 | depth-20: NIFTY/BANKNIFTY/FINNIFTY/MIDCPNIFTY | depth-200: NIFTY CE/PE, BANKNIFTY CE/PE"
  - Tests: `test_phase2_complete_telegram_includes_depth_counts`

- [ ] H. Rule file update — record the new canonical behavior
  - Files: `.claude/rules/project/depth-subscription.md`, `.claude/rules/project/live-market-feed-subscription.md`
  - Replace "boot waits up to 30s for LTPs" with "09:12:30 IST scheduler dispatches all three subscriptions from the 09:12 close".
  - No test (docs) — but the integration tests in C/E cover the behavior.

- [ ] I. Ratchet guard test
  - Files: `crates/core/tests/phase2_unified_depth_guard.rs` (new)
  - Source-scans `main.rs` to verify depth connections no longer subscribe at boot (no `select_depth_instruments` call in boot path). Source-scans `phase2_scheduler.rs` for the new `dispatch_depth_subscriptions` call.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot at 08:00, market day | Main feed subscribes Phase 1 (indices + index F&O + stock equities). Depth WS tasks spawn idle. No depth subscribe until 09:12:30. |
| 2 | 09:00–09:12 IST | Pre-open ticks flow for indices + stocks. Buffer captures NIFTY, BANKNIFTY, and F&O stock closes per minute. No depth data flowing. |
| 3 | 09:12:30 IST | Phase 2 fires. Stock F&O subscribe burst + Depth-20 subscribe (4 underlyings × ~49 contracts) + Depth-200 subscribe (4 contracts). Single Telegram notification. |
| 4 | 09:15 IST onwards | Depth rebalancer starts 60s loop. Main feed stock F&O list frozen for the day. |
| 5 | Crash at 11:00, restart at 11:01 | Snapshot read from disk → stock F&O + depth selections replayed in one dispatch. Depth sockets reconnect and subscribe from snapshot. Rebalancer runs immediately (already past 09:15). |
| 6 | Boot at 10:30 (late start) | Phase 2 `RunImmediate` path fires with `minutes_late: 78`. Same unified dispatch. Rebalancer active immediately. |
| 7 | NIFTY didn't tick 09:12 (extremely rare) | Fallback to 09:11 → 09:10 → 09:09 → 09:08 slot. If all empty → Phase 2 aborts with CRITICAL Telegram, depth stays idle for the day. |
| 8 | Weekend boot | Phase 2 scheduler returns `SkipToday { reason: "weekend" }`. Depth never subscribes. Correct. |

- [ ] J. Scenario-aware fallback state machine in Phase 2 scheduler
  - Files: `crates/core/src/instrument/phase2_scheduler.rs`
  - Explicit enum `Phase2DispatchSource { Normal0912Close, SnapshotRecovery, LiveLtpFallback, Aborted }`. Every dispatch path logs its source + reasoning.
  - Tests: `test_dispatch_source_normal_path`, `test_dispatch_source_snapshot_path`, `test_dispatch_source_live_ltp_path`, `test_dispatch_source_aborted_path`

- [ ] K. Full observability surface for every Phase 2 transition
  - Files: `crates/core/src/instrument/phase2_scheduler.rs`, `crates/core/src/notification/events.rs`, `crates/app/src/observability.rs`, `deploy/docker/grafana/dashboards/operator-health.json`
  - Emit on EVERY path (normal / recovery / late-start / abort / skip):
    - **Prometheus counter**: `tv_phase2_dispatches_total{source="...", result="ok|aborted"}`
    - **Prometheus gauge**: `tv_phase2_last_dispatch_ist_secs`, `tv_phase2_stock_fno_subscribed`, `tv_phase2_depth_20_subscribed`, `tv_phase2_depth_200_subscribed`
    - **Prometheus histogram**: `tv_phase2_dispatch_latency_seconds` (how long from 09:12:30 wake-up to all three subscribed)
    - **Telegram**: edge-triggered, one message per dispatch with source + counts + fallback reason if any
    - **Structured ERROR log** (routed to errors.jsonl) for: abort path, stale snapshot rejection, LTP timeout after 90s, buffer all-empty
    - **Structured INFO log** for: normal dispatch, snapshot recovery, late-start fallback
    - **Grafana panel**: "Phase 2 Daily Status" — shows today's dispatch source, counts, latency
  - Tests: `test_phase2_metrics_emitted_on_normal_path`, `test_phase2_metrics_emitted_on_recovery_path`, `test_phase2_metrics_emitted_on_live_ltp_path`, `test_phase2_metrics_emitted_on_abort_path`, `test_phase2_telegram_edge_triggered`, `test_errors_jsonl_contains_abort_path`

- [ ] L. Subscription audit trail (persistent record of what was subscribed, when, why)
  - Files: `crates/storage/src/subscription_audit.rs` (new), `crates/core/src/instrument/phase2_scheduler.rs`
  - New QuestDB table `subscription_audit_log` with columns: `ts`, `event_type SYMBOL`, `source SYMBOL`, `feed SYMBOL` (main|depth_20|depth_200), `underlying SYMBOL`, `instrument_count INT`, `atm_strike DOUBLE`, `spot_price_used DOUBLE`, `spot_price_source SYMBOL` (0912_close|live_ltp|snapshot), `notes STRING`
  - Every Phase 2 dispatch writes 1+ rows. Every 60s depth rebalance writes 1 row per swap. Every boot Phase 1 writes 1 row.
  - Tests: `test_audit_log_ddl_includes_all_columns`, `test_audit_log_row_written_on_phase2_dispatch`, `test_audit_log_row_written_on_depth_rebalance`

- [ ] M. Grafana dashboard panels + alert rules
  - Files: `deploy/docker/grafana/dashboards/market-data.json`, `deploy/docker/prometheus/alerts/phase2.yml` (new)
  - Panels: Phase 2 dispatch source breakdown (pie chart), dispatch latency histogram, subscribed-instrument-count over time, depth rebalance count per hour
  - Prometheus alerts: `Phase2NotDispatchedBy0915` (fires if `tv_phase2_last_dispatch_ist_secs` is 0 after 09:15 on a trading day), `Phase2AbortedToday` (fires once per abort), `DepthRebalancerStalled` (no rebalance check in >120s during market hours)
  - Tests: `test_grafana_dashboard_includes_phase2_panels`, `test_prometheus_alerts_file_parses`

- [ ] N. Debug context — every decision carries a trace span
  - Files: `crates/core/src/instrument/phase2_scheduler.rs`, `crates/core/src/instrument/depth_rebalancer.rs`
  - Use `#[tracing::instrument(...)]` on every pub fn in the scheduler + rebalancer. Attach fields: `source`, `minutes_late`, `nifty_spot`, `banknifty_spot`, `atm_strike`, `instrument_count`. OpenTelemetry spans route to Jaeger for post-mortem.
  - Tests: `test_phase2_span_carries_source_field`, `test_rebalancer_span_carries_drift_strikes_field`

## Scenario Matrix (explicit fallback chain)

| Scenario | Source | Trigger | Data source | Observability |
|---|---|---|---|---|
| **A1 Normal** | `Normal0912Close` | 09:12:30 IST on a trading day | 09:12 close from preopen buffer | INFO log + INFO Telegram + metrics + audit row |
| **A2 Buffer partial** | `Normal0912Close` | 09:12 slot empty → walk back | 09:11/10/09/08 slot fallback | WARN log + INFO Telegram ("using 09:11 close for NIFTY") + metrics + audit row |
| **B1 Clean mid-day crash** | `SnapshotRecovery` | restart, today's snapshot on disk | snapshot file (same ATM as pre-crash) | INFO log + INFO Telegram ("Phase 2 recovered from snapshot, age: 2h16m") + metrics + audit row |
| **B2 Snapshot corrupt** | falls through to `LiveLtpFallback` | snapshot deserialize fails | current `SharedSpotPrices` | ERROR log (jsonl) + ERROR Telegram + metrics + audit row |
| **C1 Fresh clone at 10:00** | `LiveLtpFallback` | past 09:12, no snapshot, no buffer | current `SharedSpotPrices` | WARN log + WARN Telegram ("LATE START at 10:00, 09:12 close unavailable, using live LTP") + metrics + audit row |
| **C2 Fresh clone + LTP slow** | `LiveLtpFallback` → `Aborted` | waits 90s, no LTP arrives | — | ERROR log (jsonl) + CRITICAL Telegram + metrics + audit row |
| **D1 All buffer slots empty (outage 09:00-09:12)** | `Aborted` | 5/5 slots empty AND no snapshot AND no live LTP within 90s | — | ERROR log (jsonl) + CRITICAL Telegram + metrics + audit row + depth stays idle for the day |
| **E1 Weekend/holiday** | `SkipToday` | TradingCalendar says non-trading day | — | INFO log, no Telegram (expected), no audit row |
| **E2 Post-market (15:30+)** | `SkipToday` | past 15:30 IST | — | INFO log, no Telegram (expected), no audit row |
| **F1 Depth-20 rebalance (60s tick)** | — | spot drift ≥ 3 strikes | `SharedSpotPrices` | INFO log + INFO Telegram (old → new label) + counter inc + audit row |
| **F2 Depth-200 rebalance** | — | same | same | INFO log + INFO Telegram + counter inc + audit row |
| **F3 Depth rebalance fails (socket dead)** | — | `DepthCommand::Swap*` send error | — | ERROR log (jsonl) + WARN Telegram + counter inc |
| **F4 Spot price stale > 180s during market hours** | — | edge-triggered | — | ERROR log (jsonl) + ERROR Telegram (rising edge only) + counter inc |

## Observability Matrix (metric/log/alert coverage)

| Signal | Normal (A1) | Partial buffer (A2) | Recovery (B1) | Corrupt snapshot (B2) | Late start (C1) | LTP timeout (C2) | Buffer empty (D1) | Skip (E) | Rebalance (F1/F2) | Rebalance fail (F3) | Stale spot (F4) |
|---|:-:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|
| Telegram | INFO | INFO | INFO | ERROR | WARN | CRITICAL | CRITICAL | — | INFO | WARN | ERROR |
| errors.jsonl | — | — | — | ✅ | — | ✅ | ✅ | — | — | ✅ | ✅ |
| tracing INFO | ✅ | — | ✅ | — | — | — | — | ✅ | ✅ | — | — |
| tracing WARN | — | ✅ | — | — | ✅ | — | — | — | — | — | — |
| tracing ERROR | — | — | — | ✅ | — | ✅ | ✅ | — | — | ✅ | ✅ |
| Prometheus counter | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Prometheus gauge | ✅ | ✅ | ✅ | — | ✅ | — | — | — | — | — | — |
| Grafana panel | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Prometheus alert rule | — | — | — | ✅ (aborted) | — | ✅ (aborted) | ✅ (aborted) | — | — | — | ✅ (stale) |
| Jaeger span | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| subscription_audit_log row | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | — | — |

Every critical transition is captured in at least 5 independent channels: Telegram (operator), tracing log (local journalctl), errors.jsonl (Claude auto-triage), Prometheus (metrics/alerts), audit log (SEBI / post-mortem), Jaeger span (deep debug). Zero single-point-of-failure for observability.

## Out of Scope

- Changing the main feed Phase 1 subscribe (indices + stock equities) — unchanged.
- Changing the 60s depth rebalancer cadence or drift threshold — unchanged.
- Changing the order update WebSocket — unchanged.
- Removing the SharedSpotPrices map — still needed by the 60s rebalancer post-09:15.

## Files Touched (21 files)

**Core logic (6):**
1. `crates/core/src/instrument/preopen_price_buffer.rs`
2. `crates/core/src/instrument/phase2_scheduler.rs`
3. `crates/core/src/websocket/depth_connection.rs`
4. `crates/core/src/instrument/depth_rebalancer.rs`
5. `crates/core/src/notification/events.rs`
6. `crates/app/src/main.rs`

**New files (5):**
7. `crates/core/tests/phase2_unified_depth_guard.rs` (ratchet)
8. `crates/core/benches/phase2_dispatch.rs` (budget)
9. `crates/storage/src/subscription_audit.rs` (audit log writer)
10. `deploy/docker/prometheus/alerts/phase2.yml` (3 alert rules)
11. `docs/runbooks/phase2-aborted.md`, `phase2-late-start.md`, `depth-rebalance-failed.md`

**Observability (3):**
12. `crates/app/src/observability.rs` (new metrics registered)
13. `deploy/docker/grafana/dashboards/operator-health.json` (Phase 2 panel)
14. `deploy/docker/grafana/dashboards/market-data.json` (depth audit panel)

**Rules + docs (3):**
15. `.claude/rules/project/depth-subscription.md`
16. `.claude/rules/project/live-market-feed-subscription.md`
17. `.claude/rules/project/observability-architecture.md` (append Phase 2 section)

**Automation (2):**
18. `scripts/validate-automation.sh` (5 new checks)
19. `Makefile` (add `make phase2-status`)

**Schema (2):**
20. `crates/storage/src/instrument_persistence.rs` (new DDL for `subscription_audit_log`)
21. `scripts/migrate-subscription-audit-log.sql` (one-time migration)

## 100% Coverage Commitments (extend existing CI ratchets — no new invention)

Every new file / symbol added by items A–N MUST cross the existing gates before merge. Zero new exemptions allowed.

| Gate | Source of truth | Enforcement |
|---|---|---|
| 100% line coverage per crate | `quality/crate-coverage-thresholds.toml` | `scripts/coverage-gate.sh` blocks PR if any touched crate drops |
| 100% branch coverage on new pub fn | `cargo-llvm-cov --branch` | CI job `coverage` (post-merge + PR blocker when touched) |
| 100% mutation survival = 0 | `.github/workflows/mutation.yml:103-113` | `cargo-mutants` — any `SURVIVED` in results fails PR |
| O(1) zero-allocation hot path | DHAT tests under `crates/*/tests/dhat_*.rs` | Hard fail in CI for `core` + `trading` crates |
| Benchmark budgets | `quality/benchmark-budgets.toml` | `scripts/bench-gate.sh` — 5% regression gate |
| Pub fn ↔ test mapping | `.claude/hooks/pub-fn-test-guard.sh` | Every new pub fn has matching `#[test]` or `// TEST-EXEMPT:` annotation |
| Boot symmetry (G3+G4) | `.claude/hooks/boot-symmetry-guard.sh` | New state machines MUST have a poller + both boot paths wired |
| Fuzz targets for parsers | `fuzz/` | Any new byte-parser needs a fuzz target (5min/week per target) |
| ErrorCode rule-file crossref | `crates/common/tests/error_code_rule_file_crossref.rs` | Every new ErrorCode variant has a rule file mention |
| ErrorCode tag-field guard | `crates/common/tests/error_code_tag_guard.rs` | Every `error!(...)` mentioning a code carries `code = ErrorCode::X.code_str()` |
| DEDUP segment meta-guard | `crates/storage/tests/dedup_segment_meta_guard.rs` | Any new `DEDUP_KEY_*` mentioning `security_id` MUST also mention segment |
| security_id uniqueness (I-P1-11) | banned-pattern scanner category 5 | `HashSet<u32>` / `HashSet<SecurityId>` on instrument paths blocked |
| Live-feed purity | banned-pattern scanner category 6 | No backfill/synth symbols in historical paths |

- [ ] O. Items A–N pass every gate above at PR merge time
  - No per-plan-item test file is exempt. If coverage drops, ratchet in `quality/crate-coverage-thresholds.toml` enforces it. If mutation survives, CI blocks.

## Zero-Tick-Loss SLA (extend existing Phase 10.1 + Phase 11 ratchets)

No new invention here — the existing guard tests (4 Prometheus alerts, 7 source invariants, 6 resilience alerts, 6 metric emissions) already pin this. Items A–N MUST preserve all of them.

| Invariant | Where enforced | What it guarantees |
|---|---|---|
| Every WS frame spilled to WAL before ack | `crates/storage/src/ws_frame_spill.rs` | Frame survives process crash + disk flush before QuestDB write attempted |
| WAL replay on restart | `crates/app/src/main.rs:219-330` | Spill files drained into QuestDB (ticks + depth + order update) before new frames accepted |
| SPSC 65K buffer, never blocks WS read loop | `crates/core/src/pipeline/tick_processor.rs` | Hot path: WS read → SPSC send in `<100ns` (benchmark pinned) |
| QuestDB ILP writer retries on error | `crates/storage/src/tick_persistence.rs` | Up to N retries; on persistent failure → WAL spill + ERROR Telegram |
| Valkey cache survival | Phase 11 resilience SLA alert guard | Kill-test ratchet (6 alerts + 6 metrics) |
| QuestDB resilience | Phase 11 resilience SLA alert guard | Kill-test ratchet |
| WS reconnect during market hours infinite | `.claude/rules/project/live-market-feed-subscription.md` | Main feed retries forever 09:00–15:30; depth caps at 60 (CRITICAL after) |
| Tick gap detection | `crates/trading/src/risk/tick_gap.rs` | Warmup + warn + error thresholds; per-security isolation |

- [ ] P. Plan items A–N preserve every zero-tick-loss pin (verified by re-running `cargo test --workspace` pre-push)
  - Specifically: `test_zero_tick_loss_prometheus_alerts_pinned`, `resilience_sla_alert_guard.rs` (6 tests), `live_feed_purity_guard.rs` (6 tests)
  - If any pin fails, the PR is blocked — no override.

## O(1) Latency Guarantees (extend existing benchmark budgets)

| Hot-path operation | Budget | Source | Enforcement |
|---|---|---|---|
| tick_binary_parse | 10 ns | `quality/benchmark-budgets.toml` | 5% regression gate |
| tick_pipeline_routing | 100 ns | same | same |
| papaya_lookup | 50 ns | same | same |
| full_tick_processing | 10 µs | same | same |
| oms_state_transition | 100 ns | same | same |
| market_hour_validation | 50 ns | same | same |
| config_toml_load | 10 ms | same | same |

- [ ] Q. Phase 2 dispatch path + depth subscribe path benchmark added
  - Files: `crates/core/benches/phase2_dispatch.rs` (new)
  - Budget: `phase2_compute_subscriptions < 1 ms` for 200 stock F&O + 4 depth underlyings
  - Rationale: runs once per day, not hot path — but prove it's O(universe_size) not worse

## Automation Requirements (aligns with existing MCP + hooks, no new invention)

Every new Claude Code session / co-work task MUST have automated access to:

| Capability | Today | After this plan |
|---|---|---|
| Log tail (app + errors.jsonl) | `mcp__tickvault-logs__app_log_tail`, `tail_errors` | Unchanged — already automated |
| QuestDB queries | `mcp__tickvault-logs__questdb_sql` | Add query templates for `subscription_audit_log` |
| Prometheus queries | `mcp__tickvault-logs__prometheus_query` | Add `tv_phase2_*` to approved query list |
| Grafana snapshots | `mcp__tickvault-logs__grafana_query` | New "Phase 2 Daily Status" dashboard queryable |
| Runbook lookup | `mcp__tickvault-logs__find_runbook_for_code` | Add runbooks: `phase2-aborted.md`, `phase2-late-start.md`, `depth-rebalance-failed.md` |
| Health doctor | `make doctor` | Add Phase 2 section (dispatched today? source? count?) |
| Validation | `make validate-automation` | Add 5 new checks: Phase 2 dispatch recorded, audit log has today's row, depth rebalancer alive, 09:15 gate wired, snapshot schema valid |

- [ ] R. Runbooks + MCP query catalog extended
  - Files: `docs/runbooks/phase2-aborted.md` (new), `docs/runbooks/phase2-late-start.md` (new), `docs/runbooks/depth-rebalance-failed.md` (new)
  - Files: `scripts/validate-automation.sh` (extend with 5 new checks)

## Honest Guarantee Statement (no hallucination)

What the plan CAN guarantee (enforced by existing + new ratchets):
1. Every Phase 2 dispatch path has at least 5 independent observability channels.
2. Zero ticks lost attributable to tickvault code (WAL spill survives crashes; replayed on restart).
3. O(1) hot-path operations within pinned nanosecond budgets.
4. 100% line + branch coverage on every new file.
5. Zero surviving mutations on every new pub fn.
6. Every error code surfaces to Telegram + errors.jsonl + Prometheus within 30 seconds.
7. Every subscription decision persisted to `subscription_audit_log` for SEBI audit.

What the plan CANNOT guarantee (physical/external limits):
1. Network partitions between our box and Dhan — mitigated by reconnect + WAL spill, not eliminated.
2. Dhan-side rate limits or server errors — mitigated by exponential backoff, not eliminated.
3. QuestDB hardware failure — mitigated by WAL spill + retry, not eliminated.
4. Human error during config changes — mitigated by CI gates + dry-run, not eliminated.

Proof of each guarantee = the test / hook / metric referenced in the ratchet tables above. If any pin fails post-merge, CI blocks and operator is paged.

## Risks

- **Pre-open depth blackout (accepted):** no depth data 09:00–09:12:30. Strategies that watch pre-open book lose this signal.
- **09:12 single-slot dependency:** if NIFTY/BANKNIFTY had no tick in the 09:12 minute bucket, fallback walks backward through 09:11..09:08. If all 5 slots are empty (extreme outage), Phase 2 aborts — depth idle for the day. Mitigation: CRITICAL Telegram.
- **Snapshot schema change:** existing on-disk snapshot format must be extended. Old snapshots become unreadable — one-time skip on first deploy (documented in PROMPT C comment).
