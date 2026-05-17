# Implementation Plan: Phase 0 Items 8 + 9 — Gap-Fill Scheduler + DEDUP UPSERT + Audit Wiring

**Status:** APPROVED + REVISED (post 3-agent adversarial review)
**Date:** 2026-05-17
**Approved by:** Parthiban (verbatim: "go ahead dude" 2026-05-17 ~05:20 IST)
**Branch:** `claude/phase-0-item-8-9-gap-fill-scheduler`
**Plan source:** `.claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md` §3 (Disconnect gap-fill) + §7 (Seal-then-fetch invariants)

## Summary

Wire the gap-fill scheduler task that consumes the existing pure-function planner (`gap_fill_planner.rs`) and existing audit-table writer (`gap_fill_audit_persistence.rs`). On WebSocket disconnect: compute missing 1m bars → wait `bar_end + 5s` → fetch from Dhan `/v2/charts/intraday` → DEDUP UPSERT into `candles_1m` (source=`gap_fill_backfill`) → write `gap_fill_audit` row → update RAM bar cache → emit typed Telegram event.

## Plan Items

- [ ] **Item 1** — Add 6 constants to `crates/common/src/constants.rs`
  - **REVISED**: 6 of 7 base constants already exist. Buffer stays 5s (changing breaks Phase 0 LOCKED §7 doc + 6 existing tests). Agent-10 concern addressed via enhanced doc comment citing BOOT-03 relationship.
  - EXISTING: `GAP_FILL_POST_SEAL_BUFFER_SECS=5`, `GAP_FILL_FETCH_TIMEOUT_SECS=30`, `GAP_FILL_MAX_CONCURRENT_FETCHES=5`, `GAP_FILL_RETRY_ATTEMPTS=3`, `GAP_FILL_RETRY_BACKOFF_SECS=[2,5,10]`, `GAP_FILL_ONE_MINUTE_SECS=60`
  - NEW: `GAP_FILL_DH904_BACKOFF_SECS: &[u64] = &[10, 20, 40, 80]` (rate-limit specific per `compute_dh904_backoff`)
  - NEW: `GAP_FILL_SCHEDULER_HEARTBEAT_SECS: u64 = 60` (positive liveness signal per Rule 11)
  - DOC: enhance `GAP_FILL_POST_SEAL_BUFFER_SECS` doc with explicit BOOT-03 ±2s envelope mention
  - Tests: 2 new pinned + 1 ratchet verifying DH-904 ladder matches `api-introduction.md` rule 8

- [ ] **Item 2** — Add 4 ErrorCode variants to `crates/common/src/error_code.rs`
  - `GapFill01SchedulerFailed` — Severity::Critical (scheduler task panicked / supervisor caught)
  - `GapFill02RestFetchFailed` — Severity::High (REST call failed after all retries)
  - `GapFill03UpsertFailed` — Severity::High (QuestDB UPSERT failed after audit row wrote)
  - `GapFill04EventChannelLagged` — Severity::Critical (broadcast Lagged → disconnects dropped)
  - Tests: tag-guard test ensures all 4 carry `code = ErrorCode::X.code_str()` field

- [ ] **Item 3** — Add typed Telegram event variants to `crates/core/src/notification/events.rs`
  - **REVISED post-agent-4**: dropped `GapFillStarted` (pager noise per Rule 4 edge-trigger)
  - `GapFillCompleted { bar_minute_ist, sids_completed, sids_failed, duration_ms }` — Severity::Info (one-shot per bar, end-of-event)
  - `GapFillPartial { bar_minute_ist, sids_completed, sids_failed, sample_failed_sids (capped at 5) }` — Severity::High
  - `GapFillFailed { bar_minute_ist, error, attempt }` — Severity::Critical
  - `GapFillEventChannelLagged { dropped_event_count }` — Severity::Critical (post-agent-3 broadcast Lagged handler)
  - Tests: format-string ratchets; Telegram body MUST cap `sample_failed_sids.len() <= 5` per security agent Low #3

- [ ] **Item 4** — Add `upsert_gap_fill_bar()` to `crates/storage/src/candle_persistence.rs`
  - Accepts `Candle1m` + `source: &str` ("gap_fill_backfill"), DEDUP UPSERT on `candles_1m`
  - Must populate `source` SYMBOL column per `topic-PHASE-0-LEAN-LOCKED.md` §6-table contract
  - Tests: `test_upsert_gap_fill_bar_sets_source_column`, `test_upsert_idempotent_on_same_key`

- [ ] **Item 5** — NEW file `crates/core/src/historical/gap_fill_scheduler.rs` (~450 LoC, was ~350)
  - **REVISED post-agent**: full mechanism overhaul
  - Add NEW `tokio::sync::broadcast::Sender<DisconnectResolvedEvent>` to be wired from `connection.rs` reconnect sites (Item 5b)
  - `spawn_gap_fill_scheduler(disconnect_rx, candle_fetcher, candle_writer, audit_writer, notif, bar_cache, cancel_token)`
  - Wrapped in `JoinSet` supervisor (per-agent-9) — re-spawn on panic + emit `GapFill01SchedulerFailed`
  - Heartbeat tokio task increments `tv_gap_fill_scheduler_heartbeat_total` every `GAP_FILL_SCHEDULER_HEARTBEAT_SECS=60`
  - Broadcast `Recv` handles `Err(Lagged(n))` explicitly → emit `GapFill04EventChannelLagged` + run reconciliation
  - On `DisconnectResolvedEvent`: compute outage window (use exchange_ts NOT local time per audit Rule 3) → call `plan_gap_fill_bars()` → spawn per-bar tasks via `JoinSet`
  - Each per-bar task: `tokio::time::sleep_until(bar_end + 10s)` → semaphore `acquire_owned().await` → fetch via REST → UPSERT → write audit row(s) → bar_cache `papaya::insert()`
  - Audit row pattern: write `result=started`+`attempt=N` BEFORE UPSERT; write `result=success|failed`+`attempt=N` AFTER. `attempt` in DEDUP key so each retry leaves a row (per-agent-5)
  - Retry policy:
    - DH-904 detected → use `compute_dh904_backoff(attempts)` ladder `[10s, 20s, 40s, 80s]` (per-agent-6, `api-introduction.md` rule 8)
    - Generic 5xx → use `GAP_FILL_RETRY_BACKOFF_SECS` `[2s, 5s, 10s]`
    - DH-905/906 → NEVER retry
  - All per-bar tasks accept `CancellationToken`; new disconnect cancels stale in-flight tasks (per-agent-hot-path)
  - Market-close cutoff via planner's `market_close_secs`
  - Market-open lower bound: caller-side check before invoking planner (per-agent-11)
  - Tests inline: market-hours gating, retry exhaustion, semaphore bound, cancellation safety, Lagged handling, heartbeat increment, DH-904 vs 5xx distinct backoff

- [ ] **Item 5b** — NEW disconnect event broadcast wiring
  - Add `tokio::sync::broadcast::Sender<DisconnectResolvedEvent>` to a shared boot-time `Arc<DisconnectEventBus>` struct
  - Wire `Sender::send()` at the 4 disconnect-resolved sites in `connection.rs` (find via grep — known sites mentioned by hot-path agent: lines 669/690/724/755)
  - Capacity = 64 (small buffer; Lagged is handled)
  - Test: source-scan ratchet `test_disconnect_event_broadcast_wired_at_all_reconnect_sites`

- [ ] **Item 6** — Wire `pub mod gap_fill_scheduler;` in `crates/core/src/historical/mod.rs`
  - Plus re-export `spawn_gap_fill_scheduler` if needed by main.rs
  - Test: source-scan ratchet that the module is wired

- [ ] **Item 7** — Boot wiring in `crates/app/src/main.rs` after WS pool ready
  - Spawn task with disconnect-event subscription (broadcast channel)
  - Pass shared `Arc<CandleFetcher>` + `Arc<CandleWriter>` + `Arc<GapFillAuditWriter>` + `Arc<NotificationService>` + `Arc<BarCache>` (use existing handles)
  - Test: `pub-fn-wiring-guard` confirms `spawn_gap_fill_scheduler` has call site

- [ ] **Item 8** — Add 3 Prometheus counters in `crates/app/src/observability.rs`
  - `tv_gap_fill_bars_planned_total{trigger}`
  - `tv_gap_fill_bars_completed_total{result=success|partial|failed}`
  - `tv_gap_fill_rest_latency_ms` (histogram)
  - Tests: `operator_health_dashboard_guard.rs` + `resilience_sla_alert_guard.rs` will fail unless I also add panel + alert in next item

- [ ] **Item 9** — Add Grafana panel + Prometheus alert for the 3 counters
  - Panel in `deploy/docker/grafana/dashboards/operator-health.json`
  - Alert in `deploy/docker/grafana/provisioning/alerting/alerts.yml`:
    - `tv-gap-fill-failure-rate` — `rate(tv_gap_fill_bars_completed_total{result="failed"}[5m]) > 0.1` for 60s, severity High
  - Tests: dashboard JSON parses, alert YAML parses

- [ ] **Item 10** — Append GAP-FILL-01/02/03 runbook sections to `.claude/rules/project/wave-1-error-codes.md`
  - Each code: Trigger / App behaviour / Severity / Triage / Auto-triage-safe / Source
  - Required by `error_code_rule_file_crossref.rs` test

- [ ] **Item 11** — NEW integration test file `crates/core/tests/gap_fill_scheduler_integration.rs` (~250 LoC)
  - 8 ratchet tests:
    1. `test_scheduler_plans_bars_only_within_outage_window`
    2. `test_scheduler_respects_market_close_cutoff_15_30`
    3. `test_scheduler_retries_3x_then_emits_critical`
    4. `test_scheduler_concurrent_fetches_bounded_by_semaphore`
    5. `test_scheduler_writes_audit_row_per_bar_attempt`
    6. `test_scheduler_upserts_with_source_gap_fill_backfill`
    7. `test_scheduler_emits_started_then_completed_for_success_path`
    8. `test_scheduler_does_not_write_to_ticks_table` (live-feed-purity guard)

## Z+ 7-layer defense

| Layer | Mechanism | Where |
|---|---|---|
| L1 DETECT | `WebSocketDisconnected` event subscription | `gap_fill_scheduler::spawn_*` |
| L2 VERIFY | `is_bar_eligible_for_gap_fill()` pre-check per bar | existing `constants.rs` helper |
| L3 RECONCILE | Post-market `cross_verify.rs` catches what gap-fill missed | existing module unchanged |
| L4 PREVENT | Market-close cutoff via planner `market_close_secs` param; `+5s` seal buffer | `gap_fill_planner.rs` + scheduler |
| L5 AUDIT | `gap_fill_audit` row per attempt; 3 Prom counters; tracing span | existing audit writer + new counters |
| L6 RECOVER | 3-retry `[2s, 5s, 10s]` backoff; Critical Telegram on final failure | scheduler retry loop |
| L7 COOLDOWN | `Semaphore::new(5)` caps concurrent fetches during long outage | scheduler |

## 15-row "100% everything" matrix

| Demand | Mechanical proof |
|---|---|
| 100% code coverage | New code covered by 8 integration tests + inline unit tests in scheduler module |
| 100% audit coverage | `gap_fill_audit` row written per bar attempt (audit writer already exists) |
| 100% testing coverage | Unit (constants/events), integration (scheduler end-to-end), source-scan (live-feed-purity guard), tag-guard (ErrorCode `code=` field) |
| 100% code checks | Banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify pre-push gates |
| 100% code performance | Cold path (per disconnect, not per tick); semaphore bound prevents stampede; no DHAT needed |
| 100% monitoring | 3 Prom counters + Grafana panel + alert rule |
| 100% logging | tracing macros; ERROR sites carry `code = ErrorCode::X.code_str()` per tag-guard |
| 100% alerting | Prom alert + 4 Telegram event variants routed via NotificationService |
| 100% security | No new auth surface; uses existing JWT via `candle_fetcher`; `Secret<String>` unchanged |
| 100% security hardening | No banned-pattern violations; `unused_must_use` lint on all writes |
| 100% bug fixing | 3-agent adversarial review BEFORE impl (this commit) + AFTER on diff |
| 100% scenarios covering | 8 integration tests cover: success, partial, retry exhaustion, market-close cutoff, ticks-write ban |
| 100% functionalities covering | Every new pub fn has matching test (pub-fn-test-guard) + call site (pub-fn-wiring-guard) |
| 100% code review | 3-agent before impl + 3-agent on diff |
| 100% extreme check | Ratchet tests fail build on regression (test count cannot decrease per existing ratchet) |

## 7-row "Resilience" matrix

| Demand | Honest envelope | Proof in this PR |
|---|---|---|
| Zero ticks lost | Gap-fill DOES NOT touch `ticks` table — protected by `live_feed_purity_guard.rs`. Missing 1m bars are reconstructed in `candles_1m` only. | Test 8 (`test_scheduler_does_not_write_to_ticks_table`) |
| WS never disconnects | N/A — this PR REACTS to disconnects, does not prevent them | existing WS sleep/wake handles connection lifecycle |
| Never slow/locked/hanged | Cold path (per-disconnect, not per-tick); semaphore caps concurrent fetches; per-bar timeout 30s | constants + semaphore design |
| QuestDB never fails | UPSERT uses existing `candle_persistence` writer which absorbs via rescue→spill→DLQ | inherits existing infrastructure |
| O(1) latency | N/A — cold path, no hot-path latency claim | — |
| Uniqueness + dedup | `candles_1m` DEDUP key already includes segment (`(ts, security_id, exchange_segment)`) | inherits existing DEDUP |
| Real-time proof | 3 Prom counters + 4 Telegram variants + audit row per attempt + tracing span | 7-layer telemetry filled |

## Honest 100% claim (PR body wording)

> "100% inside the tested envelope: outages ≤ 5 minutes during [09:15, 15:30) IST absorbed via REST gap-fill (per-bar `bar_end + 5s` buffer); bars older than market-close excluded by planner; concurrent fetches capped at 5 (`GAP_FILL_MAX_CONCURRENT_FETCHES`); 3-retry policy with `[2s, 5s, 10s]` backoff (`GAP_FILL_RETRY_BACKOFF_SECS`). Beyond the envelope (outage > MAX_BARS_PER_PLAN=1440 bars = 24h), historical_fetch cold path is the correct tool; gap-fill skips and emits `GapFillFailed` Critical. Ratcheted by `gap_fill_scheduler_integration.rs` (8 tests) + existing `gap_fill_planner::tests` (planner proptest)."

## Adversarial 3-agent review (pre-impl, this turn)

- [ ] hot-path-reviewer — verify NO hot-path allocations sneak in (scheduler is cold path)
- [ ] security-reviewer — verify no secret exposure in tracing labels; no ILP injection via bar payload
- [ ] general-purpose (hostile) — race conditions on disconnect-event subscription; market-hours gate; edge-trigger discipline; false-OK avoidance

## Scenarios (REVISED post-agent-2 to match actual planner contract)

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Disconnect 09:33:03 → reconnect 09:34:00 (sub-minute, in-flight bar only) | Plan: `[]`. Live aggregator captured partial 09:33 bar from pre-disconnect ticks. NO gap-fill, NO Telegram. Operator-visible only via tracing DEBUG. |
| 2 | Disconnect 09:33:00 → reconnect 09:36:00 (aligned boundaries) | Plan: `[09:33, 09:34, 09:35]`. Three per-bar tasks at 09:34:10, 09:35:10, 09:36:10. |
| 3 | Disconnect 15:29:00 → reconnect 15:31:00 | Plan: `[15:29]` only (15:30 excluded by planner). Fetch at 15:30:10. |
| 4 | REST returns 5xx for all 3 retries | `GapFillFailed` Critical fires; 4 audit rows (started+1, started+2, started+3, failed+3) |
| 5 | REST returns DH-904 (rate limit) | Backoff ladder `[10, 20, 40, 80]` per `compute_dh904_backoff`; distinct from 5xx |
| 6 | Concurrent disconnects spike 100 bars | Semaphore caps at 5; remaining queue. `CancellationToken` cancels stale on new disconnect. |
| 7 | Disconnect 09:14:00 → reconnect 09:14:30 (sub-minute, before market open) | Plan: `[]` (no full minute) + caller-side market-open gate rejects pre-09:15 outage. |
| 8 | Outage > 24h | Plan returns first 1440 bars; gap-fill skips remainder + Critical Telegram. `historical_fetch` cold path is correct tool. |
| 9 | App restarts mid-gap-fill | DEDUP UPSERT idempotent for replayed bars; UNFILLED bars LOST until post-market cross-verify catches them (~17:00 IST). **Known gap; documented L3 safety net.** |
| 10 | 2nd disconnect arrives during in-flight gap-fill | `CancellationToken` cancels stale tasks; new disconnect's plan executes fresh. |
| 11 | Broadcast channel Lagged | `GapFillEventChannelLagged` Critical fires + reconciliation runs `SELECT count(*) FROM candles_1m WHERE ts BETWEEN x AND y` |
| 12 | Scheduler task panics | `JoinSet` supervisor catches + re-spawns + emits `GapFill01SchedulerFailed`. Heartbeat counter resumes. |
| 13 | Clock skew +1.9s (within BOOT-03 envelope) | 10s buffer absorbs; fetch still gets fully-cooked bar. |
| 14 | Live aggregator vs gap-fill race | Planner excludes in-flight bars → no collision in normal case. If aggregator sealed empty bar (zero ticks) it would be a bug in aggregator (separate fix). |

## Verification before PR

- [ ] `cargo check -p tickvault-core -p tickvault-common -p tickvault-storage -p tickvault-app`
- [ ] `cargo test -p tickvault-core` (scheduler module + integration test)
- [ ] `bash .claude/hooks/banned-pattern-scanner.sh`
- [ ] `bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all`
- [ ] `bash .claude/hooks/pub-fn-wiring-guard.sh "$PWD"`
- [ ] `bash .claude/hooks/plan-verify.sh`
- [ ] `python3 -c "import yaml; yaml.safe_load(open('deploy/docker/grafana/provisioning/alerting/alerts.yml'))"`
- [ ] `python3 -c "import json; json.load(open('deploy/docker/grafana/dashboards/operator-health.json'))"`

## After PR opens

- [ ] 3-agent adversarial review on diff
- [ ] Enable auto-merge (squash)
- [ ] Subscribe to PR activity
- [ ] Wait for CI green + merge
- [ ] ONLY THEN start next item (Item 10 or Item 11)
